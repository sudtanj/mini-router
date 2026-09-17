//! OpenAI-shaped error responses.
//!
//! Clients (and every OpenAI SDK) expect `{"error": {...}}`, so failures the
//! router generates itself are dressed the same way an upstream would dress
//! them. Anything else turns into an unhelpful parse error inside the SDK.

use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::protocol::Protocol;

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

    /// The nearest Anthropic error type name.
    fn anthropic_type(self) -> &'static str {
        match self {
            ErrorKind::InvalidRequest => "invalid_request_error",
            ErrorKind::Authentication => "authentication_error",
            ErrorKind::NotFound => "not_found_error",
            ErrorKind::RateLimit => "rate_limit_error",
            ErrorKind::UpstreamUnavailable => "overloaded_error",
            ErrorKind::Internal => "api_error",
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

    /// The same error, shaped for whichever dialect the client is speaking.
    /// An OpenAI SDK and an Anthropic SDK each parse only their own form, and
    /// a failure the client cannot read is a failure twice over.
    pub fn dialect_body(&self, protocol: Protocol) -> String {
        match protocol {
            Protocol::Openai => self.body(),
            Protocol::Anthropic => {
                let mut obj = serde_json::json!({
                    "type": "error",
                    "error": {
                        "type": self.kind.anthropic_type(),
                        "message": self.message,
                    }
                });
                if let Some(up) = &self.upstream {
                    obj["error"]["upstream"] = serde_json::Value::String(up.clone());
                }
                obj.to_string()
            }
        }
    }

    /// Turn this into a response in the client's dialect.
    pub fn into_dialect(self, protocol: Protocol) -> Response {
        (
            self.kind.status(),
            [(header::CONTENT_TYPE, "application/json")],
            self.dialect_body(protocol),
        )
            .into_response()
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
    fn errors_are_shaped_for_the_clients_dialect() {
        let e = ApiError::new(ErrorKind::RateLimit, "slow down");

        let oai: serde_json::Value =
            serde_json::from_str(&e.dialect_body(Protocol::Openai)).unwrap();
        assert_eq!(oai["error"]["message"], "slow down");
        assert_eq!(oai["error"]["type"], "rate_limit_error");
        assert!(oai.get("type").is_none());

        let ant: serde_json::Value =
            serde_json::from_str(&e.dialect_body(Protocol::Anthropic)).unwrap();
        assert_eq!(ant["type"], "error");
        assert_eq!(ant["error"]["message"], "slow down");
        assert_eq!(ant["error"]["type"], "rate_limit_error");
    }

    #[test]
    fn unavailable_maps_to_overloaded_for_anthropic_clients() {
        let e = unavailable("everything is down");
        let ant: serde_json::Value =
            serde_json::from_str(&e.dialect_body(Protocol::Anthropic)).unwrap();
        assert_eq!(ant["error"]["type"], "overloaded_error");
    }

    #[test]
    fn upstream_is_included_when_known() {
        let e = unavailable("connect refused").with_upstream("opi-a");
        let v: serde_json::Value = serde_json::from_str(&e.body()).unwrap();
        assert_eq!(v["error"]["upstream"], "opi-a");
        assert_eq!(e.kind.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
