//! Model aggregation: one catalogue across every upstream.
//!
//! This is the "aggregator" half of mini-router. A client asks the router what
//! it can run, and gets the union of what the shelf can run -- plus any alias
//! you defined, so an app hard-coded to `gpt-3.5-turbo` finds something to
//! talk to.

use std::collections::BTreeMap;

use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

use crate::auth;
use crate::error::{ApiError, ErrorKind};
use crate::state::SharedState;
use crate::util::now_millis;

#[derive(Debug, Clone, Serialize)]
pub struct ModelEntry {
    pub id: String,
    pub object: &'static str,
    pub created: u64,
    pub owned_by: String,
    /// Upstreams that can serve this model. A mini-router extension; OpenAI
    /// clients ignore it, humans debugging a shelf of boards do not.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub upstreams: Vec<String>,
    /// Set when this entry exists because of an `[alias]` mapping.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias_for: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelList {
    pub object: &'static str,
    pub data: Vec<ModelEntry>,
}

/// Build the catalogue: every model an in-rotation upstream reports, plus the
/// configured aliases that resolve onto one of them.
pub async fn catalogue(state: &SharedState) -> Vec<ModelEntry> {
    let created = now_millis() / 1000;
    let mut by_model: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for up in &state.upstreams {
        if !up.is_available() {
            continue;
        }
        for m in up.models().await {
            by_model.entry(m).or_default().push(up.name.clone());
        }
    }

    let mut entries: Vec<ModelEntry> = by_model
        .iter()
        .map(|(id, ups)| ModelEntry {
            id: id.clone(),
            object: "model",
            created,
            owned_by: "mini-router".into(),
            upstreams: ups.clone(),
            alias_for: None,
        })
        .collect();

    for (alias, target) in &state.cfg.alias {
        let resolved = state.cfg.resolve_alias(alias);
        if by_model.contains_key(alias) {
            continue;
        }
        if let Some(ups) = by_model.get(resolved) {
            entries.push(ModelEntry {
                id: alias.clone(),
                object: "model",
                created,
                owned_by: "mini-router".into(),
                upstreams: ups.clone(),
                alias_for: Some(target.clone()),
            });
        }
    }
    entries.sort_by(|a, b| a.id.cmp(&b.id));
    entries
}

/// `GET /v1/models`
pub async fn list_models(
    State(state): State<SharedState>,
    headers: axum::http::HeaderMap,
) -> Response {
    if let Err(e) = auth::authorize(&state, &headers) {
        return e.into_response();
    }
    Json(ModelList {
        object: "list",
        data: catalogue(&state).await,
    })
    .into_response()
}

/// `GET /v1/models/{id}`
pub async fn get_model(
    State(state): State<SharedState>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(e) = auth::authorize(&state, &headers) {
        return e.into_response();
    }
    match catalogue(&state).await.into_iter().find(|m| m.id == id) {
        Some(entry) => Json(entry).into_response(),
        None => ApiError::new(
            ErrorKind::NotFound,
            format!("no upstream currently serves model {id:?}"),
        )
        .into_response(),
    }
}
