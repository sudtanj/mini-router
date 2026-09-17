//! Wire protocols: what a client speaks, and what a provider speaks.
//!
//! mini-router has two front doors -- an OpenAI-compatible one and an
//! Anthropic-compatible one -- and either of them can reach either kind of
//! provider. This module knows which is which: how a request is recognised,
//! which upstream path it maps onto, and how each provider expects to be
//! authenticated.

use axum::http::{header, HeaderMap, HeaderValue};
use serde::Serialize;
use std::fmt;

/// Default value for the `anthropic-version` header, required by the
/// Anthropic Messages API.
pub const DEFAULT_ANTHROPIC_VERSION: &str = "2023-06-01";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    /// OpenAI Chat Completions. Also the shape spoken by Groq, Together,
    /// OpenRouter, DeepSeek, vLLM, llama.cpp, Ollama and most of the rest.
    #[default]
    Openai,
    /// Anthropic Messages.
    Anthropic,
}

impl fmt::Display for Protocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Protocol::Openai => "openai",
            Protocol::Anthropic => "anthropic",
        })
    }
}

/// What a request is asking for, independent of which dialect it asked in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    /// Chat: `/v1/chat/completions` on one side, `/v1/messages` on the other.
    /// The only endpoint that can be translated between protocols.
    Chat,
    /// The model catalogue.
    Models,
    /// Anything else -- embeddings, rerank, audio, legacy completions. These
    /// have no cross-protocol equivalent, so they only ever reach an upstream
    /// that already speaks the dialect they were written in.
    Passthrough(String),
}

impl Endpoint {
    /// The path this endpoint lives at for a given protocol, relative to the
    /// upstream's base URL.
    pub fn path_for(&self, protocol: Protocol) -> String {
        match (self, protocol) {
            (Endpoint::Chat, Protocol::Openai) => "/chat/completions".into(),
            (Endpoint::Chat, Protocol::Anthropic) => "/messages".into(),
            (Endpoint::Models, _) => "/models".into(),
            (Endpoint::Passthrough(p), _) => p.clone(),
        }
    }
}

/// How a request arrived: which dialect, and what it wants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ingress {
    pub protocol: Protocol,
    pub endpoint: Endpoint,
}

impl Ingress {
    /// Classify an incoming request path.
    ///
    /// `/v1/chat/completions` and `/v1/messages` each name their own dialect
    /// unambiguously. `/v1/models` is shared by both, so the header decides:
    /// every Anthropic SDK sends `anthropic-version`, and nothing else does.
    /// Explicit `/openai/v1/...` and `/anthropic/v1/...` prefixes are accepted
    /// too, for when you want to be sure.
    ///
    /// Returns `None` for a path that is not an API path at all, so the router
    /// can 404 it instead of forwarding junk to a paid provider.
    pub fn classify(path: &str, headers: &HeaderMap) -> Option<Ingress> {
        // Explicit prefixes win over any sniffing.
        if let Some(rest) = path.strip_prefix("/openai") {
            return Some(Self::endpoint_of(strip_version(rest), Protocol::Openai));
        }
        if let Some(rest) = path.strip_prefix("/anthropic") {
            return Some(Self::endpoint_of(strip_version(rest), Protocol::Anthropic));
        }
        let rest = path.strip_prefix("/v1")?;
        let default = if headers.contains_key("anthropic-version") {
            Protocol::Anthropic
        } else {
            Protocol::Openai
        };
        Some(Self::endpoint_of(rest, default))
    }

    /// `tail` is the path with any prefix already removed.
    ///
    /// `default_protocol` applies to paths that do not name a dialect
    /// themselves; the two chat paths always override it.
    fn endpoint_of(tail: &str, default_protocol: Protocol) -> Ingress {
        let tail = if tail.is_empty() { "/" } else { tail };
        let (protocol, endpoint) = match tail {
            "/chat/completions" => (Protocol::Openai, Endpoint::Chat),
            "/messages" => (Protocol::Anthropic, Endpoint::Chat),
            "/models" => (default_protocol, Endpoint::Models),
            other => (default_protocol, Endpoint::Passthrough(other.to_owned())),
        };
        Ingress { protocol, endpoint }
    }

    /// Model ids appear in the path for `GET /v1/models/{id}`.
    pub fn model_id_in_path(&self) -> Option<&str> {
        match &self.endpoint {
            Endpoint::Passthrough(p) => p.strip_prefix("/models/"),
            _ => None,
        }
    }
}

/// Remove a leading `/v1`, if there is one.
fn strip_version(path: &str) -> &str {
    path.strip_prefix("/v1").unwrap_or(path)
}

/// Set the credential headers a provider expects, removing the other
/// protocol's so a leftover header from the client cannot confuse it.
pub fn apply_auth(
    headers: &mut HeaderMap,
    protocol: Protocol,
    api_key: Option<&str>,
    anthropic_version: &str,
) {
    headers.remove(header::AUTHORIZATION);
    headers.remove("x-api-key");

    match protocol {
        Protocol::Openai => {
            if let Some(key) = api_key {
                if let Ok(mut v) = HeaderValue::from_str(&format!("Bearer {key}")) {
                    v.set_sensitive(true);
                    headers.insert(header::AUTHORIZATION, v);
                }
            }
        }
        Protocol::Anthropic => {
            if let Some(key) = api_key {
                if let Ok(mut v) = HeaderValue::from_str(key) {
                    v.set_sensitive(true);
                    headers.insert("x-api-key", v);
                }
            }
            // Required by the Messages API. Respect one the client already
            // chose, so a client pinning an older version keeps working.
            if !headers.contains_key("anthropic-version") {
                if let Ok(v) = HeaderValue::from_str(anthropic_version) {
                    headers.insert("anthropic-version", v);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn chat_paths_name_their_own_dialect() {
        let none = HeaderMap::new();
        let oai = Ingress::classify("/v1/chat/completions", &none).unwrap();
        assert_eq!(oai.protocol, Protocol::Openai);
        assert_eq!(oai.endpoint, Endpoint::Chat);

        let ant = Ingress::classify("/v1/messages", &none).unwrap();
        assert_eq!(ant.protocol, Protocol::Anthropic);
        assert_eq!(ant.endpoint, Endpoint::Chat);

        // Even with the other protocol's header present, the path wins.
        let ant2 = Ingress::classify(
            "/v1/messages",
            &headers(&[("anthropic-version", "2023-06-01")]),
        )
        .unwrap();
        assert_eq!(ant2.protocol, Protocol::Anthropic);
    }

    #[test]
    fn models_is_disambiguated_by_the_anthropic_header() {
        let plain = Ingress::classify("/v1/models", &HeaderMap::new()).unwrap();
        assert_eq!(plain.protocol, Protocol::Openai);
        assert_eq!(plain.endpoint, Endpoint::Models);

        let ant = Ingress::classify(
            "/v1/models",
            &headers(&[("anthropic-version", "2023-06-01")]),
        )
        .unwrap();
        assert_eq!(ant.protocol, Protocol::Anthropic);
        assert_eq!(ant.endpoint, Endpoint::Models);
    }

    #[test]
    fn explicit_prefixes_override_everything() {
        let h = headers(&[("anthropic-version", "2023-06-01")]);
        let forced_oai = Ingress::classify("/openai/v1/models", &h).unwrap();
        assert_eq!(forced_oai.protocol, Protocol::Openai);

        let forced_ant = Ingress::classify("/anthropic/v1/models", &HeaderMap::new()).unwrap();
        assert_eq!(forced_ant.protocol, Protocol::Anthropic);

        // And the chat endpoints keep working under a prefix.
        let c = Ingress::classify("/anthropic/v1/messages", &HeaderMap::new()).unwrap();
        assert_eq!(c.endpoint, Endpoint::Chat);
        assert_eq!(c.protocol, Protocol::Anthropic);
    }

    #[test]
    fn non_api_paths_are_not_classified() {
        // These must 404 rather than be forwarded to a paid provider.
        for path in ["/not-a-thing", "/", "/admin/upstreams", "/v2/models"] {
            assert!(
                Ingress::classify(path, &HeaderMap::new()).is_none(),
                "{path} should not classify as an API path"
            );
        }
    }

    #[test]
    fn other_endpoints_are_passthrough() {
        let e = Ingress::classify("/v1/embeddings", &HeaderMap::new()).unwrap();
        assert_eq!(e.endpoint, Endpoint::Passthrough("/embeddings".into()));
        assert_eq!(e.protocol, Protocol::Openai);
    }

    #[test]
    fn chat_maps_onto_the_right_upstream_path() {
        assert_eq!(
            Endpoint::Chat.path_for(Protocol::Openai),
            "/chat/completions"
        );
        assert_eq!(Endpoint::Chat.path_for(Protocol::Anthropic), "/messages");
        assert_eq!(Endpoint::Models.path_for(Protocol::Anthropic), "/models");
        assert_eq!(
            Endpoint::Passthrough("/embeddings".into()).path_for(Protocol::Openai),
            "/embeddings"
        );
    }

    #[test]
    fn model_id_is_read_out_of_the_path() {
        let e = Ingress::classify("/v1/models/gpt-4o-mini", &HeaderMap::new()).unwrap();
        assert_eq!(e.model_id_in_path(), Some("gpt-4o-mini"));
        let c = Ingress::classify("/v1/chat/completions", &HeaderMap::new()).unwrap();
        assert_eq!(c.model_id_in_path(), None);
    }

    #[test]
    fn openai_auth_is_a_bearer_token() {
        let mut h = headers(&[("x-api-key", "leftover-from-client")]);
        apply_auth(
            &mut h,
            Protocol::Openai,
            Some("sk-up"),
            DEFAULT_ANTHROPIC_VERSION,
        );
        assert_eq!(h.get(header::AUTHORIZATION).unwrap(), "Bearer sk-up");
        assert!(
            h.get("x-api-key").is_none(),
            "the other dialect's key must be dropped"
        );
    }

    #[test]
    fn anthropic_auth_is_x_api_key_plus_version() {
        let mut h = headers(&[("authorization", "Bearer leftover-from-client")]);
        apply_auth(
            &mut h,
            Protocol::Anthropic,
            Some("sk-ant"),
            DEFAULT_ANTHROPIC_VERSION,
        );
        assert_eq!(h.get("x-api-key").unwrap(), "sk-ant");
        assert_eq!(h.get("anthropic-version").unwrap(), "2023-06-01");
        assert!(h.get(header::AUTHORIZATION).is_none());
    }

    #[test]
    fn a_client_pinned_anthropic_version_is_kept() {
        let mut h = headers(&[("anthropic-version", "2024-10-22")]);
        apply_auth(
            &mut h,
            Protocol::Anthropic,
            Some("k"),
            DEFAULT_ANTHROPIC_VERSION,
        );
        assert_eq!(h.get("anthropic-version").unwrap(), "2024-10-22");
    }

    #[test]
    fn protocol_renders_lowercase_for_the_admin_view() {
        assert_eq!(
            serde_json::to_string(&Protocol::Openai).unwrap(),
            "\"openai\""
        );
        assert_eq!(
            serde_json::to_string(&Protocol::Anthropic).unwrap(),
            "\"anthropic\""
        );
        assert_eq!(Protocol::Openai.to_string(), "openai");
    }
}
