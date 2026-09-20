//! Fan-out to `/ws/runs/{id}` viewers, keyed by run.
//!
//! Every viewer used to sit on one process-wide 1024-slot broadcast and JSON-parse every
//! event of every run just to compare `run_id`. That made the cost of one chatty build
//! `O(viewers × global event rate)` in parses, and — worse — a burst on *any* run spent
//! the queue of a viewer watching some *other* run, whose socket was then closed as
//! fatally lagged.
//!
//! Keying by run was chosen over prefixing the payload with the run id for that second
//! reason: a prefix removes the parse but leaves one shared queue, so an unrelated build
//! can still overrun a slow tab. With a channel per run a viewer is only ever woken by,
//! and only ever falls behind, the run it asked for. The map is created on first
//! subscribe and the entry is dropped when the last viewer of that run leaves, so an
//! instance with no viewers carries no channels and publishing is a hash lookup.

use serde::Deserialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;
use uuid::Uuid;

/// Events one run's viewers may fall behind by before the slowest of them is told to
/// resync. Per run, so the figure is now about a single build's output rather than the
/// whole instance's.
const RUN_CHANNEL_CAPACITY: usize = 1024;

/// Just enough of a serialized `RunEvent` to route it.
///
/// Deserializing the whole enum would rebuild every line of a `log_batch` — the one
/// event shape that arrives often — only to read two fields off the front and throw the
/// rest away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventHead {
    pub run_id: Uuid,
    /// Step output rather than a status transition. Log events go to the run's viewers
    /// and nowhere else.
    pub is_log: bool,
}

#[derive(Deserialize)]
struct RawHead {
    #[serde(rename = "type")]
    kind: String,
    run_id: Uuid,
}

/// Route one serialized `RunEvent`. `None` for anything that is not one — a payload from
/// a newer replica included, which this instance must not guess at.
pub fn event_head(payload: &str) -> Option<EventHead> {
    let raw: RawHead = serde_json::from_str(payload).ok()?;
    Some(EventHead {
        run_id: raw.run_id,
        is_log: raw.kind == "log" || raw.kind == "log_batch",
    })
}

type Channels = Arc<Mutex<HashMap<Uuid, broadcast::Sender<String>>>>;

/// Per-run event channels, created on demand.
#[derive(Clone)]
pub struct RunBus {
    channels: Channels,
    capacity: usize,
}

impl Default for RunBus {
    fn default() -> Self {
        Self::new()
    }
}

impl RunBus {
    pub fn new() -> Self {
        Self::with_capacity(RUN_CHANNEL_CAPACITY)
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            channels: Arc::new(Mutex::new(HashMap::new())),
            capacity,
        }
    }

    /// Start receiving `run_id`'s events. The channel exists for as long as at least one
    /// [`RunSubscription`] holds it.
    pub fn subscribe(&self, run_id: Uuid) -> RunSubscription {
        let mut channels = self.lock();
        let tx = channels
            .entry(run_id)
            .or_insert_with(|| broadcast::channel(self.capacity).0);
        RunSubscription {
            rx: tx.subscribe(),
            run_id,
            channels: Arc::clone(&self.channels),
        }
    }

    /// Hand `payload` to this run's viewers. A run nobody is watching costs one lookup
    /// and allocates nothing.
    pub fn publish(&self, run_id: Uuid, payload: &str) {
        let channels = self.lock();
        if let Some(tx) = channels.get(&run_id) {
            let _ = tx.send(payload.to_string());
        }
    }

    /// Runs with a live channel. Fan-out is only ever paid for these.
    pub fn tracked_runs(&self) -> usize {
        self.lock().len()
    }

    /// A poisoned lock here would mean a panic while holding it, which cannot happen:
    /// nothing inside the guard can panic and no `.await` is reached under it.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Uuid, broadcast::Sender<String>>> {
        self.channels.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// One viewer's receiver. Dropping it releases the run's channel when it was the last.
pub struct RunSubscription {
    rx: broadcast::Receiver<String>,
    run_id: Uuid,
    channels: Channels,
}

impl RunSubscription {
    pub async fn recv(&mut self) -> Result<String, broadcast::error::RecvError> {
        self.rx.recv().await
    }
}

impl Drop for RunSubscription {
    fn drop(&mut self) {
        let Ok(mut channels) = self.channels.lock() else {
            return;
        };
        // `self.rx` is still alive here — fields drop after this body — so the last
        // subscriber sees a count of one. Removal happens under the same lock
        // `subscribe` takes, so a viewer arriving in this instant either finds the
        // entry and joins it, or misses it and creates a new one; it can never be
        // handed a channel this drop is about to throw away.
        if let Some(tx) = channels.get(&self.run_id)
            && tx.receiver_count() <= 1
        {
            channels.remove(&self.run_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run() -> Uuid {
        Uuid::new_v4()
    }

    #[test]
    fn a_log_batch_is_routed_without_reading_its_lines() {
        let id = Uuid::new_v4();
        // `lines` is deliberately not the shape `RunEvent::LogBatch` would parse into:
        // routing must not depend on the payload's tail.
        let payload = format!(
            r#"{{"type":"log_batch","run_id":"{id}","step_run_id":"{id}","lines":"not-an-array"}}"#
        );
        let head = event_head(&payload).expect("routable");
        assert_eq!(head.run_id, id);
        assert!(head.is_log);
    }

    #[test]
    fn status_events_are_not_log_events() {
        let id = Uuid::new_v4();
        for kind in ["run_updated", "step_updated", "resync"] {
            let payload = format!(r#"{{"type":"{kind}","run_id":"{id}"}}"#);
            let head = event_head(&payload).expect("routable");
            assert_eq!(head.run_id, id);
            assert!(!head.is_log, "{kind} belongs on the control bus");
        }
        let payload = format!(r#"{{"type":"log","run_id":"{id}"}}"#);
        assert!(event_head(&payload).unwrap().is_log);
    }

    #[test]
    fn an_unroutable_payload_is_dropped_rather_than_broadcast() {
        assert!(event_head("not json").is_none());
        assert!(event_head(r#"{"type":"log_batch"}"#).is_none(), "no run id");
        assert!(event_head(r#"{"run_id":"nope","type":"log"}"#).is_none());
    }

    #[test]
    fn a_run_nobody_watches_costs_no_channel() {
        let bus = RunBus::new();
        bus.publish(run(), r#"{"type":"log_batch"}"#);
        assert_eq!(bus.tracked_runs(), 0);
    }

    #[tokio::test]
    async fn a_viewer_only_receives_its_own_run() {
        let bus = RunBus::new();
        let (a, b) = (run(), run());
        let mut sub_a = bus.subscribe(a);
        let mut sub_b = bus.subscribe(b);
        assert_eq!(bus.tracked_runs(), 2);

        bus.publish(b, "b-event");
        bus.publish(a, "a-event");

        assert_eq!(sub_a.recv().await.unwrap(), "a-event");
        assert_eq!(sub_b.recv().await.unwrap(), "b-event");
        // `a` was never woken by `b`'s event: its next receive is empty, not "b-event".
        assert!(matches!(
            sub_a.rx.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn two_viewers_of_one_run_share_a_channel_and_both_get_the_event() {
        let bus = RunBus::new();
        let id = run();
        let mut one = bus.subscribe(id);
        let mut two = bus.subscribe(id);
        assert_eq!(bus.tracked_runs(), 1);
        bus.publish(id, "event");
        assert_eq!(one.recv().await.unwrap(), "event");
        assert_eq!(two.recv().await.unwrap(), "event");
    }

    #[test]
    fn the_channel_outlives_one_viewer_leaving_and_dies_with_the_last() {
        let bus = RunBus::new();
        let id = run();
        let one = bus.subscribe(id);
        let two = bus.subscribe(id);
        drop(one);
        assert_eq!(
            bus.tracked_runs(),
            1,
            "one viewer left, the other is still on"
        );
        drop(two);
        assert_eq!(bus.tracked_runs(), 0, "last one out drops the channel");
    }

    #[test]
    fn a_viewer_arriving_after_the_last_one_left_gets_a_fresh_channel() {
        let bus = RunBus::new();
        let id = run();
        drop(bus.subscribe(id));
        assert_eq!(bus.tracked_runs(), 0);
        let _again = bus.subscribe(id);
        assert_eq!(bus.tracked_runs(), 1);
    }

    #[tokio::test]
    async fn overrunning_one_run_lags_that_run_only() {
        let bus = RunBus::with_capacity(4);
        let (a, b) = (run(), run());
        let mut slow = bus.subscribe(a);
        let mut other = bus.subscribe(b);
        for i in 0..10 {
            bus.publish(a, &format!("a{i}"));
        }
        bus.publish(b, "b0");
        // The viewer of `b` is untouched by `a`'s burst — the whole point of the split.
        assert_eq!(other.recv().await.unwrap(), "b0");
        assert!(matches!(
            slow.recv().await,
            Err(broadcast::error::RecvError::Lagged(6))
        ));
        // And after the lag it keeps receiving: the oldest event still in the channel.
        assert_eq!(slow.recv().await.unwrap(), "a6");
    }
}
