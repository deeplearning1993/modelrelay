use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;
use std::fmt;

/// Safe router error. It intentionally never contains credentials or headers.
#[derive(Debug)]
pub struct RouterError {
    pub(crate) status: StatusCode,
    pub(crate) code: &'static str,
    pub(crate) message: String,
}

impl RouterError {
    pub(crate) fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: "invalid_request",
            message: message.into(),
        }
    }

    pub(crate) fn unauthorized(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            code: "credential_unavailable",
            message: message.into(),
        }
    }

    pub(crate) fn upstream(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            code: "upstream_error",
            message: message.into(),
        }
    }

    pub(crate) fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "router_error",
            message: message.into(),
        }
    }
}

impl fmt::Display for RouterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for RouterError {}

impl IntoResponse for RouterError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({"error": {"code": self.code, "message": self.message}})),
        )
            .into_response()
    }
}

impl From<cmr_storage::StorageError> for RouterError {
    fn from(error: cmr_storage::StorageError) -> Self {
        Self::internal(error.to_string())
    }
}

impl From<cmr_providers::AdapterError> for RouterError {
    fn from(error: cmr_providers::AdapterError) -> Self {
        match error {
            cmr_providers::AdapterError::MalformedUpstream(message) => Self::upstream(
                StatusCode::BAD_GATEWAY,
                format!("malformed upstream payload: {message}"),
            ),
            cmr_providers::AdapterError::Json(error) => Self::upstream(
                StatusCode::BAD_GATEWAY,
                format!("invalid upstream JSON: {error}"),
            ),
            error @ (cmr_providers::AdapterError::InvalidRequest(_)
            | cmr_providers::AdapterError::Unsupported(_)
            | cmr_providers::AdapterError::InvalidPreset(_)) => {
                Self::bad_request(error.to_string())
            }
        }
    }
}

impl From<reqwest::Error> for RouterError {
    fn from(error: reqwest::Error) -> Self {
        if error.is_timeout() {
            Self::upstream(StatusCode::GATEWAY_TIMEOUT, "upstream request timed out")
        } else {
            Self::upstream(
                StatusCode::BAD_GATEWAY,
                format!("upstream transport failed: {error}"),
            )
        }
    }
}

impl From<serde_json::Error> for RouterError {
    fn from(error: serde_json::Error) -> Self {
        Self::upstream(
            StatusCode::BAD_GATEWAY,
            format!("invalid upstream JSON: {error}"),
        )
    }
}
