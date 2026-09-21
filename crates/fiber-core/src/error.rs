//! Typed errors that API layers map to client-facing status codes.

/// Why a store call failed, in the terms an HTTP layer has to answer in.
///
/// The store returns `anyhow::Result`, so this travels boxed and `fiber-api` recovers it
/// with `downcast_ref`. It exists because the alternative was seven sites matching on
/// `contains("forbidden")` and `ends_with("not found")`: rewording a message silently
/// turned a 404 into a 500, and any error from any layer whose text happened to contain
/// "forbidden" — an sqlx message quoting a role name, say — became a 403. The class of
/// bug is that the message is for a human and the status is for a program; only one of
/// them should be load-bearing.
///
/// Statuses are fixed here, not at the call site: `Forbidden` → 403, `NotFound` → 404,
/// `Validation` → 400, `Other` → 500 with the body masked and the cause chain logged.
#[derive(Debug)]
pub enum StoreError {
    /// Authenticated, and not allowed to do this.
    Forbidden(String),
    /// The object does not exist, or no longer does.
    NotFound(String),
    /// Malformed input, or a state transition the caller may not make (a bad cron, the
    /// last owner, retrying a run that is still going).
    Validation(String),
    /// Named for the log, masked for the client.
    Other(String),
}

impl StoreError {
    /// `forbidden`, so the one thing a client is told stays the same word it always was.
    pub fn forbidden() -> Self {
        StoreError::Forbidden("forbidden".into())
    }

    pub fn not_found(what: &str) -> Self {
        StoreError::NotFound(format!("{what} not found"))
    }
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Forbidden(m)
            | StoreError::NotFound(m)
            | StoreError::Validation(m)
            | StoreError::Other(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for StoreError {}

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
