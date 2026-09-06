//! Typed errors that API layers map to client-facing status codes.

/// A request that failed validation (malformed definition, bad cron, a state
/// transition the caller may not make, ...). `fiber-api` maps this — and
/// `dag::DagError` — to `400`; untyped errors are `500`.
#[derive(Debug)]
pub struct ValidationError(pub String);

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ValidationError {}
