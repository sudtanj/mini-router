//! Active health probing and model discovery.
//!
//! One lightweight task per upstream. The probe doubles as discovery: the
//! response to `GET {base}/models` tells us both that the box is alive and
//! which models it is currently holding, so a board that swaps models keeps
//! routing correctly without a restart.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderMap, Request};
use http_body_util::{BodyExt, Limited};
use tokio::task::JoinHandle;
use tokio::time::{interval_at, Instant, MissedTickBehavior};

use crate::protocol::apply_auth;
use crate::state::SharedState;
use crate::upstream::Upstream;

/// Cap on a probe response. A model list is a few kilobytes; anything larger is
/// a misconfigured URL pointing at something that is not an API.
const MAX_PROBE_BYTES: usize = 256 * 1024;

/// Start the probe loops. Returns the handles so the caller can abort them on
/// shutdown. An interval of 0 disables probing entirely: upstream health is
/// then judged from live traffic alone.
pub fn spawn_probes(state: SharedState) -> Vec<JoinHandle<()>> {
    if state.cfg.health.interval_secs == 0 {
        tracing::info!("active health probing disabled (health.interval_secs = 0)");
        return Vec::new();
    }
    let interval = state.cfg.health.interval();
    state
        .upstreams
        .iter()
        .cloned()
        .enumerate()
        .map(|(i, up)| {
            let state = state.clone();
            tokio::spawn(async move {
                // Stagger the probes so a shelf of boards is not woken up in
                // the same millisecond every cycle.
                let stagger = interval
                    .mul_f64(i as f64 / state.upstreams.len().max(1) as f64)
                    .min(interval);
                let mut ticker = interval_at(Instant::now() + stagger, interval);
                ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
                loop {
                    probe(&state, &up).await;
                    ticker.tick().await;
                }
            })
        })
        .collect()
}

/// Probe one upstream once, updating health and the discovered model list.
pub async fn probe(state: &SharedState, up: &Arc<Upstream>) {
    let cfg = &state.cfg.health;
    let url = up.cfg.join(&cfg.path);
    let mut builder = Request::builder().method("GET").uri(&url);
    if let Some(hs) = builder.headers_mut() {
        // Probe with the credentials this provider expects, in its dialect.
        *hs = HeaderMap::new();
        for (name, value) in &up.cfg.headers {
            if let (Ok(n), Ok(v)) = (
                axum::http::HeaderName::try_from(name.as_str()),
                axum::http::HeaderValue::from_str(value),
            ) {
                hs.insert(n, v);
            }
        }
        apply_auth(
            hs,
            up.cfg.protocol,
            up.api_key.as_deref(),
            &state.cfg.translate.anthropic_version,
        );
    }
    let Ok(req) = builder.body(Body::empty()) else {
        tracing::error!(upstream = %up.name, url = %url, "invalid health probe url");
        return;
    };

    let outcome = tokio::time::timeout(cfg.timeout(), state.client.request(req)).await;
    match outcome {
        Err(_) => {
            fail(
                state,
                up,
                format!("health probe timed out after {}s", cfg.timeout_secs),
            )
            .await;
        }
        Ok(Err(e)) => {
            fail(state, up, format!("health probe failed: {e}")).await;
        }
        Ok(Ok(resp)) => {
            let status = resp.status();
            if !status.is_success() {
                fail(state, up, format!("health probe returned {status}")).await;
                return;
            }
            let body = Limited::new(resp.into_body(), MAX_PROBE_BYTES)
                .collect()
                .await
                .map(|c| c.to_bytes());
            if let Ok(bytes) = body {
                if let Some(models) = parse_model_ids(&bytes) {
                    up.set_discovered_models(models).await;
                }
            }
            let was_down = up.health() != crate::upstream::Health::Up;
            up.record_success(cfg.success_threshold);
            if was_down {
                up.set_last_error(None).await;
            }
        }
    }
}

async fn fail(state: &SharedState, up: &Arc<Upstream>, message: String) {
    let cfg = &state.cfg.health;
    tracing::debug!(upstream = %up.name, "{message}");
    up.set_last_error(Some(message)).await;
    up.record_failure(cfg.failure_threshold, cfg.cooldown());
}

/// Pull model ids out of a `GET /models` response.
///
/// Both dialects use `data[].id`, so one parser covers OpenAI, Anthropic and
/// the many OpenAI-compatible providers. Ollama's native
/// `{"models": [{"name": ...}]}` is accepted too, for a local model server
/// sitting alongside the remote ones.
pub fn parse_model_ids(body: &[u8]) -> Option<Vec<String>> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    let items = v
        .get("data")
        .or_else(|| v.get("models"))
        .and_then(|d| d.as_array())?;
    let mut ids: Vec<String> = items
        .iter()
        .filter_map(|item| {
            item.get("id")
                .or_else(|| item.get("name"))
                .and_then(|i| i.as_str())
                .map(str::to_owned)
        })
        .collect();
    ids.sort();
    ids.dedup();
    Some(ids)
}

/// Sleep helper used by the shutdown path to let in-flight streams drain.
pub async fn drain(grace: Duration) {
    tokio::time::sleep(grace).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_openai_model_list() {
        let body = br#"{"object":"list","data":[
            {"id":"gpt-4o-mini","object":"model"},
            {"id":"gpt-4o","object":"model"}]}"#;
        assert_eq!(
            parse_model_ids(body).unwrap(),
            vec!["gpt-4o".to_string(), "gpt-4o-mini".to_string()]
        );
    }

    #[test]
    fn parses_anthropic_model_list() {
        let body = br#"{"data":[
            {"type":"model","id":"claude-haiku-4-5","display_name":"Claude Haiku 4.5"},
            {"type":"model","id":"claude-sonnet-4-5","display_name":"Claude Sonnet 4.5"}],
            "has_more":false}"#;
        assert_eq!(
            parse_model_ids(body).unwrap(),
            vec![
                "claude-haiku-4-5".to_string(),
                "claude-sonnet-4-5".to_string()
            ]
        );
    }

    #[test]
    fn parses_ollama_native_list() {
        let body = br#"{"models":[{"name":"llama3.2:1b"},{"name":"gemma2:2b"}]}"#;
        assert_eq!(
            parse_model_ids(body).unwrap(),
            vec!["gemma2:2b".to_string(), "llama3.2:1b".to_string()]
        );
    }

    #[test]
    fn deduplicates_and_sorts() {
        let body = br#"{"data":[{"id":"b"},{"id":"a"},{"id":"b"}]}"#;
        assert_eq!(
            parse_model_ids(body).unwrap(),
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn rejects_bodies_that_are_not_model_lists() {
        assert!(parse_model_ids(b"not json").is_none());
        assert!(parse_model_ids(br#"{"error":"nope"}"#).is_none());
        assert_eq!(
            parse_model_ids(br#"{"data":[]}"#).unwrap(),
            Vec::<String>::new()
        );
    }
}
