//! Errors from model calls, classified so callers can tell what to do next.

use std::time::Duration;

#[derive(Debug, Clone, thiserror::Error)]
pub enum LlmError {
    /// The provider configuration is unusable (missing key, bad URL).
    #[error("{0}")]
    Config(String),

    /// The key was rejected. Retrying will not help.
    #[error("the provider rejected the API key ({status}): {message}")]
    Auth { status: u16, message: String },

    /// Too many requests, or the account's quota is used up.
    #[error("the provider is rate limiting or out of quota (429): {message}")]
    RateLimited {
        message: String,
        /// How long the provider asked us to wait, if it said.
        retry_after: Option<Duration>,
    },

    /// The provider said the request itself is wrong.
    #[error("the provider rejected the request ({status}): {message}")]
    BadRequest { status: u16, message: String },

    /// The provider failed on its side.
    #[error("the provider had a server error ({status}): {message}")]
    Server { status: u16, message: String },

    /// The request could not be sent or the connection broke.
    #[error("could not reach the provider: {0}")]
    Network(String),

    #[error("the provider did not answer within {0:?}")]
    Timeout(Duration),

    /// The response was not in the expected format.
    #[error("unexpected response from the provider: {0}")]
    Protocol(String),
}

impl LlmError {
    /// Whether trying the same request again might succeed.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            LlmError::RateLimited { .. }
                | LlmError::Server { .. }
                | LlmError::Network(_)
                | LlmError::Timeout(_)
        )
    }
}
