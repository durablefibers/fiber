//! Shared helpers for artifact path sanitization and size limits.

/// Soft cap for HTTP / restored artifacts (WS base64 stays smaller).
pub const MAX_ARTIFACT_BYTES: u64 = 64 * 1024 * 1024;
/// Legacy WebSocket base64 upload limit.
pub const MAX_WS_ARTIFACT_BYTES: u64 = 8 * 1024 * 1024;

/// Artifacts one step may store, and how many bytes they may add up to.
///
/// A step is leased by an agent and a project *writer* decides what it uploads, so
/// without a ceiling one pipeline can fill the artifact store one 64 MiB object at a
/// time. The defaults are generous for a build (fifty files, half a gigabyte) and small
/// enough that a runaway loop stops being the operator's disk problem.
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
    pub fn from_env() -> Self {
        Self::from_raw(
            std::env::var("FIBER_MAX_ARTIFACTS_PER_STEP")
                .ok()
                .as_deref(),
            std::env::var("FIBER_MAX_ARTIFACT_BYTES_PER_STEP")
                .ok()
                .as_deref(),
        )
    }

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

/// Sanitize a workspace-relative artifact path. Rejects `..` and absolute paths.
pub fn sanitize_artifact_rel_path(name: &str) -> Option<String> {
    let name = name.trim().trim_start_matches('/').trim_start_matches('\\');
    if name.is_empty() || name.contains("..") {
        return None;
    }
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' || c == '/' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() || cleaned.starts_with('/') {
        None
    } else {
        Some(cleaned)
    }
}

/// Basename for Content-Disposition / object key leaf.
pub fn artifact_basename(rel: &str) -> String {
    std::path::Path::new(rel)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(rel)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- per-step caps -----------------------------------------------------------------

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

    #[test]
    fn accepts_plain_relative_paths() {
        assert_eq!(
            sanitize_artifact_rel_path("out/VERSION").as_deref(),
            Some("out/VERSION")
        );
        assert_eq!(
            sanitize_artifact_rel_path("release.tar.gz").as_deref(),
            Some("release.tar.gz")
        );
        assert_eq!(
            sanitize_artifact_rel_path("a-b_c.d/e").as_deref(),
            Some("a-b_c.d/e")
        );
    }

    #[test]
    fn strips_leading_slashes_and_whitespace() {
        assert_eq!(
            sanitize_artifact_rel_path("/etc/passwd").as_deref(),
            Some("etc/passwd")
        );
        assert_eq!(
            sanitize_artifact_rel_path("\\\\x/y").as_deref(),
            Some("x/y")
        );
        assert_eq!(
            sanitize_artifact_rel_path("  out/a  ").as_deref(),
            Some("out/a")
        );
    }

    #[test]
    fn rejects_traversal_and_empty() {
        assert_eq!(sanitize_artifact_rel_path("../secret"), None);
        assert_eq!(sanitize_artifact_rel_path("out/../../secret"), None);
        assert_eq!(sanitize_artifact_rel_path("a/..b"), None);
        assert_eq!(sanitize_artifact_rel_path(""), None);
        assert_eq!(sanitize_artifact_rel_path("   "), None);
        assert_eq!(sanitize_artifact_rel_path("/"), None);
    }

    #[test]
    fn replaces_unsafe_characters() {
        assert_eq!(
            sanitize_artifact_rel_path("a b\0c").as_deref(),
            Some("a_b_c")
        );
        assert_eq!(
            sanitize_artifact_rel_path("héllo/wörld").as_deref(),
            Some("h_llo/w_rld")
        );
        assert_eq!(
            sanitize_artifact_rel_path("$(id)").as_deref(),
            Some("__id_")
        );
    }

    #[test]
    fn basename_is_leaf() {
        assert_eq!(artifact_basename("out/dir/file.txt"), "file.txt");
        assert_eq!(artifact_basename("file.txt"), "file.txt");
    }
}
