//! # A2A (Agent-to-Agent) Protocol — Minimal MVP
//!
//! Implements a minimal subset of the Google A2A protocol so that two
//! ZeroClaw agents can talk to each other over HTTP+JSON-RPC 2.0.
//!
//! ## Implemented
//!
//! - `GET /.well-known/agent.json` — Agent Card discovery
//! - `POST /a2a/v1/rpc` — JSON-RPC 2.0 endpoint, methods:
//!   - `message/send` — synchronous: dispatch to the agent loop, return the
//!     completed task + artifact
//!   - `tasks/get` — poll status of a previously submitted task
//! - Bearer token authentication (constant-time comparison)
//! - Bounded in-memory task store
//!
//! ## Out of scope for this build
//!
//! See `docs/a2a-minimal-port-gaps.md` for the full gap list. Short version:
//! no `message/stream` SSE, no `tasks/cancel`, no `input-required`, no push
//! notifications, no binary artifacts, no persistence across restart.

use crate::AppState;
use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use zeroclaw_config::pairing::constant_time_eq;

/// Ceiling on in-memory tasks to prevent unbounded growth on a long-lived daemon.
/// Oldest completed tasks get evicted when this is hit.
const MAX_TASKS: usize = 10_000;

// ── Task state ───────────────────────────────────────────────────

/// In-memory task store shared via `AppState`.
pub struct TaskStore {
    tasks: RwLock<HashMap<String, TaskRecord>>,
}

impl TaskStore {
    pub fn new() -> Self {
        Self {
            tasks: RwLock::new(HashMap::new()),
        }
    }
}

impl Default for TaskStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Server-side record of an inbound task.
#[derive(Debug, Clone, Serialize)]
pub struct TaskRecord {
    pub id: String,
    pub status: TaskStatus,
    /// Response artifacts; populated on completion.
    pub artifacts: Vec<Value>,
    /// Unix seconds — used for LRU eviction.
    pub updated_at: u64,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TaskStatus {
    Working,
    Completed,
    Failed,
}

// ── JSON-RPC wire format ─────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct JsonRpcRequest {
    #[serde(default = "default_jsonrpc_version")]
    pub jsonrpc: String,
    pub method: String,
    #[serde(default)]
    pub params: Value,
    #[serde(default)]
    pub id: Value,
}

fn default_jsonrpc_version() -> String {
    "2.0".to_string()
}

fn rpc_error(id: Value, code: i32, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    })
}

// ── Agent Card ───────────────────────────────────────────────────

/// Build the Agent Card JSON advertised at `/.well-known/agent.json`.
/// Spec reference: <https://google.github.io/A2A/#agent-card>
fn build_agent_card(cfg: &zeroclaw_config::schema::A2aConfig) -> Value {
    let name = cfg
        .agent_name
        .clone()
        .unwrap_or_else(|| "zeroclaw-agent".to_string());
    let description = cfg
        .description
        .clone()
        .unwrap_or_else(|| "ZeroClaw agent exposed via A2A.".to_string());
    let url = cfg
        .public_url
        .clone()
        .unwrap_or_else(|| "http://127.0.0.1/".to_string());
    let version = cfg
        .version
        .clone()
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());

    let skills: Vec<Value> = cfg
        .capabilities
        .iter()
        .map(|cap| {
            json!({
                "id": cap,
                "name": cap,
                "description": format!("{name} supports capability: {cap}"),
                "tags": [cap],
            })
        })
        .collect();

    json!({
        "name": name,
        "description": description,
        "url": url,
        "version": version,
        "protocolVersion": "0.2",
        "capabilities": {
            "streaming": false,
            "pushNotifications": false,
        },
        "skills": skills,
        "defaultInputModes": ["text"],
        "defaultOutputModes": ["text"],
    })
}

pub async fn handle_agent_card(State(state): State<AppState>) -> impl IntoResponse {
    let cfg = state.config.lock().a2a.clone();
    if !cfg.enabled {
        return (StatusCode::NOT_FOUND, Json(json!({"error": "A2A disabled"})));
    }
    (StatusCode::OK, Json(build_agent_card(&cfg)))
}

// ── Bearer auth ──────────────────────────────────────────────────

/// Returns `Ok(())` if the Authorization header matches the configured token,
/// or if no token is configured (auth disabled). `Err(status)` on failure.
fn check_bearer(headers: &HeaderMap, expected: Option<&str>) -> Result<(), StatusCode> {
    let Some(expected) = expected else {
        return Ok(());
    };
    let auth = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let token = auth.strip_prefix("Bearer ").unwrap_or("");
    if constant_time_eq(token, expected) {
        Ok(())
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

// ── RPC dispatch ─────────────────────────────────────────────────

pub async fn handle_rpc(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<JsonRpcRequest>,
) -> (StatusCode, Json<Value>) {
    let cfg = state.config.lock().a2a.clone();

    if !cfg.enabled {
        return (
            StatusCode::NOT_FOUND,
            Json(rpc_error(req.id, -32601, "A2A disabled")),
        );
    }

    if let Err(status) = check_bearer(&headers, cfg.bearer_token.as_deref()) {
        return (
            status,
            Json(rpc_error(req.id.clone(), -32001, "Unauthorized")),
        );
    }

    match req.method.as_str() {
        "message/send" => handle_message_send(&state, req).await,
        "tasks/get" => handle_tasks_get(&state.a2a_tasks, req).await,
        other => {
            let err = format!("Method not found: {other}");
            (StatusCode::OK, Json(rpc_error(req.id, -32601, &err)))
        }
    }
}

// ── message/send ─────────────────────────────────────────────────

/// Extract the text content from a spec `message/send` params block.
/// Falls back to a plain `params.message` string for simple clients.
fn extract_text(params: &Value) -> Option<String> {
    // Spec path: params.message.parts[*].text where kind == "text"
    if let Some(parts) = params.pointer("/message/parts").and_then(|p| p.as_array()) {
        let joined: String = parts
            .iter()
            .filter_map(|p| {
                let kind = p.get("kind").and_then(|k| k.as_str()).unwrap_or("text");
                if kind == "text" {
                    p.get("text").and_then(|t| t.as_str())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join(" ");
        if !joined.trim().is_empty() {
            return Some(joined);
        }
    }
    // Simple fallback
    params.get("message").and_then(|v| v.as_str()).map(String::from)
}

async fn handle_message_send(
    state: &AppState,
    req: JsonRpcRequest,
) -> (StatusCode, Json<Value>) {
    let Some(text) = extract_text(&req.params) else {
        return (
            StatusCode::OK,
            Json(rpc_error(req.id, -32602, "Invalid params: missing message text")),
        );
    };

    let task_id = uuid::Uuid::new_v4().to_string();
    let now = unix_now();

    // Reserve a slot in the task store. Evict oldest completed if we're at cap.
    {
        let mut tasks = state.a2a_tasks.tasks.write().await;
        if tasks.len() >= MAX_TASKS {
            evict_oldest_terminal(&mut tasks);
            if tasks.len() >= MAX_TASKS {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(rpc_error(
                        req.id,
                        -32000,
                        "Task store full — too many in-flight tasks",
                    )),
                );
            }
        }
        tasks.insert(
            task_id.clone(),
            TaskRecord {
                id: task_id.clone(),
                status: TaskStatus::Working,
                artifacts: vec![],
                updated_at: now,
            },
        );
    }

    // Dispatch synchronously to the runtime agent loop.
    let config = state.config.lock().clone();
    let session_id = format!("a2a-{task_id}");
    let result =
        zeroclaw_runtime::agent::loop_::process_message(config, &text, Some(&session_id)).await;

    match result {
        Ok(response) => {
            let artifact = json!({
                "artifactId": uuid::Uuid::new_v4().to_string(),
                "name": "response",
                "parts": [{ "kind": "text", "text": response }]
            });
            {
                let mut tasks = state.a2a_tasks.tasks.write().await;
                if let Some(t) = tasks.get_mut(&task_id) {
                    t.status = TaskStatus::Completed;
                    t.artifacts = vec![artifact.clone()];
                    t.updated_at = unix_now();
                }
            }
            (
                StatusCode::OK,
                Json(json!({
                    "jsonrpc": "2.0",
                    "id": req.id,
                    "result": {
                        "id": task_id,
                        "status": { "state": "completed" },
                        "artifacts": [artifact]
                    }
                })),
            )
        }
        Err(e) => {
            tracing::error!(task_id = %task_id, error = %e, "A2A message/send agent-loop failure");
            {
                let mut tasks = state.a2a_tasks.tasks.write().await;
                if let Some(t) = tasks.get_mut(&task_id) {
                    t.status = TaskStatus::Failed;
                    t.updated_at = unix_now();
                }
            }
            (
                StatusCode::OK,
                Json(json!({
                    "jsonrpc": "2.0",
                    "id": req.id,
                    "result": {
                        "id": task_id,
                        "status": { "state": "failed", "message": "Internal processing error" }
                    }
                })),
            )
        }
    }
}

// ── tasks/get ────────────────────────────────────────────────────

async fn handle_tasks_get(
    store: &Arc<TaskStore>,
    req: JsonRpcRequest,
) -> (StatusCode, Json<Value>) {
    let task_id = req.params.get("id").and_then(|v| v.as_str()).unwrap_or("");
    if task_id.is_empty() {
        return (
            StatusCode::OK,
            Json(rpc_error(req.id, -32602, "Invalid params: missing task id")),
        );
    }
    let tasks = store.tasks.read().await;
    match tasks.get(task_id) {
        Some(t) => (
            StatusCode::OK,
            Json(json!({
                "jsonrpc": "2.0",
                "id": req.id,
                "result": {
                    "id": t.id,
                    "status": { "state": t.status },
                    "artifacts": t.artifacts
                }
            })),
        ),
        None => (
            StatusCode::OK,
            Json(rpc_error(req.id, -32001, "Task not found")),
        ),
    }
}

// ── Optional REST helper: GET /a2a/v1/tasks/{id} ─────────────────

/// Convenience REST handler for easy polling from a shell (curl). Bypasses
/// JSON-RPC envelope; useful for humans, not part of the A2A spec proper.
pub async fn handle_task_get_rest(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(task_id): Path<String>,
) -> (StatusCode, Json<Value>) {
    let cfg = state.config.lock().a2a.clone();
    if !cfg.enabled {
        return (StatusCode::NOT_FOUND, Json(json!({"error": "A2A disabled"})));
    }
    if let Err(status) = check_bearer(&headers, cfg.bearer_token.as_deref()) {
        return (status, Json(json!({"error": "unauthorized"})));
    }
    let tasks = state.a2a_tasks.tasks.read().await;
    match tasks.get(&task_id) {
        Some(t) => (StatusCode::OK, Json(serde_json::to_value(t).unwrap_or_default())),
        None => (StatusCode::NOT_FOUND, Json(json!({"error": "task not found"}))),
    }
}

// ── Eviction + time ──────────────────────────────────────────────

fn evict_oldest_terminal(tasks: &mut HashMap<String, TaskRecord>) {
    let victim = tasks
        .iter()
        .filter(|(_, t)| !matches!(t.status, TaskStatus::Working))
        .min_by_key(|(_, t)| t.updated_at)
        .map(|(k, _)| k.clone());
    if let Some(k) = victim {
        tasks.remove(&k);
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

// ── Tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_text_from_spec_message() {
        let p = json!({
            "message": { "parts": [
                { "kind": "text", "text": "hello " },
                { "kind": "text", "text": "world" }
            ]}
        });
        assert_eq!(extract_text(&p).as_deref(), Some("hello  world"));
    }

    #[test]
    fn extract_text_falls_back_to_simple_message() {
        let p = json!({ "message": "plain text" });
        assert_eq!(extract_text(&p).as_deref(), Some("plain text"));
    }

    #[test]
    fn extract_text_empty_returns_none() {
        let p = json!({});
        assert_eq!(extract_text(&p), None);
    }

    #[test]
    fn agent_card_has_required_fields() {
        let cfg = zeroclaw_config::schema::A2aConfig {
            enabled: true,
            agent_name: Some("aiops".into()),
            description: Some("AI knowledge".into()),
            public_url: Some("http://example.test".into()),
            version: Some("0.1.0".into()),
            capabilities: vec!["tts".into(), "stt".into()],
            ..Default::default()
        };
        let card = build_agent_card(&cfg);
        assert_eq!(card["name"], "aiops");
        assert_eq!(card["description"], "AI knowledge");
        assert_eq!(card["url"], "http://example.test");
        assert_eq!(card["version"], "0.1.0");
        assert_eq!(card["skills"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn bearer_auth_rejects_wrong_token() {
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer wrong".parse().unwrap(),
        );
        assert!(check_bearer(&h, Some("right")).is_err());
    }

    #[test]
    fn bearer_auth_accepts_correct_token() {
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer s3cret".parse().unwrap(),
        );
        assert!(check_bearer(&h, Some("s3cret")).is_ok());
    }

    #[test]
    fn bearer_auth_noop_when_unset() {
        let h = HeaderMap::new();
        assert!(check_bearer(&h, None).is_ok());
    }

    #[tokio::test]
    async fn evict_oldest_terminal_removes_completed() {
        let mut tasks = HashMap::new();
        tasks.insert(
            "old-done".into(),
            TaskRecord {
                id: "old-done".into(),
                status: TaskStatus::Completed,
                artifacts: vec![],
                updated_at: 1,
            },
        );
        tasks.insert(
            "working".into(),
            TaskRecord {
                id: "working".into(),
                status: TaskStatus::Working,
                artifacts: vec![],
                updated_at: 0,
            },
        );
        evict_oldest_terminal(&mut tasks);
        assert!(tasks.contains_key("working"));
        assert!(!tasks.contains_key("old-done"));
    }
}
