//! Client authentication.
//!
//! One shared secret list, checked in constant time. This is a gateway for a
//! home lab, not an identity provider: the goal is to stop a stray device on
//! the LAN from burning the board's only CPU, not to model tenants.

use axum::http::HeaderMap;

use crate::error::{ApiError, ErrorKind};
use crate::metrics::Metrics;
use crate::state::AppState;
use crate::util::constant_time_eq;

/// Pull the presented key out of `Authorization: Bearer ...` or `x-api-key`.
pub fn presented_key(headers: &HeaderMap) -> Option<&str> {
    if let Some(v) = headers.get(axum::http::header::AUTHORIZATION) {
        let raw = v.to_str().ok()?.trim();
        // Bearer prefix is case-insensitive per RFC 6750.
        if raw.len() > 7 && raw[..7].eq_ignore_ascii_case("bearer ") {
            return Some(raw[7..].trim());
        }
        return Some(raw);
    }
    headers.get("x-api-key")?.to_str().ok().map(str::trim)
}

/// Check a request against the configured keys.
pub fn authorize(state: &AppState, headers: &HeaderMap) -> Result<(), ApiError> {
    if !state.cfg.server.auth.require_auth {
        return Ok(());
    }
    let presented = presented_key(headers).unwrap_or("");
    // Always compare against every key so the work does not depend on which
    // key matched, or on whether one matched at all.
    let mut ok = false;
    for key in &state.client_keys {
        ok |= constant_time_eq(presented.as_bytes(), key.as_bytes());
    }
    if ok {
        return Ok(());
    }
    Metrics::incr(&state.metrics.unauthorized_total);
    Err(ApiError::new(
        ErrorKind::Authentication,
        "missing or invalid API key",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    fn state(require_auth: bool, keys: &[&str]) -> AppState {
        let vars = vec![
            (
                "MINI_ROUTER_PROVIDER_A_URL".to_string(),
                "http://127.0.0.1:11434/v1".to_string(),
            ),
            (
                "MINI_ROUTER_REQUIRE_AUTH".to_string(),
                require_auth.to_string(),
            ),
            ("MINI_ROUTER_API_KEYS".to_string(), keys.join(",")),
        ];
        AppState::new(crate::env::load(&vars).unwrap().config)
    }

    #[test]
    fn bearer_prefix_is_stripped_case_insensitively() {
        assert_eq!(
            presented_key(&headers(&[("authorization", "Bearer sk-1")])),
            Some("sk-1")
        );
        assert_eq!(
            presented_key(&headers(&[("authorization", "bearer sk-1")])),
            Some("sk-1")
        );
        assert_eq!(
            presented_key(&headers(&[("x-api-key", "sk-2")])),
            Some("sk-2")
        );
        assert_eq!(presented_key(&HeaderMap::new()), None);
    }

    #[test]
    fn auth_off_lets_everything_through() {
        let s = state(false, &[]);
        assert!(authorize(&s, &HeaderMap::new()).is_ok());
    }

    #[test]
    fn valid_key_is_accepted_and_wrong_key_is_not() {
        let s = state(true, &["sk-good", "sk-also-good"]);
        assert!(authorize(&s, &headers(&[("authorization", "Bearer sk-good")])).is_ok());
        assert!(authorize(&s, &headers(&[("authorization", "Bearer sk-also-good")])).is_ok());
        let err = authorize(&s, &headers(&[("authorization", "Bearer sk-bad")])).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Authentication);
        assert!(authorize(&s, &HeaderMap::new()).is_err());
    }
}
