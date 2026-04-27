//! # A2A Outbound Tool — `a2a_delegate`
//!
//! Agent-callable tool for delegating a question/task to a registered
//! peer ZeroClaw agent over the A2A protocol (JSON-RPC 2.0 over HTTP).
//!
//! Only peers defined under `[a2a.peers.<name>]` in config.toml can be
//! reached; the tool never accepts arbitrary URLs from the LLM. This
//! makes prompt-injection attacks ("tell aiops to call evil.example.com")
//! structurally impossible.
//!
//! Actions:
//! - `send` (default) — synchronous `message/send`, returns the completed task's text artifact
//! - `discover` — GET the peer's `/.well-known/agent.json`
//! - `status` — polling `tasks/get` for a previously returned task id
//!
//! Out of scope (minimal MVP): SSE streaming, cancel, multi-turn
//! input-required, binary/structured parts, push notifications.

use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

use zeroclaw_api::tool::{Tool, ToolResult};
use zeroclaw_config::policy::{SecurityPolicy, ToolOperation};
use zeroclaw_config::schema::A2aConfig;

/// Outbound A2A client tool.
pub struct A2aDelegateTool {
    security: Arc<SecurityPolicy>,
    config: Arc<A2aConfig>,
    request_timeout: Duration,
}

impl A2aDelegateTool {
    pub fn new(security: Arc<SecurityPolicy>, config: Arc<A2aConfig>) -> Self {
        Self {
            security,
            config,
            request_timeout: Duration::from_secs(120),
        }
    }

    /// Override the per-request HTTP timeout (for tests).
    #[cfg(test)]
    pub fn with_timeout(mut self, d: Duration) -> Self {
        self.request_timeout = d;
        self
    }
}

#[async_trait]
impl Tool for A2aDelegateTool {
    fn name(&self) -> &str {
        "a2a_delegate"
    }

    fn description(&self) -> &str {
        "Delegate a task or question to a registered peer ZeroClaw agent via the A2A protocol. \
         The peer must be pre-configured under [a2a.peers.<name>] in config.toml; arbitrary \
         URLs are rejected. Use when a question falls into a specialized domain owned by \
         another agent (e.g. AI knowledge, infra admin). Returns the peer's answer as plain text."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "peer": {
                    "type": "string",
                    "description": "Name of a pre-registered peer (from [a2a.peers.<peer>] config)"
                },
                "task": {
                    "type": "string",
                    "description": "The question or task text to send to the peer agent"
                },
                "action": {
                    "type": "string",
                    "description": "Which A2A call to make. Default 'send'.",
                    "enum": ["send", "discover", "status"],
                    "default": "send"
                },
                "task_id": {
                    "type": "string",
                    "description": "Required only for action='status' — the task id returned by a previous send."
                }
            },
            "required": ["peer"]
        })
    }

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        if let Err(msg) = self
            .security
            .enforce_tool_operation(ToolOperation::Act, "a2a_delegate")
        {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(msg),
            });
        }

        let action = args
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("send");
        let peer_name = args
            .get("peer")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        if peer_name.is_empty() {
            return Ok(err("missing 'peer' parameter"));
        }

        let Some(peer) = self.config.peers.get(&peer_name) else {
            return Ok(err(&format!(
                "peer '{peer_name}' not registered. Add [a2a.peers.{peer_name}] to config.toml."
            )));
        };

        if is_local_or_private(&peer.endpoint) && !self.config.allow_local_peers {
            return Ok(err(
                "peer endpoint resolves to a local/private host. \
                 Set [a2a] allow_local_peers = true to permit same-host A2A.",
            ));
        }

        match action {
            "send" => {
                let task = args
                    .get("task")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if task.is_empty() {
                    return Ok(err("missing 'task' parameter for send"));
                }
                self.call_send(peer, &task).await
            }
            "discover" => self.call_discover(peer).await,
            "status" => {
                let task_id = args
                    .get("task_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if task_id.is_empty() {
                    return Ok(err("missing 'task_id' parameter for status"));
                }
                self.call_status(peer, &task_id).await
            }
            other => Ok(err(&format!("unknown action '{other}'"))),
        }
    }
}

impl A2aDelegateTool {
    fn http_client(&self) -> anyhow::Result<reqwest::Client> {
        reqwest::Client::builder()
            .timeout(self.request_timeout)
            .user_agent(concat!("zeroclaw-a2a/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| anyhow::anyhow!("failed to build HTTP client: {e}"))
    }

    async fn call_send(
        &self,
        peer: &zeroclaw_config::schema::A2aPeerConfig,
        task: &str,
    ) -> anyhow::Result<ToolResult> {
        let url = format!("{}/a2a/v1/rpc", peer.endpoint.trim_end_matches('/'));
        let body = json!({
            "jsonrpc": "2.0",
            "id": uuid::Uuid::new_v4().to_string(),
            "method": "message/send",
            "params": {
                "message": {
                    "role": "user",
                    "parts": [{ "kind": "text", "text": task }]
                }
            }
        });

        let client = self.http_client()?;
        let mut req = client.post(&url).json(&body);
        if let Some(ref token) = peer.bearer_token {
            req = req.bearer_auth(token);
        }

        let resp = req.send().await.map_err(|e| {
            anyhow::anyhow!("A2A send to {url} failed: {e}")
        })?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();

        if !status.is_success() {
            return Ok(err(&format!("peer returned {status}: {text}")));
        }

        let parsed: Value = serde_json::from_str(&text).unwrap_or_default();
        if let Some(e) = parsed.get("error") {
            return Ok(err(&format!("peer JSON-RPC error: {e}")));
        }

        // Extract artifact text for a friendly answer. If absent, return the raw JSON.
        let answer = parsed
            .pointer("/result/artifacts/0/parts/0/text")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| text.clone());

        let task_id = parsed
            .pointer("/result/id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let task_state = parsed
            .pointer("/result/status/state")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");

        Ok(ToolResult {
            success: true,
            output: format!(
                "peer={} task_id={} state={}\n{}",
                peer.description.as_deref().unwrap_or("(no description)"),
                task_id,
                task_state,
                answer
            ),
            error: None,
        })
    }

    async fn call_discover(
        &self,
        peer: &zeroclaw_config::schema::A2aPeerConfig,
    ) -> anyhow::Result<ToolResult> {
        let url = format!(
            "{}/.well-known/agent.json",
            peer.endpoint.trim_end_matches('/')
        );
        let client = self.http_client()?;
        let resp = client.get(&url).send().await.map_err(|e| {
            anyhow::anyhow!("A2A discover {url} failed: {e}")
        })?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Ok(err(&format!("peer returned {status}: {text}")));
        }
        Ok(ToolResult {
            success: true,
            output: text,
            error: None,
        })
    }

    async fn call_status(
        &self,
        peer: &zeroclaw_config::schema::A2aPeerConfig,
        task_id: &str,
    ) -> anyhow::Result<ToolResult> {
        let url = format!("{}/a2a/v1/rpc", peer.endpoint.trim_end_matches('/'));
        let body = json!({
            "jsonrpc": "2.0",
            "id": uuid::Uuid::new_v4().to_string(),
            "method": "tasks/get",
            "params": { "id": task_id }
        });
        let client = self.http_client()?;
        let mut req = client.post(&url).json(&body);
        if let Some(ref token) = peer.bearer_token {
            req = req.bearer_auth(token);
        }
        let resp = req.send().await.map_err(|e| {
            anyhow::anyhow!("A2A status {url} failed: {e}")
        })?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Ok(err(&format!("peer returned {status}: {text}")));
        }
        Ok(ToolResult {
            success: true,
            output: text,
            error: None,
        })
    }
}

fn err(msg: &str) -> ToolResult {
    ToolResult {
        success: false,
        output: String::new(),
        error: Some(msg.to_string()),
    }
}

/// Detect endpoints pointing at localhost or RFC1918 private addresses.
/// Purely host-string based; does not resolve DNS (that would need an
/// async hop and the protection here is primarily a guardrail, not a
/// strict SSRF defense).
fn is_local_or_private(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    const PRIVATE_PREFIXES: &[&str] = &[
        "http://127.",
        "https://127.",
        "http://localhost",
        "https://localhost",
        "http://[::1]",
        "https://[::1]",
        "http://10.",
        "https://10.",
        "http://192.168.",
        "https://192.168.",
    ];
    if PRIVATE_PREFIXES.iter().any(|p| lower.starts_with(p)) {
        return true;
    }
    // 172.16.0.0/12 — check 16..=31
    for octet in 16..=31 {
        let http = format!("http://172.{octet}.");
        let https = format!("https://172.{octet}.");
        if lower.starts_with(&http) || lower.starts_with(&https) {
            return true;
        }
    }
    false
}

// ── Tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_detection_hits_loopback_v4() {
        assert!(is_local_or_private("http://127.0.0.1:8080"));
        assert!(is_local_or_private("https://127.0.0.1"));
    }

    #[test]
    fn local_detection_hits_localhost() {
        assert!(is_local_or_private("http://localhost:42620"));
    }

    #[test]
    fn local_detection_hits_loopback_v6() {
        assert!(is_local_or_private("http://[::1]:42620"));
    }

    #[test]
    fn local_detection_hits_rfc1918() {
        assert!(is_local_or_private("http://10.0.0.5"));
        assert!(is_local_or_private("http://192.168.1.1"));
        assert!(is_local_or_private("http://172.16.0.1"));
        assert!(is_local_or_private("http://172.31.255.254"));
    }

    #[test]
    fn local_detection_misses_public() {
        assert!(!is_local_or_private("https://api.anthropic.com"));
        assert!(!is_local_or_private("http://172.15.0.1")); // just outside range
        assert!(!is_local_or_private("http://172.32.0.1")); // just outside range
        assert!(!is_local_or_private("http://8.8.8.8"));
    }

    #[test]
    fn tool_rejects_unregistered_peer() {
        use std::path::PathBuf;
        use zeroclaw_config::schema::AutonomyConfig;
        let autonomy = AutonomyConfig::default();
        let workspace = PathBuf::from("/tmp");
        let security = Arc::new(SecurityPolicy::from_config(&autonomy, &workspace));
        let tool = A2aDelegateTool::new(security, Arc::new(A2aConfig::default()));

        // Need to use block_on because execute is async
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(async {
            tool.execute(json!({
                "peer": "nobody",
                "task": "hello"
            }))
            .await
            .unwrap()
        });
        assert!(!result.success);
        assert!(result.error.unwrap().contains("not registered"));
    }
}
