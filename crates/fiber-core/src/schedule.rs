//! Pipeline schedule helpers: `interval_minutes` and/or cron expressions.

use chrono::{DateTime, Utc};
use cron::Schedule;
use fiber_proto::{PipelineDefinition, PipelineTriggers};
use std::str::FromStr;

/// Validate schedule fields (especially cron syntax).
pub fn validate_triggers(on: &PipelineTriggers) -> Result<(), String> {
    if let Some(expr) = on.cron.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        Schedule::from_str(expr).map_err(|e| format!("invalid cron `{expr}`: {e}"))?;
    }
    Ok(())
}

/// Next wake time from triggers. Prefers `cron` when set; else `interval_minutes`.
pub fn next_due_from_triggers(
    on: &PipelineTriggers,
    after: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    if let Some(expr) = on.cron.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        return next_from_cron(expr, after);
    }
    on.interval_minutes
        .filter(|m| *m > 0)
        .map(|m| after + chrono::Duration::minutes(m as i64))
}

/// Initial due when creating/updating a pipeline (due soon / now for interval; next cron tick).
pub fn initial_due_from_definition(def: &PipelineDefinition) -> Option<DateTime<Utc>> {
    let on = def.on.as_ref()?;
    if let Some(expr) = on.cron.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        // Next occurrence after now (not immediate spam).
        return next_from_cron(expr, Utc::now());
    }
    on.interval_minutes.filter(|m| *m > 0).map(|_| Utc::now())
}

pub fn has_schedule(on: &PipelineTriggers) -> bool {
    on.cron
        .as_ref()
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
        || on.interval_minutes.filter(|m| *m > 0).is_some()
}

/// The part of the triggers that decides *when* a pipeline fires next: the trimmed cron
/// when set (it takes precedence), else the interval. `None` for no schedule. Two
/// definitions with the same key keep the same `next_due_at` across an edit; a
/// different key means the due time must be recomputed from the new rule, or a
/// daily-to-hourly change still fires first at the old daily time.
pub fn schedule_key(on: Option<&PipelineTriggers>) -> Option<String> {
    let on = on?;
    if let Some(expr) = on.cron.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        return Some(format!("cron:{expr}"));
    }
    on.interval_minutes
        .filter(|m| *m > 0)
        .map(|m| format!("interval:{m}"))
}

pub fn schedule_trigger_label(on: &PipelineTriggers) -> String {
    if let Some(expr) = on.cron.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        return format!("schedule:cron:{expr}");
    }
    if let Some(m) = on.interval_minutes.filter(|m| *m > 0) {
        return format!("schedule:{m}m");
    }
    "schedule".into()
}

/// Cron expressions are **6-field** (with seconds): `SEC MIN HOUR DAY MONTH DOW`
/// e.g. `0 */5 * * * *` = every 5 minutes.
pub fn next_from_cron(expr: &str, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let schedule = Schedule::from_str(expr).ok()?;
    schedule.after(&after).next()
}

#[cfg(test)]
mod tests {
    use super::*;
    use fiber_proto::PipelineTriggers;

    #[test]
    fn cron_every_minute() {
        let after = Utc::now();
        let next = next_from_cron("0 * * * * *", after).expect("parse");
        assert!(next > after);
        assert!((next - after).num_seconds() <= 60);
    }

    #[test]
    fn prefers_cron_over_interval() {
        let on = PipelineTriggers {
            push: None,
            pull_request: None,
            interval_minutes: Some(60),
            cron: Some("0 0 * * * *".into()),
        };
        let after = Utc::now();
        let next = next_due_from_triggers(&on, after).unwrap();
        // Hourly at :00 — within an hour
        assert!((next - after).num_minutes() <= 60);
    }

    #[test]
    fn rejects_bad_cron() {
        let on = PipelineTriggers {
            push: None,
            pull_request: None,
            interval_minutes: None,
            cron: Some("not a cron".into()),
        };
        assert!(validate_triggers(&on).is_err());
    }

    fn triggers(cron: Option<&str>, interval: Option<u32>) -> PipelineTriggers {
        PipelineTriggers {
            push: None,
            pull_request: None,
            interval_minutes: interval,
            cron: cron.map(Into::into),
        }
    }

    #[test]
    fn schedule_key_changes_only_when_the_firing_rule_does() {
        let daily = triggers(Some("0 0 0 * * *"), None);
        // Whitespace and an interval the cron shadows are not a change.
        let same = triggers(Some("  0 0 0 * * *  "), Some(60));
        assert_eq!(schedule_key(Some(&daily)), schedule_key(Some(&same)));
        let hourly = triggers(Some("0 0 * * * *"), None);
        assert_ne!(schedule_key(Some(&daily)), schedule_key(Some(&hourly)));
        assert_ne!(
            schedule_key(Some(&triggers(None, Some(5)))),
            schedule_key(Some(&triggers(None, Some(10))))
        );
    }

    #[test]
    fn schedule_key_is_none_without_a_schedule() {
        assert_eq!(schedule_key(None), None);
        assert_eq!(schedule_key(Some(&triggers(None, None))), None);
        assert_eq!(schedule_key(Some(&triggers(Some("   "), Some(0)))), None);
        // Dropping the schedule is itself a change, so the due time is cleared.
        assert_ne!(
            schedule_key(Some(&triggers(None, Some(5)))),
            schedule_key(None)
        );
    }
}
