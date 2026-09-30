//! REST 只读端点（spec §4）：写操作一律走 WS。
//!
//! 鉴权：`--auth-token` 启用时要求 `?token=` 匹配，否则 401。

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::get;
use axum::Router;
use openslate_protocol::{AgentNodeDto, ConfigViewDto, SessionListItemDto};
use openslate_store_sqlite::query::RunRecord;
use serde::Deserialize;
use serde_json::json;

use crate::state::{build_config_view, AppState};

#[derive(Deserialize)]
pub struct TokenQuery {
    token: Option<String>,
}

/// token 校验：无 `--auth-token` 恒过；有则要求 `?token=` 严格相等。
fn token_ok(state: &Arc<AppState>, q: &TokenQuery) -> bool {
    match &state.auth_token {
        Some(expected) => q.token.as_deref() == Some(expected.as_str()),
        None => true,
    }
}

fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, "invalid or missing token").into_response()
}

async fn health(State(state): State<Arc<AppState>>) -> Response {
    Json(json!({
        "status": "ok",
        "proto": openslate_protocol::PROTOCOL_VERSION,
        "clients": state.hub.count(),
    }))
    .into_response()
}

async fn config(State(state): State<Arc<AppState>>, Query(q): Query<TokenQuery>) -> Response {
    if !token_ok(&state, &q) {
        return unauthorized();
    }
    let view = {
        let inner = state.core.lock();
        build_config_view(&inner)
    };
    Json(view).into_response()
}

async fn agents(State(state): State<Arc<AppState>>, Query(q): Query<TokenQuery>) -> Response {
    if !token_ok(&state, &q) {
        return unauthorized();
    }
    let tree = {
        let inner = state.core.lock();
        AgentNodeDto::from(&inner.agent_tree)
    };
    Json(tree).into_response()
}

async fn skills(State(state): State<Arc<AppState>>, Query(q): Query<TokenQuery>) -> Response {
    if !token_ok(&state, &q) {
        return unauthorized();
    }
    let skills = {
        let inner = state.core.lock();
        inner.skills.clone()
    };
    Json(skills).into_response()
}

async fn sessions(State(state): State<Arc<AppState>>, Query(q): Query<TokenQuery>) -> Response {
    if !token_ok(&state, &q) {
        return unauthorized();
    }
    let store = { state.core.lock().store.clone() };
    let Some(store) = store else {
        return Json(Vec::<SessionListItemDto>::new()).into_response();
    };
    match store.list_runs(100, 0).await {
        Ok(runs) => {
            let items: Vec<SessionListItemDto> = runs
                .into_iter()
                .map(|r: RunRecord| SessionListItemDto {
                    id: r.id,
                    title: r.title,
                    root_agent_id: r.root_agent_id,
                    status: r.status,
                    started_at: r.started_at,
                    finished_at: r.finished_at,
                    cost_usd: r.cost_usd,
                })
                .collect();
            Json(items).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("store query failed: {e}"),
        )
            .into_response(),
    }
}

/// 只读 REST 路由（挂 /api 下）。
pub fn rest_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/health", get(health))
        .route("/config", get(config))
        .route("/agents", get(agents))
        .route("/skills", get(skills))
        .route("/sessions", get(sessions))
}

/// ConfigViewDto 直出辅助（lib::print_startup 提示用）。
pub fn config_view_json(view: &ConfigViewDto) -> String {
    serde_json::to_string_pretty(view).unwrap_or_default()
}
