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
