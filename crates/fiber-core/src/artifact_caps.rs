//! Per-step artifact caps: how many artifacts one step may store, and how many bytes they
//! may add up to.
//!
//! A step is leased by an agent and a project *writer* decides what it uploads, so without
//! a ceiling one pipeline can fill the artifact store one 64 MiB object at a time. The
//! defaults are generous for a build (fifty files, half a gigabyte) and small enough that
//! a runaway loop stops being the operator's disk problem.
//!
//! The decision lives next to the store because the store is where it is enforced:
//! [`crate::Store::create_artifact`] asks it under a per-step lock, in the transaction
//! that inserts the row. Reading the environment is `fiber-api`'s job.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArtifactCaps {
    pub max_count: i64,
    pub max_total_bytes: i64,
}

pub const DEFAULT_MAX_ARTIFACTS_PER_STEP: i64 = 50;
pub const DEFAULT_MAX_ARTIFACT_BYTES_PER_STEP: i64 = 512 * 1024 * 1024;

impl Default for ArtifactCaps {
    fn default() -> Self {
        Self {
            max_count: DEFAULT_MAX_ARTIFACTS_PER_STEP,
            max_total_bytes: DEFAULT_MAX_ARTIFACT_BYTES_PER_STEP,
        }
    }
}

impl ArtifactCaps {
    /// Parsing and flooring, separated from the environment so it can be tested. A
    /// nonsense or non-positive value falls back to the default rather than to zero,
    /// which would refuse every artifact on the instance.
    pub fn from_raw(count: Option<&str>, bytes: Option<&str>) -> Self {
        fn positive(raw: Option<&str>, default: i64) -> i64 {
            raw.and_then(|v| v.trim().parse::<i64>().ok())
                .filter(|n| *n > 0)
                .unwrap_or(default)
        }
        Self {
            max_count: positive(count, DEFAULT_MAX_ARTIFACTS_PER_STEP),
            max_total_bytes: positive(bytes, DEFAULT_MAX_ARTIFACT_BYTES_PER_STEP),
        }
    }

    /// Whether one more artifact of `size` bytes fits, given what the step already stored
    /// under *other* names. Re-uploading the same name replaces its row, so its bytes are
    /// excluded by the caller — a step retried five times must not exhaust its own cap.
    pub fn refusal(&self, other_count: i64, other_bytes: i64, size: i64) -> Option<String> {
        if other_count + 1 > self.max_count {
            return Some(format!(
                "step already has {other_count} artifacts (limit {}); \
                 archive them into one file or raise FIBER_MAX_ARTIFACTS_PER_STEP",
                self.max_count
            ));
        }
        if other_bytes.saturating_add(size) > self.max_total_bytes {
            return Some(format!(
                "step artifacts would total {} bytes (limit {}); \
                 raise FIBER_MAX_ARTIFACT_BYTES_PER_STEP",
                other_bytes.saturating_add(size),
                self.max_total_bytes
            ));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_fall_back_rather_than_refusing_everything() {
        let d = ArtifactCaps::from_raw(None, None);
        assert_eq!(d.max_count, DEFAULT_MAX_ARTIFACTS_PER_STEP);
        assert_eq!(d.max_total_bytes, DEFAULT_MAX_ARTIFACT_BYTES_PER_STEP);
        // Zero would refuse every artifact on the instance, and a typo would too.
        assert_eq!(ArtifactCaps::from_raw(Some("0"), Some("-1")), d);
        assert_eq!(ArtifactCaps::from_raw(Some("many"), Some("")), d);
        let set = ArtifactCaps::from_raw(Some("3"), Some("100"));
        assert_eq!((set.max_count, set.max_total_bytes), (3, 100));
    }

    #[test]
    fn the_count_cap_admits_exactly_its_limit() {
        let caps = ArtifactCaps::from_raw(Some("3"), Some("1000"));
        assert!(caps.refusal(2, 0, 1).is_none(), "the third artifact fits");
        let msg = caps.refusal(3, 0, 1).expect("the fourth must be refused");
        assert!(msg.contains("FIBER_MAX_ARTIFACTS_PER_STEP"), "{msg}");
    }

    #[test]
    fn the_byte_cap_counts_the_incoming_artifact() {
        let caps = ArtifactCaps::from_raw(Some("50"), Some("1000"));
        assert!(caps.refusal(1, 900, 100).is_none(), "exactly at the limit");
        let msg = caps
            .refusal(1, 900, 101)
            .expect("one byte over must be refused");
        assert!(msg.contains("FIBER_MAX_ARTIFACT_BYTES_PER_STEP"), "{msg}");
        // A huge claimed size cannot wrap into "fits".
        assert!(caps.refusal(1, 900, i64::MAX).is_some());
    }

    #[test]
    fn re_uploading_the_same_name_does_not_spend_the_cap_twice() {
        // A step is at-least-once: the same name arrives again on every retry, and its
        // row is replaced. Counting the old row would fail a step for repeating itself.
        let caps = ArtifactCaps::from_raw(Some("1"), Some("100"));
        assert!(
            caps.refusal(0, 0, 100).is_none(),
            "the replacement of the only artifact still fits"
        );
    }
}
