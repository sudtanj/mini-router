//! The aggregated model catalogue.
//!
//! This is the "aggregator" half of mini-router: one place to ask what you can
//! run, across every provider account you have. The answer is the union of
//! what each provider reports, plus the pools and aliases you defined, served
//! in whichever dialect the client asked in.

use std::collections::BTreeMap;

use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};

use crate::error::{ApiError, ErrorKind};
use crate::protocol::Protocol;
use crate::state::SharedState;
use crate::util::now_millis;

/// One entry in the catalogue, before it is dressed in a dialect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelEntry {
    pub id: String,
    /// Providers that can serve it.
    pub upstreams: Vec<String>,
    /// Set when this entry is a pool: the provider-side models behind it.
    pub pool_members: Vec<String>,
    /// Set when this entry exists because of an `[alias]` mapping.
    pub alias_for: Option<String>,
    pub description: Option<String>,
}

impl ModelEntry {
    fn owned_by(&self) -> String {
        if !self.pool_members.is_empty() {
            return "mini-router-pool".into();
        }
        match self.upstreams.len() {
            1 => self.upstreams[0].clone(),
            _ => "mini-router".into(),
        }
    }

    fn display_name(&self) -> String {
        self.description.clone().unwrap_or_else(|| self.id.clone())
    }
}

/// Build the catalogue: pools, then every model an in-rotation provider
/// reports, then the aliases that resolve onto one of those.
pub async fn catalogue(state: &SharedState) -> Vec<ModelEntry> {
    let mut by_model: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for up in &state.upstreams {
        if !up.is_available() {
            continue;
        }
        for m in up.models().await {
            by_model.entry(m).or_default().push(up.name.clone());
        }
    }

    let mut entries: Vec<ModelEntry> = Vec::new();

    // Pools first: these are the names most deployments actually call.
    for (name, pool) in &state.cfg.pools {
        let mut upstreams = Vec::new();
        let mut members = Vec::new();
        for m in &pool.members {
            let Some(up) = state.get(&m.upstream) else {
                continue;
            };
            members.push(format!("{}:{}", m.upstream, m.model));
            if up.is_available() && !upstreams.contains(&m.upstream) {
                upstreams.push(m.upstream.clone());
            }
        }
        entries.push(ModelEntry {
            id: name.clone(),
            upstreams,
            pool_members: members,
            alias_for: None,
            description: pool.description.clone(),
        });
    }

    for (id, ups) in &by_model {
        entries.push(ModelEntry {
            id: id.clone(),
            upstreams: ups.clone(),
            pool_members: Vec::new(),
            alias_for: None,
            description: None,
        });
    }

    for (alias, target) in &state.cfg.alias {
        if entries.iter().any(|e| e.id == *alias) {
            continue;
        }
        let resolved = state.cfg.resolve_alias(alias);
        // An alias is only advertised if it leads somewhere real.
        if let Some(existing) = entries.iter().find(|e| e.id == resolved) {
            entries.push(ModelEntry {
                id: alias.clone(),
                upstreams: existing.upstreams.clone(),
                pool_members: existing.pool_members.clone(),
                alias_for: Some(target.clone()),
                description: None,
            });
        }
    }

    entries.sort_by(|a, b| a.id.cmp(&b.id));
    entries.dedup_by(|a, b| a.id == b.id);
    entries
}

fn to_openai(entry: &ModelEntry, created: u64) -> Value {
    let mut v = json!({
        "id": entry.id,
        "object": "model",
        "created": created,
        "owned_by": entry.owned_by(),
    });
    decorate(&mut v, entry);
    v
}

fn to_anthropic(entry: &ModelEntry, created: u64) -> Value {
    let mut v = json!({
        "id": entry.id,
        "type": "model",
        "display_name": entry.display_name(),
        "created_at": rfc3339(created),
    });
    decorate(&mut v, entry);
    v
}

/// mini-router's own extension fields. Both SDKs ignore what they do not know,
/// and a human debugging a shelf of providers wants all of this.
fn decorate(v: &mut Value, entry: &ModelEntry) {
    if !entry.upstreams.is_empty() {
        v["upstreams"] = json!(entry.upstreams);
    }
    if !entry.pool_members.is_empty() {
        v["pool_members"] = json!(entry.pool_members);
    }
    if let Some(a) = &entry.alias_for {
        v["alias_for"] = json!(a);
    }
}

/// Seconds since the epoch as an RFC 3339 timestamp, which is what the
/// Anthropic catalogue uses.
fn rfc3339(secs: u64) -> String {
    // Civil-from-days, so we do not pull in a date crate for one field.
    let days = (secs / 86_400) as i64;
    let tod = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        y,
        m,
        d,
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    )
}

/// `GET /v1/models`, in the dialect the client asked in.
pub async fn list_models(state: &SharedState, protocol: Protocol) -> Response {
    let created = now_millis() / 1000;
    let entries = catalogue(state).await;
    let body = match protocol {
        Protocol::Openai => json!({
            "object": "list",
            "data": entries.iter().map(|e| to_openai(e, created)).collect::<Vec<_>>(),
        }),
        Protocol::Anthropic => {
            let data: Vec<Value> = entries.iter().map(|e| to_anthropic(e, created)).collect();
            json!({
                "data": data,
                "has_more": false,
                "first_id": entries.first().map(|e| e.id.clone()),
                "last_id": entries.last().map(|e| e.id.clone()),
            })
        }
    };
    json_response(StatusCode::OK, body)
}

/// `GET /v1/models/{id}`
pub async fn get_model(state: &SharedState, protocol: Protocol, id: &str) -> Response {
    let created = now_millis() / 1000;
    match catalogue(state).await.into_iter().find(|m| m.id == id) {
        Some(entry) => {
            let body = match protocol {
                Protocol::Openai => to_openai(&entry, created),
                Protocol::Anthropic => to_anthropic(&entry, created),
            };
            json_response(StatusCode::OK, body)
        }
        None => ApiError::new(
            ErrorKind::NotFound,
            format!("no provider currently serves model {id:?}"),
        )
        .into_dialect(protocol),
    }
}

fn json_response(status: StatusCode, body: Value) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_formats_known_instants() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_700_000_000), "2023-11-14T22:13:20Z");
        // A leap day, which is where hand-rolled date maths usually breaks.
        assert_eq!(rfc3339(1_709_164_800), "2024-02-29T00:00:00Z");
    }

    fn entry(id: &str) -> ModelEntry {
        ModelEntry {
            id: id.into(),
            upstreams: vec!["openai".into()],
            pool_members: vec![],
            alias_for: None,
            description: None,
        }
    }

    #[test]
    fn openai_and_anthropic_shapes_differ_where_they_should() {
        let e = entry("gpt-4o-mini");
        let o = to_openai(&e, 1_700_000_000);
        assert_eq!(o["object"], "model");
        assert_eq!(o["created"], 1_700_000_000u64);
        // A single-provider model is owned by that provider.
        assert_eq!(o["owned_by"], "openai");

        let a = to_anthropic(&e, 1_700_000_000);
        assert_eq!(a["type"], "model");
        assert_eq!(a["created_at"], "2023-11-14T22:13:20Z");
        assert_eq!(a["display_name"], "gpt-4o-mini");
    }

    #[test]
    fn a_pool_is_labelled_as_one() {
        let mut e = entry("fast");
        e.pool_members = vec!["openai:gpt-4o-mini".into(), "anthropic:claude".into()];
        let o = to_openai(&e, 0);
        assert_eq!(o["owned_by"], "mini-router-pool");
        assert_eq!(o["pool_members"][1], "anthropic:claude");
    }
}
