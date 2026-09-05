use anyhow::Result;
use chrono::Utc;
use fiber_core::{DueIndex, Store, has_schedule, next_due_from_triggers, schedule_trigger_label};
use fiber_proto::{RunEvent, ServerMessage, StepStatus};
use redis::AsyncCommands;
use redis::aio::ConnectionManager;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{RwLock, broadcast, mpsc};
use tracing::{debug, info, warn};
use uuid::Uuid;

const QUEUE_KEY: &str = "fiber:ready_steps";
const EVENTS_CHANNEL: &str = "fiber:events";
const LEASE_SECS: i64 = 300;

#[derive(Clone)]
pub struct Scheduler {
    store: Store,
    redis: ConnectionManager,
    /// agent_id -> labels / concurrency
    agents: Arc<RwLock<HashMap<Uuid, AgentPresence>>>,
    /// agent_id -> outbound WS sender
    connections: Arc<RwLock<HashMap<Uuid, mpsc::UnboundedSender<ServerMessage>>>>,
    events: broadcast::Sender<String>,
    /// Earliest schedule due per pipeline (memoturn DueIndex pattern).
    schedule_due: Arc<DueIndex<Uuid>>,
    /// Distinguishes this process so Redis echo is not double-delivered locally.
    instance_id: Uuid,
}

#[derive(Debug, Clone)]
pub struct AgentPresence {
    pub labels: Vec<String>,
    pub concurrency: u32,
    pub inflight: u32,
    /// `None` = global pool.
    pub project_id: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadyOffer {
    pub step_run_id: Uuid,
    pub labels: Vec<String>,
}

impl Scheduler {
    pub fn new(store: Store, redis: ConnectionManager) -> Self {
        let (events, _) = broadcast::channel(1024);
        Self {
            store,
            redis,
            agents: Arc::new(RwLock::new(HashMap::new())),
            connections: Arc::new(RwLock::new(HashMap::new())),
            events,
            schedule_due: Arc::new(DueIndex::new()),
            instance_id: Uuid::new_v4(),
        }
    }

    /// Seed in-memory schedule due-index from Postgres (`next_due_at`).
    pub async fn seed_schedule_due(&self) -> Result<()> {
        self.schedule_due.clear();
        let pipelines = self.store.list_all_pipelines().await?;
        for p in pipelines {
            if let Some(due) = p.next_due_at {
                self.schedule_due.record(p.id, due);
            }
        }
        Ok(())
    }

    pub fn subscribe(&self) -> broadcast::Receiver<String> {
        self.events.subscribe()
    }

    /// Liveness probe for Redis (used by `/ready`).
    pub async fn redis_ping(&self) -> Result<()> {
        let mut redis = self.redis.clone();
        let _: String = redis::cmd("PING").query_async(&mut redis).await?;
        Ok(())
    }

    pub async fn publish_event(&self, payload: &str) {
        // Local subscribers get the event immediately.
        let _ = self.events.send(payload.to_string());
        // Other API instances receive via Redis; envelope skips echo on this node.
        let envelope = format!("{}|{}", self.instance_id, payload);
        let mut redis = self.redis.clone();
        if let Err(e) = redis.publish::<_, _, ()>(EVENTS_CHANNEL, envelope).await {
            warn!(error = %e, "redis publish fiber:events failed");
        }
    }

    /// Subscribe to Redis `fiber:events` and forward into the local broadcast bus.
    /// Must use a dedicated connection (pubsub holds the link).
    pub async fn events_loop(self: Arc<Self>, redis_url: String) {
        loop {
            if let Err(e) = self.events_loop_once(&redis_url).await {
                warn!(error = %e, "redis events subscriber ended; reconnecting");
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
    }

    async fn events_loop_once(&self, redis_url: &str) -> Result<()> {
        use futures_util::StreamExt;
        let client = redis::Client::open(redis_url)?;
        let mut pubsub = client.get_async_pubsub().await?;
        pubsub.subscribe(EVENTS_CHANNEL).await?;
        info!(channel = EVENTS_CHANNEL, "subscribed to redis run events");
        let mut stream = pubsub.on_message();
        while let Some(msg) = stream.next().await {
            let envelope: String = msg.get_payload().unwrap_or_default();
            let Some((origin, payload)) = envelope.split_once('|') else {
                continue;
            };
            if origin == self.instance_id.to_string() {
                continue;
            }
            if payload.is_empty() {
                continue;
            }
            let _ = self.events.send(payload.to_string());
        }
        Ok(())
    }

    pub async fn register_agent(
        &self,
        agent_id: Uuid,
        labels: Vec<String>,
        concurrency: u32,
        project_id: Option<Uuid>,
    ) {
        let mut agents = self.agents.write().await;
        agents.insert(
            agent_id,
            AgentPresence {
                labels,
                concurrency,
                inflight: 0,
                project_id,
            },
        );
    }

    /// Update labels/concurrency without resetting inflight (for live agents).
    pub async fn update_agent_presence(
        &self,
        agent_id: Uuid,
        labels: Vec<String>,
        concurrency: u32,
    ) {
        let mut agents = self.agents.write().await;
        if let Some(a) = agents.get_mut(&agent_id) {
            a.labels = labels;
            a.concurrency = concurrency.max(1);
        }
    }

    pub async fn register_connection(
        &self,
        agent_id: Uuid,
        tx: mpsc::UnboundedSender<ServerMessage>,
    ) {
        let mut conns = self.connections.write().await;
        conns.insert(agent_id, tx);
    }

    pub async fn unregister_connection(&self, agent_id: Uuid) {
        let mut conns = self.connections.write().await;
        conns.remove(&agent_id);
    }

    pub async fn has_connection(&self, agent_id: Uuid) -> bool {
        let conns = self.connections.read().await;
        conns.contains_key(&agent_id)
    }

    /// Notify the agent and drop its outbound channel (writer exits; reads should stop).
    pub async fn force_disconnect_agent(&self, agent_id: Uuid, reason: &str) {
        self.send_to_agent(
            agent_id,
            ServerMessage::Error {
                message: reason.to_string(),
            },
        )
        .await;
        let _ = self.on_agent_disconnect(agent_id).await;
    }

    pub async fn send_to_agent(&self, agent_id: Uuid, msg: ServerMessage) {
        let conns = self.connections.read().await;
        if let Some(tx) = conns.get(&agent_id) {
            let _ = tx.send(msg);
        }
    }

    pub async fn unregister_agent(&self, agent_id: Uuid) {
        {
            let mut agents = self.agents.write().await;
            agents.remove(&agent_id);
        }
        self.unregister_connection(agent_id).await;
    }

    /// Requeue steps orphaned when an agent disconnects mid-run.
    pub async fn on_agent_disconnect(&self, agent_id: Uuid) -> Result<()> {
        let requeued = self.store.requeue_agent_steps(agent_id).await?;
        if !requeued.is_empty() {
            info!(%agent_id, count = requeued.len(), "requeued steps after agent disconnect");
        }
        for s in requeued {
            self.enqueue_step(s.id, s.labels_vec()).await?;
            let ev = RunEvent::StepUpdated {
                run_id: s.run_id,
                step_run_id: s.id,
                step_id: s.step_id.clone(),
                status: StepStatus::Queued,
            };
            if let Ok(payload) = serde_json::to_string(&ev) {
                self.publish_event(&payload).await;
            }
        }
        self.unregister_agent(agent_id).await;
        Ok(())
    }

    #[tracing::instrument(skip(self), level = "debug")]
    pub async fn enqueue_run_ready(&self, run_id: Uuid) -> Result<()> {
        let steps = self.store.list_step_runs(run_id).await?;
        for s in steps {
            if s.status_enum() == StepStatus::Queued {
                self.enqueue_step(s.id, s.labels_vec()).await?;
            }
        }
        Ok(())
    }

    pub async fn enqueue_step(&self, step_run_id: Uuid, labels: Vec<String>) -> Result<()> {
        let offer = ReadyOffer {
            step_run_id,
            labels,
        };
        let payload = serde_json::to_string(&offer)?;
        let mut redis = self.redis.clone();
        let _: () = redis.rpush(QUEUE_KEY, payload).await?;
        debug!(%step_run_id, "enqueued step");
        Ok(())
    }

    /// Cancel run in DB, notify agents to kill in-flight steps, publish events.
    pub async fn cancel_run(&self, run_id: Uuid) -> Result<fiber_core::Run> {
        let (run, running) = self.store.cancel_run(run_id).await?;

        for s in &running {
            if let Some(aid) = s.agent_id {
                {
                    let mut agents = self.agents.write().await;
                    if let Some(a) = agents.get_mut(&aid) {
                        a.inflight = a.inflight.saturating_sub(1);
                    }
                }
                self.send_to_agent(aid, ServerMessage::Cancel { step_run_id: s.id })
                    .await;
            }
            let ev = RunEvent::StepUpdated {
                run_id: s.run_id,
                step_run_id: s.id,
                step_id: s.step_id.clone(),
                status: StepStatus::Cancelled,
            };
            if let Ok(payload) = serde_json::to_string(&ev) {
                self.publish_event(&payload).await;
            }
        }

        // Also emit updates for pending/queued that were cancelled
        if let Ok(steps) = self.store.list_step_runs(run_id).await {
            for s in steps {
                if s.status_enum() == StepStatus::Cancelled {
                    let ev = RunEvent::StepUpdated {
                        run_id: s.run_id,
                        step_run_id: s.id,
                        step_id: s.step_id.clone(),
                        status: StepStatus::Cancelled,
                    };
                    if let Ok(payload) = serde_json::to_string(&ev) {
                        self.publish_event(&payload).await;
                    }
                }
            }
        }

        let ev = RunEvent::RunUpdated {
            run_id: run.id,
            status: run.status_enum(),
        };
        if let Ok(payload) = serde_json::to_string(&ev) {
            self.publish_event(&payload).await;
        }

        Ok(run)
    }

    /// Find a queued step matching agent labels + project pool and lease it.
    pub async fn offer_for_agent(&self, agent_id: Uuid) -> Result<Option<fiber_core::StepRun>> {
        {
            let agents = self.agents.read().await;
            if let Some(a) = agents.get(&agent_id) {
                if a.inflight >= a.concurrency {
                    return Ok(None);
                }
            }
        }

        let (agent_labels, agent_project_id) = {
            let agents = self.agents.read().await;
            match agents.get(&agent_id) {
                Some(a) => (a.labels.clone(), a.project_id),
                None => (Vec::new(), None),
            }
        };

        let queued = self
            .store
            .list_queued_steps_for_pool(agent_project_id)
            .await?;
        for step in queued {
            let needed = step.labels_vec();
            if labels_match(&agent_labels, &needed) {
                if let Some(leased) = self.store.lease_step(step.id, agent_id, LEASE_SECS).await? {
                    let mut agents = self.agents.write().await;
                    if let Some(a) = agents.get_mut(&agent_id) {
                        a.inflight += 1;
                    }
                    info!(%agent_id, step = %leased.step_id, "leased step");
                    let _ = self.drain_redis_step(leased.id).await;
                    return Ok(Some(leased));
                }
            }
        }
        Ok(None)
    }

    async fn drain_redis_step(&self, step_run_id: Uuid) -> Result<()> {
        let mut redis = self.redis.clone();
        let len: isize = redis.llen(QUEUE_KEY).await.unwrap_or(0);
        for _ in 0..len {
            let item: Option<String> = redis.lpop(QUEUE_KEY, None).await?;
            if let Some(s) = item {
                if let Ok(offer) = serde_json::from_str::<ReadyOffer>(&s) {
                    if offer.step_run_id != step_run_id {
                        let _: () = redis.rpush(QUEUE_KEY, s).await?;
                    }
                }
            }
        }
        Ok(())
    }

    pub async fn renew_leases(&self, agent_id: Uuid) -> Result<()> {
        let n = self.store.renew_agent_leases(agent_id, LEASE_SECS).await?;
        if n > 0 {
            debug!(%agent_id, n, "renewed leases");
        }
        Ok(())
    }

    pub async fn on_step_complete(
        &self,
        agent_id: Uuid,
        step_run_id: Uuid,
        status: StepStatus,
        exit_code: Option<i32>,
        error: Option<String>,
    ) -> Result<Option<fiber_core::StepRun>> {
        {
            let mut agents = self.agents.write().await;
            if let Some(a) = agents.get_mut(&agent_id) {
                a.inflight = a.inflight.saturating_sub(1);
            }
        }

        let current = match self.store.get_step_run(step_run_id).await? {
            Some(s) => s,
            None => return Ok(None),
        };

        // Ignore late completes after cancel / reclaim / re-lease to another agent.
        if current.status_enum() != StepStatus::Running || current.agent_id != Some(agent_id) {
            debug!(
                %step_run_id,
                status = %current.status,
                "ignoring late step complete"
            );
            return Ok(None);
        }

        if status == StepStatus::Failed && current.attempt <= current.retries {
            let backoff_secs = 2u64.pow(current.attempt.max(1) as u32).min(60);
            info!(
                step = %current.step_id,
                attempt = current.attempt,
                retries = current.retries,
                backoff_secs,
                "retrying failed step"
            );
            let retried = self.store.requeue_for_retry(step_run_id).await?;
            let ev = RunEvent::StepUpdated {
                run_id: retried.run_id,
                step_run_id: retried.id,
                step_id: retried.step_id.clone(),
                status: StepStatus::Queued,
            };
            if let Ok(payload) = serde_json::to_string(&ev) {
                self.publish_event(&payload).await;
            }
            let scheduler = self.clone();
            let labels = retried.labels_vec();
            let sid = retried.id;
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)).await;
                let _ = scheduler.enqueue_step(sid, labels).await;
            });
            return Ok(Some(retried));
        }

        let Some(completed) = self
            .store
            .complete_running_step(step_run_id, agent_id, status, exit_code, error)
            .await?
        else {
            return Ok(None);
        };

        let changed = self.store.propagate_after_step(completed.run_id).await?;
        for s in &changed {
            if s.status_enum() == StepStatus::Queued {
                self.enqueue_step(s.id, s.labels_vec()).await?;
            }
            let ev = RunEvent::StepUpdated {
                run_id: s.run_id,
                step_run_id: s.id,
                step_id: s.step_id.clone(),
                status: s.status_enum(),
            };
            if let Ok(payload) = serde_json::to_string(&ev) {
                self.publish_event(&payload).await;
            }
        }

        let ev = RunEvent::StepUpdated {
            run_id: completed.run_id,
            step_run_id: completed.id,
            step_id: completed.step_id.clone(),
            status: completed.status_enum(),
        };
        if let Ok(payload) = serde_json::to_string(&ev) {
            self.publish_event(&payload).await;
        }

        if let Some(run) = self.store.get_run(completed.run_id).await? {
            let ev = RunEvent::RunUpdated {
                run_id: run.id,
                status: run.status_enum(),
            };
            if let Ok(payload) = serde_json::to_string(&ev) {
                self.publish_event(&payload).await;
            }
        }

        Ok(Some(completed))
    }

    pub async fn schedule_loop(self: Arc<Self>) {
        if let Err(e) = self.seed_schedule_due().await {
            warn!(error = %e, "schedule due-index seed failed");
        }
        loop {
            if let Err(e) = self.tick_schedules().await {
                warn!(error = %e, "schedule tick failed");
            }
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        }
    }

    #[tracing::instrument(skip(self), level = "info")]
    async fn tick_schedules(&self) -> Result<()> {
        let now = chrono::Utc::now();
        // Fast path: in-memory index says nothing is due yet.
        if let Some(earliest) = self.schedule_due.earliest() {
            if earliest > now {
                debug!(%earliest, "no pipeline schedules due yet");
                return Ok(());
            }
        }

        // DB is source of truth (index may over-report or be empty before seed/create).
        let pipelines = self.store.list_due_pipelines(now).await?;
        for p in pipelines {
            let Ok(def) = fiber_core::store::value_to_definition(&p.definition) else {
                continue;
            };
            let Some(on) = &def.on else {
                let _ = self.store.clear_schedule_due(p.id).await;
                self.schedule_due.set(p.id, None);
                continue;
            };
            if !has_schedule(on) {
                let _ = self.store.clear_schedule_due(p.id).await;
                self.schedule_due.set(p.id, None);
                continue;
            }
            let recent = self.store.list_runs(p.project_id, 20).await?;
            let active = recent.iter().any(|r| {
                r.pipeline_id == p.id && matches!(r.status.as_str(), "pending" | "running")
            });
            if active {
                continue;
            }
            let trigger = schedule_trigger_label(on);
            info!(pipeline = %p.id, %trigger, "scheduled run");
            let (run, _, _) = self.store.start_run(p.id, &trigger).await?;
            let next = next_due_from_triggers(on, Utc::now());
            self.store.mark_scheduled(p.id, next).await?;
            self.schedule_due.set(p.id, next);
            self.enqueue_run_ready(run.id).await?;
        }
        Ok(())
    }

    pub async fn reclaim_loop(self: Arc<Self>) {
        loop {
            if let Err(e) = self.reclaim_once().await {
                warn!(error = %e, "lease reclaim failed");
            }
            tokio::time::sleep(std::time::Duration::from_secs(15)).await;
        }
    }

    async fn reclaim_once(&self) -> Result<()> {
        let stale_secs: i64 = std::env::var("FIBER_AGENT_STALE_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(45);
        let stale = self.store.mark_stale_agents_offline(stale_secs).await?;
        for agent_id in stale {
            info!(%agent_id, stale_secs, "marked agent offline (stale heartbeat)");
            if let Err(e) = self.on_agent_disconnect(agent_id).await {
                warn!(error = %e, %agent_id, "stale agent disconnect cleanup failed");
            }
        }

        let requeued = self.store.requeue_expired_leases().await?;
        if requeued.is_empty() {
            return Ok(());
        }
        info!(count = requeued.len(), "reclaimed expired leases");
        for s in requeued {
            self.enqueue_step(s.id, s.labels_vec()).await?;
            let ev = RunEvent::StepUpdated {
                run_id: s.run_id,
                step_run_id: s.id,
                step_id: s.step_id.clone(),
                status: StepStatus::Queued,
            };
            if let Ok(payload) = serde_json::to_string(&ev) {
                self.publish_event(&payload).await;
            }
        }
        Ok(())
    }
}

fn labels_match(agent: &[String], required: &[String]) -> bool {
    if required.is_empty() {
        return true;
    }
    required.iter().all(|r| agent.iter().any(|a| a == r))
}
