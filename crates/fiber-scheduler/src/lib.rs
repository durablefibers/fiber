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

const EVENTS_CHANNEL: &str = "fiber:events";
/// Agent-directed messages (cancel, disconnect) fanned out to every API instance, so
/// the one holding the agent's socket delivers them.
const AGENT_CMDS_CHANNEL: &str = "fiber:agent_cmds";
const LEASE_SECS: i64 = 300;
/// Ceiling on the per-attempt retry backoff, so a high `retries` cannot park a step for hours.
const MAX_RETRY_BACKOFF_SECS: u64 = 60;

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

impl AgentPresence {
    /// Take a concurrency slot if one is free. Check and increment are one operation so
    /// two concurrent offers for the same agent cannot both pass the cap.
    pub fn try_reserve_slot(&mut self) -> bool {
        if self.inflight >= self.concurrency {
            return false;
        }
        self.inflight += 1;
        true
    }

    /// Give a slot back. Saturating: a duplicate release must not wrap to u32::MAX and
    /// hand the agent unlimited concurrency.
    pub fn release_slot(&mut self) {
        self.inflight = self.inflight.saturating_sub(1);
    }
}

/// Envelope on `fiber:agent_cmds`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentCommand {
    pub agent_id: Uuid,
    pub kind: AgentCommandKind,
}

/// Deliberately narrow: nothing on this channel can make an agent *run* anything, so a
/// Redis compromise cannot become code execution on build machines.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentCommandKind {
    /// Kill a step the agent is running (run cancel, timeout).
    Cancel { step_run_id: Uuid },
    /// Terminate the agent's session here (token rotated, agent deleted).
    Disconnect { reason: String },
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

    async fn publish_agent_command(&self, cmd: &AgentCommand) {
        let Ok(payload) = serde_json::to_string(cmd) else {
            return;
        };
        let envelope = format!("{}|{}", self.instance_id, payload);
        let mut redis = self.redis.clone();
        if let Err(e) = redis
            .publish::<_, _, ()>(AGENT_CMDS_CHANNEL, envelope)
            .await
        {
            warn!(error = %e, "redis publish fiber:agent_cmds failed");
        }
    }

    /// Subscribe to `fiber:agent_cmds` and deliver commands to agents connected to
    /// this instance. Dedicated connection (pubsub holds the link).
    pub async fn agent_cmds_loop(self: Arc<Self>, redis_url: String) {
        loop {
            if let Err(e) = self.agent_cmds_loop_once(&redis_url).await {
                warn!(error = %e, "redis agent-command subscriber ended; reconnecting");
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
    }

    async fn agent_cmds_loop_once(&self, redis_url: &str) -> Result<()> {
        use futures_util::StreamExt;
        let client = redis::Client::open(redis_url)?;
        let mut pubsub = client.get_async_pubsub().await?;
        pubsub.subscribe(AGENT_CMDS_CHANNEL).await?;
        info!(
            channel = AGENT_CMDS_CHANNEL,
            "subscribed to redis agent commands"
        );
        let mut stream = pubsub.on_message();
        while let Some(msg) = stream.next().await {
            let envelope: String = msg.get_payload().unwrap_or_default();
            let Some((origin, payload)) = envelope.split_once('|') else {
                continue;
            };
            if origin == self.instance_id.to_string() {
                continue;
            }
            let Ok(cmd) = serde_json::from_str::<AgentCommand>(payload) else {
                continue;
            };
            self.apply_agent_command(cmd).await;
        }
        Ok(())
    }

    /// Apply a command for an agent that may be connected to this instance.
    async fn apply_agent_command(&self, cmd: AgentCommand) {
        match cmd.kind {
            AgentCommandKind::Cancel { step_run_id } => {
                self.deliver_cancel(cmd.agent_id, step_run_id, true).await;
            }
            AgentCommandKind::Disconnect { reason } => {
                if !self.has_connection(cmd.agent_id).await {
                    return;
                }
                self.send_local(cmd.agent_id, ServerMessage::Error { message: reason })
                    .await;
                // Requeue is idempotent (the originating instance already did it).
                let _ = self.on_agent_disconnect(cmd.agent_id).await;
            }
        }
    }

    async fn send_local(&self, agent_id: Uuid, msg: ServerMessage) -> bool {
        let conns = self.connections.read().await;
        match conns.get(&agent_id) {
            Some(tx) => tx.send(msg).is_ok(),
            None => false,
        }
    }

    pub async fn register_agent(
        &self,
        agent_id: Uuid,
        labels: Vec<String>,
        concurrency: u32,
        project_id: Option<Uuid>,
    ) {
        let mut agents = self.agents.write().await;
        // A repeated Hello on a live session must not reset the concurrency accounting.
        let inflight = agents.get(&agent_id).map(|a| a.inflight).unwrap_or(0);
        agents.insert(
            agent_id,
            AgentPresence {
                labels,
                concurrency,
                inflight,
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
    /// End an agent's session wherever it is connected: locally now, and on every
    /// other instance via `fiber:agent_cmds`. Its running steps are requeued.
    pub async fn force_disconnect_agent(&self, agent_id: Uuid, reason: &str) {
        self.send_local(
            agent_id,
            ServerMessage::Error {
                message: reason.to_string(),
            },
        )
        .await;
        let _ = self.on_agent_disconnect(agent_id).await;
        self.publish_agent_command(&AgentCommand {
            agent_id,
            kind: AgentCommandKind::Disconnect {
                reason: reason.to_string(),
            },
        })
        .await;
    }

    /// Ask the agent holding `step_run_id` to kill it, wherever it is connected:
    /// delivered here if the socket is local, and always fanned out over Redis (a
    /// local send success only proves the writer task is alive, not that this replica
    /// still holds the agent's live socket). `release_slot` frees the agent's
    /// concurrency slot on the delivering replica — pass `false` when the completion
    /// path already did.
    pub async fn cancel_step_on_agent(
        &self,
        agent_id: Uuid,
        step_run_id: Uuid,
        release_slot: bool,
    ) {
        self.deliver_cancel(agent_id, step_run_id, release_slot)
            .await;
        self.publish_agent_command(&AgentCommand {
            agent_id,
            kind: AgentCommandKind::Cancel { step_run_id },
        })
        .await;
    }

    async fn deliver_cancel(&self, agent_id: Uuid, step_run_id: Uuid, release_slot: bool) {
        if !self
            .send_local(agent_id, ServerMessage::Cancel { step_run_id })
            .await
        {
            return;
        }
        if release_slot {
            let mut agents = self.agents.write().await;
            if let Some(a) = agents.get_mut(&agent_id) {
                a.inflight = a.inflight.saturating_sub(1);
            }
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

    /// Steps are pulled from Postgres by `offer_for_agent` on every heartbeat and
    /// completion, so becoming `queued` is the whole enqueue. Kept as a hook for
    /// logging / a future push-notify; there is deliberately no Redis queue (the old
    /// list was never consumed and was rotated in full on every lease).
    pub async fn enqueue_step(&self, step_run_id: Uuid, labels: Vec<String>) -> Result<()> {
        debug!(%step_run_id, ?labels, "step queued");
        Ok(())
    }

    /// Start a run, then cancel anything it supersedes.
    ///
    /// Every path that starts a run goes through here — the API's manual start, both
    /// GitHub webhook paths, and the schedule loop — because concurrency that four call
    /// sites have to remember is concurrency that one of them will forget.
    pub async fn start_run_for_commit(
        &self,
        pipeline_id: Uuid,
        trigger: &str,
        commit: fiber_core::models::RunCommit,
    ) -> Result<(
        fiber_core::Run,
        Vec<fiber_core::StepRun>,
        fiber_core::dag::CompiledDag,
    )> {
        let started = self
            .store
            .start_run_for_commit(pipeline_id, trigger, commit)
            .await?;
        self.cancel_superseded(&started.0).await;
        Ok(started)
    }

    pub async fn start_run(
        &self,
        pipeline_id: Uuid,
        trigger: &str,
    ) -> Result<(
        fiber_core::Run,
        Vec<fiber_core::StepRun>,
        fiber_core::dag::CompiledDag,
    )> {
        self.start_run_for_commit(pipeline_id, trigger, Default::default())
            .await
    }

    /// Cancel the older unfinished runs of `run`'s concurrency group.
    ///
    /// Best effort by design: a run that finished between the query and the cancel is
    /// already where we want it, and a cancel that fails must not take the new run down
    /// with it — the worst case is one extra build, not a lost one.
    async fn cancel_superseded(&self, run: &fiber_core::Run) {
        let Some(group) = run.concurrency_group.as_deref() else {
            return;
        };
        let older = match self
            .store
            .superseded_runs(run.project_id, group, run.id, run.created_at)
            .await
        {
            Ok(ids) => ids,
            Err(e) => {
                warn!(run_id = %run.id, group, error = %e, "could not look up superseded runs");
                return;
            }
        };
        for id in older {
            match self
                .cancel_run_with_reason(id, Some("superseded by a newer run"))
                .await
            {
                Ok(_) => info!(superseded = %id, by = %run.id, group, "cancelled superseded run"),
                Err(e) => warn!(run_id = %id, error = %e, "could not cancel superseded run"),
            }
        }
    }

    /// Cancel run in DB, notify agents to kill in-flight steps, publish events.
    pub async fn cancel_run(&self, run_id: Uuid) -> Result<fiber_core::Run> {
        self.cancel_run_with_reason(run_id, None).await
    }

    pub async fn cancel_run_with_reason(
        &self,
        run_id: Uuid,
        reason: Option<&str>,
    ) -> Result<fiber_core::Run> {
        let (run, running) = self.store.cancel_run_with_reason(run_id, reason).await?;

        for s in &running {
            if let Some(aid) = s.agent_id {
                // The replica that delivers the Cancel releases the agent's slot.
                self.cancel_step_on_agent(aid, s.id, true).await;
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
        // Reserve a concurrency slot under one write lock (check + increment together),
        // and give it back below if nothing was leased. Two concurrent offers for the
        // same agent can no longer both pass the check.
        // Fail closed without presence: a socket that never sent Hello (or whose
        // presence was reclaimed) gets nothing rather than the global pool.
        let agent_labels = {
            let mut agents = self.agents.write().await;
            match agents.get_mut(&agent_id) {
                Some(a) => {
                    if !a.try_reserve_slot() {
                        return Ok(None);
                    }
                    a.labels.clone()
                }
                None => {
                    debug!(%agent_id, "no presence for agent; not offering");
                    return Ok(None);
                }
            }
        };
        let leased = self.try_lease_for(agent_id, agent_labels).await;
        if !matches!(leased, Ok(Some(_))) {
            let mut agents = self.agents.write().await;
            if let Some(a) = agents.get_mut(&agent_id) {
                a.release_slot();
            }
        }
        leased
    }

    async fn try_lease_for(
        &self,
        agent_id: Uuid,
        agent_labels: Vec<String>,
    ) -> Result<Option<fiber_core::StepRun>> {
        // Pool scope is authoritative from the DB row, never from in-memory state.
        let agent_project_id = match self.store.get_agent(agent_id).await? {
            Some(a) => a.project_id,
            None => return Ok(None),
        };

        let queued = self
            .store
            .list_queued_steps_for_pool(agent_project_id)
            .await?;
        for step in queued {
            let needed = step.labels_vec();
            if labels_match(&agent_labels, &needed) {
                if let Some(leased) = self.store.lease_step(step.id, agent_id, LEASE_SECS).await? {
                    info!(%agent_id, step = %leased.step_id, "leased step");
                    return Ok(Some(leased));
                }
            }
        }
        Ok(None)
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
        let found = self.store.get_step_run(step_run_id).await?;

        // Decide about the slot before anything returns: a step whose rows were deleted
        // under the agent still has to give its slot back.
        if releases_slot(
            found.as_ref().map(|s| (s.status_enum(), s.agent_id)),
            agent_id,
        ) {
            let mut agents = self.agents.write().await;
            if let Some(a) = agents.get_mut(&agent_id) {
                a.release_slot();
            }
        }

        let Some(current) = found else {
            debug!(%step_run_id, "step complete for a row that no longer exists");
            return Ok(None);
        };

        // Ignore late completes after cancel / reclaim / re-lease to another agent.
        if !completion_is_current(current.status_enum(), current.agent_id, agent_id) {
            debug!(
                %step_run_id,
                status = %current.status,
                "ignoring late step complete"
            );
            return Ok(None);
        }

        if let Some(backoff_secs) = retry_plan(status, current.attempt, current.retries) {
            info!(
                step = %current.step_id,
                attempt = current.attempt,
                retries = current.retries,
                backoff_secs,
                "retrying failed step"
            );
            // The backoff lives on the row (`not_before`), so it survives restarts and
            // is honoured by every instance's offers.
            let Some(retried) = self
                .store
                .requeue_for_retry(
                    step_run_id,
                    agent_id,
                    backoff_secs as i64,
                    exit_code,
                    error.as_deref(),
                )
                .await?
            else {
                debug!(%step_run_id, "retry requeue skipped: step no longer running here");
                return Ok(None);
            };
            let ev = RunEvent::StepUpdated {
                run_id: retried.run_id,
                step_run_id: retried.id,
                step_id: retried.step_id.clone(),
                status: StepStatus::Queued,
            };
            if let Ok(payload) = serde_json::to_string(&ev) {
                self.publish_event(&payload).await;
            }
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
        // Always ask Postgres (an indexed scan of `next_due_at`). The in-memory due
        // index is seeded at boot and not updated when pipelines are created or
        // edited through the API — using it as a gate meant schedules created after
        // boot never fired until the next restart.
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
            let Some(observed_due) = p.next_due_at else {
                continue;
            };
            let next = next_due_from_triggers(on, Utc::now());
            // Compare-and-set on next_due_at (plus "no active run"): across several API
            // instances exactly one claims the slot; the rest see it already advanced.
            // While a run is still active the slot stays put and is retried next tick.
            if !self
                .store
                .claim_schedule_slot(p.id, observed_due, next)
                .await?
            {
                debug!(pipeline = %p.id, "schedule slot not claimed (active run or claimed elsewhere)");
                continue;
            }
            self.schedule_due.set(p.id, next);
            let trigger = schedule_trigger_label(on);
            info!(pipeline = %p.id, %trigger, "scheduled run");
            match self.start_run(p.id, &trigger).await {
                Ok((run, _, _)) => self.enqueue_run_ready(run.id).await?,
                // The slot is already advanced; skipping one occurrence beats double-firing.
                Err(e) => warn!(pipeline = %p.id, error = %e, "scheduled run failed to start"),
            }
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
            // Requeues here and drops the socket on whichever replica still holds it.
            self.force_disconnect_agent(agent_id, "stale heartbeat — reconnect")
                .await;
        }

        self.enforce_timeouts().await?;

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

impl Scheduler {
    /// Server-side backstop for step and run timeouts. Agents enforce the step limit
    /// themselves; this catches agents that are old, hung, or gone, after a grace period.
    async fn enforce_timeouts(&self) -> Result<()> {
        let cfg = TimeoutConfig::from_env();
        for t in self
            .store
            .list_timed_out_steps(cfg.default_minutes, cfg.grace_minutes)
            .await?
        {
            let Some(agent_id) = t.agent_id else {
                continue;
            };
            warn!(step = %t.step_id, run = %t.run_id, minutes = t.timeout_minutes, "step timed out (server backstop)");
            let error = format!("timed out after {} min", t.timeout_minutes);
            // Fails the attempt (honouring retries), propagates, publishes — then asks the
            // agent to kill whatever is still running for it.
            self.on_step_complete(
                agent_id,
                t.step_run_id,
                StepStatus::Failed,
                None,
                Some(error),
            )
            .await?;
            // on_step_complete already released the slot for a locally held agent.
            self.cancel_step_on_agent(agent_id, t.step_run_id, false)
                .await;
        }
        for (run_id, minutes) in self.store.list_timed_out_runs().await? {
            warn!(run = %run_id, minutes, "run timed out");
            let reason = format!("run timed out after {minutes} min");
            self.cancel_run_with_reason(run_id, Some(&reason)).await?;
        }
        Ok(())
    }
}

/// Step-timeout defaults; see docs/configuration.md.
#[derive(Debug, Clone, Copy)]
pub struct TimeoutConfig {
    /// Applied to steps that set no `timeout_minutes`.
    pub default_minutes: i64,
    /// Extra minutes the server waits past a step's timeout before failing it itself.
    pub grace_minutes: i64,
}

impl TimeoutConfig {
    pub fn from_env() -> Self {
        let read = |k: &str, d: i64| timeout_minutes_or(std::env::var(k).ok().as_deref(), d);
        Self {
            default_minutes: read("FIBER_STEP_TIMEOUT_DEFAULT_MINUTES", 60),
            grace_minutes: read("FIBER_STEP_TIMEOUT_GRACE_MINUTES", 5),
        }
    }
}

/// Whether a `StepComplete` from `reporting_agent` still applies to the row as stored.
///
/// CI steps are at-least-once: a lease can expire, the step be requeued, and a second
/// agent lease it — all while the first agent is still running and about to report. Its
/// report must be dropped, or it would overwrite the live attempt's outcome. Pure so the
/// cases can be enumerated without a database.
fn completion_is_current(
    current: StepStatus,
    row_agent: Option<Uuid>,
    reporting_agent: Uuid,
) -> bool {
    current == StepStatus::Running && row_agent == Some(reporting_agent)
}

/// Whether a `StepComplete` should give the reporting agent its concurrency slot back.
///
/// `None` means the step row is gone — the run was deleted under a live agent, which
/// happens when a project is deleted while one of its steps is running on a *global*
/// agent that outlives it. The agent really did hold that slot, so it has to come back;
/// leaving it taken permanently shrinks a shared agent's capacity until it reconnects.
///
/// The trade is that an agent could spam completions for ids that never existed and
/// saturate its own counter to zero, taking more work than its `concurrency` allows.
/// That is self-inflicted load on a host the agent already runs arbitrary pipeline shell
/// on, and `release_slot` is saturating so it cannot wrap — whereas the leak is
/// permanent and reachable by any project owner against an agent shared with others.
fn releases_slot(current: Option<(StepStatus, Option<Uuid>)>, reporting_agent: Uuid) -> bool {
    match current {
        None => true,
        Some((status, row_agent)) => completion_is_current(status, row_agent, reporting_agent),
    }
}

/// `Some(backoff_seconds)` when a finished step should be requeued for another attempt.
///
/// Only a failure retries — a cancel is a person's decision and a success is done. The
/// backoff doubles per attempt and is capped, and lands on the row as `not_before` so it
/// survives a restart and is honoured by every instance.
fn retry_plan(status: StepStatus, attempt: i32, retries: i32) -> Option<u64> {
    if status != StepStatus::Failed || attempt > retries {
        return None;
    }
    Some(2u64.pow(attempt.max(1) as u32).min(MAX_RETRY_BACKOFF_SECS))
}

/// Read a positive minute count, falling back to `default` for absent, unparseable, or
/// non-positive values. A zero or negative timeout would fail every step immediately.
fn timeout_minutes_or(raw: Option<&str>, default: i64) -> i64 {
    raw.and_then(|v| v.parse::<i64>().ok())
        .filter(|v| *v >= 1)
        .unwrap_or(default)
}

fn labels_match(agent: &[String], required: &[String]) -> bool {
    if required.is_empty() {
        return true;
    }
    required.iter().all(|r| agent.iter().any(|a| a == r))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn presence(concurrency: u32, inflight: u32) -> AgentPresence {
        AgentPresence {
            labels: vec![],
            concurrency,
            inflight,
            project_id: None,
        }
    }

    // --- concurrency slots ------------------------------------------------------------

    #[test]
    fn an_agent_reserves_up_to_its_concurrency_and_no_further() {
        let mut a = presence(2, 0);
        assert!(a.try_reserve_slot());
        assert!(a.try_reserve_slot());
        assert!(!a.try_reserve_slot(), "the cap must hold");
        assert_eq!(a.inflight, 2, "a refused reservation must not count");
    }

    #[test]
    fn a_zero_concurrency_agent_is_never_offered_work() {
        // An agent that reported concurrency 0 (or was drained) takes nothing.
        let mut a = presence(0, 0);
        assert!(!a.try_reserve_slot());
        assert_eq!(a.inflight, 0);
    }

    #[test]
    fn releasing_a_slot_lets_the_next_offer_through() {
        let mut a = presence(1, 0);
        assert!(a.try_reserve_slot());
        assert!(!a.try_reserve_slot());
        a.release_slot();
        assert!(a.try_reserve_slot(), "the freed slot is reusable");
    }

    #[test]
    fn a_duplicate_release_cannot_wrap_into_unlimited_concurrency() {
        // StepComplete can arrive twice (at-least-once). An unsaturated subtract would
        // wrap u32 to ~4 billion and lift the cap entirely.
        let mut a = presence(1, 0);
        a.release_slot();
        a.release_slot();
        assert_eq!(a.inflight, 0);
        assert!(a.try_reserve_slot());
        assert!(
            !a.try_reserve_slot(),
            "the cap still holds after the wrap attempt"
        );
    }

    // --- late completes ---------------------------------------------------------------

    #[test]
    fn the_leaseholder_completing_a_running_step_is_accepted() {
        let agent = Uuid::new_v4();
        assert!(completion_is_current(
            StepStatus::Running,
            Some(agent),
            agent
        ));
    }

    #[test]
    fn a_report_from_an_agent_that_no_longer_holds_the_step_is_dropped() {
        // The lease expired, the step was requeued, and another agent now owns it. The
        // original agent finishing late must not overwrite the live attempt.
        let old_agent = Uuid::new_v4();
        let new_agent = Uuid::new_v4();
        assert!(!completion_is_current(
            StepStatus::Running,
            Some(new_agent),
            old_agent
        ));
    }

    #[test]
    fn a_report_for_a_step_that_already_finished_is_dropped() {
        let agent = Uuid::new_v4();
        for status in [
            StepStatus::Succeeded,
            StepStatus::Failed,
            StepStatus::Cancelled,
            StepStatus::Skipped,
            StepStatus::Queued,
            StepStatus::Pending,
        ] {
            assert!(
                !completion_is_current(status, Some(agent), agent),
                "{status:?} must not accept a completion"
            );
        }
    }

    #[test]
    fn a_report_for_a_step_with_no_leaseholder_is_dropped() {
        // Reclaim clears agent_id; a report arriving in that window has nothing to close.
        assert!(!completion_is_current(
            StepStatus::Running,
            None,
            Uuid::new_v4()
        ));
    }

    // --- slot release -----------------------------------------------------------------

    #[test]
    fn a_step_whose_rows_were_deleted_still_gives_its_slot_back() {
        // Deleting a project cancels and removes its runs. A step of that project
        // running on a *global* agent — which survives the project — reports into
        // nothing; the agent held the slot and must get it back, or a shared agent
        // silently loses capacity until it reconnects.
        assert!(releases_slot(None, Uuid::new_v4()));
    }

    #[test]
    fn the_leaseholder_finishing_its_own_step_gives_the_slot_back() {
        let agent = Uuid::new_v4();
        assert!(releases_slot(
            Some((StepStatus::Running, Some(agent))),
            agent
        ));
    }

    #[test]
    fn a_late_report_does_not_give_back_a_slot_it_no_longer_holds() {
        // The lease moved to another agent. Releasing here would let the original agent
        // take work beyond its cap while still running the step it was reporting on.
        let old_agent = Uuid::new_v4();
        let new_agent = Uuid::new_v4();
        assert!(!releases_slot(
            Some((StepStatus::Running, Some(new_agent))),
            old_agent
        ));
    }

    #[test]
    fn a_report_for_an_already_finished_step_does_not_release_again() {
        let agent = Uuid::new_v4();
        for status in [
            StepStatus::Succeeded,
            StepStatus::Failed,
            StepStatus::Cancelled,
            StepStatus::Skipped,
        ] {
            assert!(
                !releases_slot(Some((status, Some(agent))), agent),
                "{status:?} already released its slot once"
            );
        }
    }

    // --- retry policy -----------------------------------------------------------------

    #[test]
    fn only_a_failure_retries() {
        for status in [
            StepStatus::Succeeded,
            StepStatus::Cancelled,
            StepStatus::Skipped,
        ] {
            assert_eq!(retry_plan(status, 1, 3), None, "{status:?} must not retry");
        }
        assert!(retry_plan(StepStatus::Failed, 1, 3).is_some());
    }

    #[test]
    fn a_step_with_no_retries_configured_fails_on_its_first_attempt() {
        assert_eq!(retry_plan(StepStatus::Failed, 1, 0), None);
    }

    #[test]
    fn retries_are_spent_one_per_attempt_and_then_stop() {
        // retries = 2 means attempts 1 and 2 requeue; attempt 3 is terminal.
        assert!(retry_plan(StepStatus::Failed, 1, 2).is_some());
        assert!(retry_plan(StepStatus::Failed, 2, 2).is_some());
        assert_eq!(retry_plan(StepStatus::Failed, 3, 2), None);
        assert_eq!(retry_plan(StepStatus::Failed, 99, 2), None);
    }

    #[test]
    fn the_backoff_doubles_per_attempt_and_is_capped() {
        assert_eq!(retry_plan(StepStatus::Failed, 1, 99), Some(2));
        assert_eq!(retry_plan(StepStatus::Failed, 2, 99), Some(4));
        assert_eq!(retry_plan(StepStatus::Failed, 3, 99), Some(8));
        assert_eq!(retry_plan(StepStatus::Failed, 6, 99), Some(60), "capped");
        // The cap also keeps 2^attempt from overflowing on a large retries value.
        assert_eq!(
            retry_plan(StepStatus::Failed, 40, 99),
            Some(MAX_RETRY_BACKOFF_SECS)
        );
    }

    #[test]
    fn a_zeroth_attempt_still_waits_before_retrying() {
        // attempt is 1-based in practice; `.max(1)` guards 2^0 = 1 second, which would be
        // a near-instant hot loop against whatever just failed.
        assert_eq!(retry_plan(StepStatus::Failed, 0, 3), Some(2));
    }

    // --- timeout config ---------------------------------------------------------------

    #[test]
    fn a_timeout_falls_back_when_unset_or_unusable() {
        assert_eq!(timeout_minutes_or(None, 60), 60);
        assert_eq!(timeout_minutes_or(Some(""), 60), 60);
        assert_eq!(timeout_minutes_or(Some("soon"), 60), 60);
        // Zero or negative would time every step out immediately.
        assert_eq!(timeout_minutes_or(Some("0"), 60), 60);
        assert_eq!(timeout_minutes_or(Some("-5"), 60), 60);
    }

    #[test]
    fn a_usable_timeout_is_taken_as_given() {
        assert_eq!(timeout_minutes_or(Some("1"), 60), 1);
        assert_eq!(timeout_minutes_or(Some("120"), 60), 120);
    }

    #[test]
    fn agent_command_round_trips() {
        let cmd = AgentCommand {
            agent_id: Uuid::nil(),
            kind: AgentCommandKind::Cancel {
                step_run_id: Uuid::nil(),
            },
        };
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains("\"type\":\"cancel\""));
        let back: AgentCommand = serde_json::from_str(&json).unwrap();
        assert!(matches!(back.kind, AgentCommandKind::Cancel { .. }));
        // Nothing executable can ride this channel.
        assert!(serde_json::from_str::<AgentCommand>(
            r#"{"agent_id":"00000000-0000-0000-0000-000000000000","kind":{"type":"message","message":{"type":"offer"}}}"#
        )
        .is_err());
        let d = serde_json::to_string(&AgentCommand {
            agent_id: Uuid::nil(),
            kind: AgentCommandKind::Disconnect { reason: "x".into() },
        })
        .unwrap();
        assert!(d.contains("\"type\":\"disconnect\""));
    }

    #[test]
    fn labels_match_requires_all_required() {
        let agent = vec!["os=linux".to_string(), "docker=true".to_string()];
        assert!(labels_match(&agent, &[]));
        assert!(labels_match(&agent, &["os=linux".to_string()]));
        assert!(!labels_match(&agent, &["os=macos".to_string()]));
        assert!(!labels_match(&[], &["os=linux".to_string()]));
    }
}
