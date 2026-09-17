//! OpenAI-shaped error responses.
//!
//! Clients (and every OpenAI SDK) expect `{"error": {...}}`, so failures the
//! router generates itself are dressed the same way an upstream would dress
//! them. Anything else turns into an unhelpful parse error inside the SDK.

use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    InvalidRequest,
    Authentication,
    NotFound,
    RateLimit,
    UpstreamUnavailable,
    Internal,
}

impl ErrorKind {
    fn as_str(self) -> &'static str {
        match self {
            ErrorKind::InvalidRequest => "invalid_request_error",
            ErrorKind::Authentication => "authentication_error",
            ErrorKind::NotFound => "not_found_error",
            ErrorKind::RateLimit => "rate_limit_error",
            ErrorKind::UpstreamUnavailable => "upstream_unavailable",
            ErrorKind::Internal => "internal_error",
        }
    }

    pub fn status(self) -> StatusCode {
        match self {
            ErrorKind::InvalidRequest => StatusCode::BAD_REQUEST,
            ErrorKind::Authentication => StatusCode::UNAUTHORIZED,
            ErrorKind::NotFound => StatusCode::NOT_FOUND,
            ErrorKind::RateLimit => StatusCode::TOO_MANY_REQUESTS,
            ErrorKind::UpstreamUnavailable => StatusCode::SERVICE_UNAVAILABLE,
            ErrorKind::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

#[derive(Debug)]
pub struct ApiError {
    pub kind: ErrorKind,
    pub message: String,
    /// Upstream whose failure produced this, when there is one.
    pub upstream: Option<String>,
}

impl ApiError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            upstream: None,
        }
    }

    pub fn with_upstream(mut self, name: impl Into<String>) -> Self {
        self.upstream = Some(name.into());
        self
    }

    pub fn body(&self) -> String {
        let mut obj = serde_json::json!({
            "error": {
                "message": self.message,
                "type": self.kind.as_str(),
                "code": self.kind.as_str(),
            }
        });
        if let Some(up) = &self.upstream {
            obj["error"]["upstream"] = serde_json::Value::String(up.clone());
        }
        obj.to_string()
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.kind.status();
        (
            status,
            [(header::CONTENT_TYPE, "application/json")],
            self.body(),
        )
            .into_response()
    }
}

pub fn invalid_request(msg: impl Into<String>) -> ApiError {
    ApiError::new(ErrorKind::InvalidRequest, msg)
}

pub fn unavailable(msg: impl Into<String>) -> ApiError {
    ApiError::new(ErrorKind::UpstreamUnavailable, msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_is_openai_shaped() {
        let e = ApiError::new(ErrorKind::InvalidRequest, "missing model");
        let v: serde_json::Value = serde_json::from_str(&e.body()).unwrap();
        assert_eq!(v["error"]["message"], "missing model");
        assert_eq!(v["error"]["type"], "invalid_request_error");
        assert!(v["error"]["upstream"].is_null());
    }

    #[test]
    fn upstream_is_included_when_known() {
        let e = unavailable("connect refused").with_upstream("opi-a");
        let v: serde_json::Value = serde_json::from_str(&e.body()).unwrap();
        assert_eq!(v["error"]["upstream"], "opi-a");
        assert_eq!(e.kind.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
