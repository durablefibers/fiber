//! Shared helpers for artifact path sanitization and size limits.

/// Soft cap for HTTP / restored artifacts (WS base64 stays smaller).
pub const MAX_ARTIFACT_BYTES: u64 = 64 * 1024 * 1024;
/// Legacy WebSocket base64 upload limit.
pub const MAX_WS_ARTIFACT_BYTES: u64 = 8 * 1024 * 1024;

pub use fiber_core::ArtifactCaps;

/// The per-step caps this instance runs with. The decision itself lives in `fiber-core`
/// next to the store that enforces it; this is only the environment read.
pub fn caps_from_env() -> ArtifactCaps {
    ArtifactCaps::from_raw(
        std::env::var("FIBER_MAX_ARTIFACTS_PER_STEP")
            .ok()
            .as_deref(),
        std::env::var("FIBER_MAX_ARTIFACT_BYTES_PER_STEP")
            .ok()
            .as_deref(),
    )
}

/// Sanitize a workspace-relative artifact path. Rejects `..` and absolute paths.
pub fn sanitize_artifact_rel_path(name: &str) -> Option<String> {
    let name = name.trim().trim_start_matches('/').trim_start_matches('\\');
    // `..` as a component is refused by the predicate below; `app..tar` is a file name,
    // and refusing it here while compile accepted it failed the upload on every run.
    if name.is_empty() {
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
    // The agent restores only names this accepts; storing one it would refuse loses the
    // artifact silently at the next step instead of now.
    fiber_proto::validate::artifact_path_ok(&cleaned).then_some(cleaned)
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
        assert_eq!(sanitize_artifact_rel_path(""), None);
        assert_eq!(sanitize_artifact_rel_path("   "), None);
        assert_eq!(sanitize_artifact_rel_path("/"), None);
    }

    #[test]
    fn a_double_dot_inside_a_file_name_is_not_traversal() {
        // Compile and the agent accept these (a `..` *component* is what escapes); the
        // server refusing them failed the upload on every run instead.
        assert_eq!(
            sanitize_artifact_rel_path("dist/app..tar").as_deref(),
            Some("dist/app..tar")
        );
        assert_eq!(
            sanitize_artifact_rel_path("a/..b").as_deref(),
            Some("a/..b")
        );
        assert_eq!(sanitize_artifact_rel_path(".."), None);
    }

    #[test]
    fn never_stores_a_name_the_agent_would_refuse_to_restore() {
        for raw in [
            "./x",
            "/./x",
            " ./x",
            "\\./x",
            ".",
            "a/./b",
            "dir/",
            "a b/c",
            "../x",
            "x/..",
            "ünï/cødé",
            "a\tb",
            "..b",
            "app..tar",
        ] {
            if let Some(stored) = sanitize_artifact_rel_path(raw) {
                assert!(
                    fiber_proto::validate::artifact_path_ok(&stored),
                    "{raw:?} stored as {stored:?}, which the agent refuses"
                );
            }
        }
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
