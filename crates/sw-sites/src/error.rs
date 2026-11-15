//! Errors from the site manager.

use sw_core::SiteName;

#[derive(Debug, thiserror::Error)]
pub enum SiteError {
    /// The request is wrong in a way the caller (a person or the agent) can fix.
    #[error("{0}")]
    Invalid(String),

    #[error("site {0:?} does not exist")]
    NotFound(String),

    #[error("{0}")]
    Conflict(String),

    /// A file path was rejected by the sandbox rules.
    #[error("path {path:?} is not allowed: {reason}")]
    Path { path: String, reason: String },

    /// The site failed its checks and was not published.
    #[error("site {site} failed validation: {}", problems.join("; "))]
    Validation {
        site: SiteName,
        problems: Vec<String>,
    },

    /// The new release went live, failed its health check and was rolled back.
    #[error("release {sha} failed its health check and was rolled back: {reason}")]
    HealthCheck { sha: String, reason: String },

    #[error(transparent)]
    Core(#[from] sw_core::Error),

    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),

    #[error("git error: {0}")]
    Git(#[from] git2::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("internal error: {0}")]
    Internal(String),
}

impl SiteError {
    pub(crate) fn path(path: &str, reason: impl Into<String>) -> Self {
        Self::Path {
            path: path.to_owned(),
            reason: reason.into(),
        }
    }

    /// Whether the caller caused this error (as opposed to the server failing).
    pub fn is_user_error(&self) -> bool {
        matches!(
            self,
            Self::Invalid(_)
                | Self::NotFound(_)
                | Self::Conflict(_)
                | Self::Path { .. }
                | Self::Validation { .. }
                | Self::Core(sw_core::Error::InvalidName(_))
        )
    }
}

pub type Result<T, E = SiteError> = std::result::Result<T, E>;
