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

/// A project secret that exists but cannot be read back. Typed so the offer path can
/// tell it from a transient store error: a wrong or rotated `FIBER_SECRETS_KEY` is not
/// going to fix itself on the next heartbeat, and a step must not run without the
/// secret, so the step is failed with this message rather than retried.
#[derive(Debug)]
pub struct SecretDecryptError {
    pub name: String,
    pub source: anyhow::Error,
}

impl std::fmt::Display for SecretDecryptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "cannot decrypt project secret {} (is FIBER_SECRETS_KEY right?)",
            self.name
        )
    }
}

impl std::error::Error for SecretDecryptError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}
