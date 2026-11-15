//! API errors and their JSON form.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;
use sw_agent::AgentError;
use sw_sites::SiteError;

/// An error returned by an API handler, rendered as `{"error": "..."}`.
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }
}

impl From<SiteError> for ApiError {
    fn from(e: SiteError) -> Self {
        let status = match &e {
            SiteError::NotFound(_) => StatusCode::NOT_FOUND,
            SiteError::Conflict(_) => StatusCode::CONFLICT,
            SiteError::Validation { .. } => StatusCode::UNPROCESSABLE_ENTITY,
            SiteError::HealthCheck { .. } => StatusCode::BAD_GATEWAY,
            e if e.is_user_error() => StatusCode::BAD_REQUEST,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        if status.is_server_error() {
            tracing::error!(error = %e, "request failed");
        }
        Self::new(status, e.to_string())
    }
}

impl From<AgentError> for ApiError {
    fn from(e: AgentError) -> Self {
        let status = match &e {
            AgentError::Invalid(_) | AgentError::Provider(_) => StatusCode::BAD_REQUEST,
            AgentError::Busy(_) => StatusCode::CONFLICT,
            AgentError::Budget(_) => StatusCode::TOO_MANY_REQUESTS,
            AgentError::NotFound(_) => StatusCode::NOT_FOUND,
            AgentError::Site(_) | AgentError::Db(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        match e {
            AgentError::Site(site) => site.into(),
            other => {
                if status.is_server_error() {
                    tracing::error!(error = %other, "request failed");
                }
                Self::new(status, other.to_string())
            }
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

pub type ApiResult<T> = Result<T, ApiError>;
