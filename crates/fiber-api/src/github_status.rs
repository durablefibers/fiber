//! Reports run outcomes to GitHub as commit statuses.
//!
//! Driven by the scheduler's control-event bus rather than the completion path, so a slow
//! or failing GitHub API can never hold up a run. Runs without a commit (manual,
//! scheduled) are ignored.
//!
//! The bus can drop events: it is a bounded broadcast, and a reporter that stalls on a
//! slow GitHub call falls behind it. A dropped `run_updated` used to be the end of the
//! story — the run was finished, nothing would republish it, and a required check stayed
//! pending forever. So a lag is followed by a re-scan of the runs that reached a terminal
//! status in the window the gap covers.

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use fiber_core::Store;
use fiber_proto::RunEvent;
use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use tracing::warn;
use uuid::Uuid;

/// Shortest gap between two re-scans. A burst of lags — which is what a genuinely
/// overloaded instance produces — costs one query, not one per dropped event.
const RESCAN_MIN_INTERVAL: ChronoDuration = ChronoDuration::seconds(30);
/// How far back a re-scan looks beyond the last one. Covers the runs that finished while
/// the reporter was behind, plus clock skew between replicas writing `finished_at`.
const RESCAN_SLACK: ChronoDuration = ChronoDuration::minutes(5);
/// Terminal runs a single re-scan will post for. A ceiling on the work one lag can cause.
const RESCAN_LIMIT: i64 = 500;
/// Run ids remembered as already reported. Bounded: past it the oldest is forgotten and
/// the worst case is one duplicate status for a run finished long ago.
const RECENT_CAP: usize = 1024;

/// Which runs still need a commit status, and when the gaps are worth re-scanning for.
///
/// Pure bookkeeping, so the decisions a lag turns on are testable without Postgres or a
/// GitHub token.
pub struct ReportLedger {
    /// Instant the next re-scan must start from.
    watermark: DateTime<Utc>,
    /// When the last re-scan ran.
    last_rescan: DateTime<Utc>,
    /// A lag has been seen that no re-scan has covered yet.
    owed: bool,
    recent: VecDeque<Uuid>,
    seen: HashSet<Uuid>,
}

impl ReportLedger {
    pub fn new(now: DateTime<Utc>) -> Self {
        Self {
            watermark: now - RESCAN_SLACK,
            // Behind by a full interval, so the first lag re-scans at once rather than
            // waiting out a window the process has not yet lived through.
            last_rescan: now - RESCAN_MIN_INTERVAL,
            owed: false,
            recent: VecDeque::new(),
            seen: HashSet::new(),
        }
    }

    /// Record that the bus dropped events. The re-scan that answers it may not be
    /// allowed to run yet; the debt is kept until one does.
    pub fn note_lag(&mut self) {
        self.owed = true;
    }

    /// Claim the right to re-scan now, if one is owed and the rate limit allows it.
    ///
    /// Clearing the debt and starting the rate-limit window here, rather than after the
    /// query, is what keeps a burst of lags to one scan: the query and the GitHub calls
    /// that follow take long enough that a second lag would otherwise queue a second
    /// scan of the same window behind the first.
    pub fn take_rescan(&mut self, now: DateTime<Utc>) -> bool {
        if !self.owed || !self.due_for_rescan(now) {
            return false;
        }
        self.owed = false;
        self.last_rescan = now;
        // Not `now`: a run finishing in the instant between this line and the query
        // would otherwise fall into neither this scan nor the next.
        self.watermark = now - RESCAN_SLACK;
        true
    }

    /// Take responsibility for reporting `run_id`. `false` when it has already been
    /// reported — the event path and a re-scan can both reach the same run, and a check
    /// should not flap.
    pub fn claim(&mut self, run_id: Uuid) -> bool {
        if !self.seen.insert(run_id) {
            return false;
        }
        self.recent.push_back(run_id);
        if self.recent.len() > RECENT_CAP
            && let Some(old) = self.recent.pop_front()
        {
            self.seen.remove(&old);
        }
        true
    }

    /// Whether a lag should be answered with a query right now.
    pub fn due_for_rescan(&self, now: DateTime<Utc>) -> bool {
        now - self.last_rescan >= RESCAN_MIN_INTERVAL
    }

    /// Oldest `finished_at` a re-scan has to cover.
    pub fn rescan_from(&self) -> DateTime<Utc> {
        self.watermark
    }
}

pub async fn report_loop(store: Store, scheduler: Arc<fiber_scheduler::Scheduler>) {
    let mut events = scheduler.subscribe_control();
    let mut ledger = ReportLedger::new(Utc::now());
    // The debt a lag leaves has to be paid even if no further event ever arrives — a
    // quiet instance is exactly where one missed terminal event strands a check — so the
    // loop wakes on its own as well as on the bus.
    let mut ticker = tokio::time::interval(
        RESCAN_MIN_INTERVAL
            .to_std()
            .unwrap_or(std::time::Duration::from_secs(30)),
    );
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ticker.tick().await; // the first tick is immediate
    loop {
        let payload = tokio::select! {
            ev = events.recv() => match ev {
                Ok(p) => p,
                // The missed events may have included the only announcement of a
                // finished run, so go and look for one rather than keep waiting for it.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    warn!(missed = n, "commit status reporter lagged");
                    ledger.note_lag();
                    rescan(&store, &mut ledger).await;
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            },
            _ = ticker.tick() => {
                // Pays a debt the rate limit refused when the lag happened.
                rescan(&store, &mut ledger).await;
                continue;
            }
        };
        let Ok(RunEvent::RunUpdated { run_id, status }) = serde_json::from_str(&payload) else {
            continue;
        };
        if !status.is_terminal() || !ledger.claim(run_id) {
            continue;
        }
        match store.get_run(run_id).await {
            Ok(Some(run)) => crate::github::report_run_status(&store, &run).await,
            Ok(None) => {}
            Err(e) => warn!(error = %e, %run_id, "load run for commit status"),
        }
    }
}

/// Post for every run that finished in the window the bus may have dropped.
///
/// A no-op unless a lag is outstanding and the rate limit allows a query.
async fn rescan(store: &Store, ledger: &mut ReportLedger) {
    let from = {
        if !ledger.take_rescan(Utc::now()) {
            return;
        }
        ledger.rescan_from()
    };
    let runs = match store.list_runs_finished_since(from, RESCAN_LIMIT).await {
        Ok(runs) => runs,
        Err(e) => {
            warn!(error = %e, "re-scan for missed commit statuses");
            // Put the debt back so the next tick tries again: a database that was
            // briefly unhappy must not be the reason a required check stays pending.
            // The rate limit still holds, so this is one retry per window, and the
            // window's slack still covers the runs the failed query would have found.
            ledger.note_lag();
            return;
        }
    };
    for run in runs {
        if ledger.claim(run.id) {
            crate::github::report_run_status(store, &run).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000 + secs, 0).expect("valid")
    }

    #[test]
    fn a_run_is_reported_once_however_many_paths_reach_it() {
        let mut ledger = ReportLedger::new(at(0));
        let run = Uuid::new_v4();
        assert!(ledger.claim(run), "the terminal event posts it");
        assert!(
            !ledger.claim(run),
            "a re-scan covering the same window must not post it again"
        );
    }

    #[test]
    fn nothing_is_scanned_until_a_lag_says_something_was_missed() {
        let mut ledger = ReportLedger::new(at(0));
        assert!(
            !ledger.take_rescan(at(0)),
            "a healthy reporter must not query on every tick"
        );
        ledger.note_lag();
        assert!(
            ledger.take_rescan(at(0)),
            "a lag in the first seconds of the process still has to be answered"
        );
    }

    #[test]
    fn a_lag_inside_the_rate_limit_window_is_owed_rather_than_dropped() {
        // The bug this guards: a second lag arriving 5 s after a scan used to be
        // swallowed with nothing recorded, so the terminal event it hid stranded a
        // check exactly as before.
        let mut ledger = ReportLedger::new(at(0));
        ledger.note_lag();
        assert!(ledger.take_rescan(at(0)));
        ledger.note_lag();
        assert!(!ledger.take_rescan(at(5)), "one query, not one per event");
        assert!(!ledger.take_rescan(at(29)));
        assert!(
            ledger.take_rescan(at(30)),
            "the debt from the lag at 5 s is paid when the window opens"
        );
        assert!(
            !ledger.take_rescan(at(90)),
            "and paying it once is enough; nothing is owed now"
        );
    }

    #[test]
    fn a_re_scan_covers_the_window_before_it_rather_than_starting_at_now() {
        let mut ledger = ReportLedger::new(at(1_000));
        // A run that finished a minute before the process even noticed the lag.
        assert!(
            ledger.rescan_from() < at(1_000) - ChronoDuration::minutes(1),
            "the initial window has to reach back past the lag that triggered it"
        );
        ledger.note_lag();
        assert!(ledger.take_rescan(at(1_000)));
        assert!(
            ledger.rescan_from() <= at(1_000) - RESCAN_SLACK,
            "and the next window overlaps this one, so a run finishing during the \
             query is not lost between the two"
        );
    }

    #[test]
    fn remembering_reported_runs_is_bounded() {
        let mut ledger = ReportLedger::new(at(0));
        let first = Uuid::new_v4();
        assert!(ledger.claim(first));
        for _ in 0..RECENT_CAP {
            ledger.claim(Uuid::new_v4());
        }
        assert!(
            ledger.seen.len() <= RECENT_CAP,
            "the set cannot grow forever"
        );
        assert!(
            ledger.claim(first),
            "an id forgotten to keep memory bounded costs at most a duplicate status"
        );
    }
}
