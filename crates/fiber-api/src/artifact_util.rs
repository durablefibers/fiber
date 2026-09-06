//! Shared helpers for artifact path sanitization and size limits.

/// Soft cap for HTTP / restored artifacts (WS base64 stays smaller).
pub const MAX_ARTIFACT_BYTES: u64 = 64 * 1024 * 1024;
/// Legacy WebSocket base64 upload limit.
pub const MAX_WS_ARTIFACT_BYTES: u64 = 8 * 1024 * 1024;

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
