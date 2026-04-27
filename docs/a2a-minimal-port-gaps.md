# A2A Minimal Port — Feature Gaps

This document records what the current minimal A2A implementation **does**
and **does not** provide, compared to:

1. Google's full [A2A protocol spec](https://google.github.io/A2A/)
2. The abandoned upstream PR [#4166](https://github.com/zeroclaw-labs/zeroclaw/pull/4166) (5queezer/hrafn)
3. The open upstream feature request [#3566](https://github.com/zeroclaw-labs/zeroclaw/issues/3566)

The goal of this minimal build is to **demonstrate same-host A2A agent-to-agent
communication** (infra-bot ↔ aiops) as a technology exercise, not to ship a
production-complete A2A server. Everything listed under "deliberately out of
scope" is a known and intentional omission, not an oversight.

---

## What IS implemented (in this minimal port)

### Inbound server — `crates/zeroclaw-gateway/src/a2a.rs`

| Endpoint | Method | What it does |
|---|---|---|
| `GET /.well-known/agent.json` | — | Agent Card JSON (name / description / url / version / capabilities) |
| `POST /a2a/v1/rpc` | `message/send` | Synchronous: dispatches text to the agent loop (`zeroclaw_runtime::agent::loop_::process_message`), returns the completed task + text artifact in one request |
| `POST /a2a/v1/rpc` | `tasks/get` | Polls status of a task returned by a previous `message/send` |
| `GET /a2a/v1/tasks/{id}` | — | REST convenience variant of `tasks/get` (easier `curl` debugging, not in spec) |

- Bearer token auth via `Authorization: Bearer <token>`, **constant-time** comparison (`zeroclaw_config::pairing::constant_time_eq`) to avoid timing side-channels.
- In-memory `TaskStore` bounded to `MAX_TASKS = 10_000`; oldest terminal task gets evicted when full.
- When `[a2a] enabled = false`, all three endpoints return `404 A2A disabled`.

### Outbound tool — `crates/zeroclaw-tools/src/a2a.rs`

Agent-callable tool `a2a_delegate` with three actions:

| `action` | Purpose |
|---|---|
| `send` (default) | JSON-RPC `message/send` to a peer, returns the text artifact as the answer |
| `discover` | `GET {peer}/.well-known/agent.json` |
| `status` | JSON-RPC `tasks/get` for polling a task id |

Security:
- Peer must be pre-registered under `[a2a.peers.<name>]` in config.toml — **arbitrary URLs from the LLM are rejected**.
- Localhost / RFC1918 private addresses rejected unless `[a2a] allow_local_peers = true`.
- Tool is gated by `SecurityPolicy::enforce_tool_operation(ToolOperation::Act, ...)` like every other agent tool.
- Tool is only registered in the agent's tool list when **both** `a2a.enabled` and `!peers.is_empty()` — no empty-peer-list tool exposure that would mislead the LLM.

### Config schema — `crates/zeroclaw-config/src/schema.rs`

```toml
[a2a]
enabled = true                      # master switch; false → all endpoints 404
agent_name = "aiops"                # shows in agent card
description = "AI solution knowledge agent"
public_url = "http://127.0.0.1:42620"
bearer_token = "..."                # Authorization: Bearer <this> to call in
version = "0.7.3-a2a-mvp"
capabilities = ["ai-solution-knowledge"]
allow_local_peers = true            # needed for same-host demo

[a2a.peers.aiops]                   # outbound registry, keyed by friendly name
endpoint = "http://127.0.0.1:42620"
bearer_token = "..."
description = "AI solution knowledge agent"
```

`A2aConfig` uses a custom `Debug` impl that redacts `bearer_token`.

### Tests — 14 passing

- 8 in `crates/zeroclaw-gateway/src/a2a.rs` (text extraction, agent card, bearer auth, eviction)
- 6 in `crates/zeroclaw-tools/src/a2a.rs` (local-address detection variants, tool rejects unregistered peer)

Run: `cargo test -p zeroclaw-gateway -p zeroclaw-tools --lib a2a::`

---

## What is DELIBERATELY OUT OF SCOPE

### A2A spec features intentionally not implemented

| Feature | Why skipped |
|---|---|
| `message/stream` (SSE) | Our same-host demo completes in seconds — no streaming UX benefit. Full spec implementation needs SSE endpoint, chunked artifact delivery, session resume on disconnect. ~200 lines of extra server code. |
| `tasks/cancel` | No long-running async tasks exist yet — `message/send` is synchronous. Adding cancel requires decoupling task execution from the HTTP handler. |
| `input-required` multi-turn state (`contextId`) | We pass one text turn and return one answer. Multi-turn (agent asks clarifying Q, caller responds, continues) needs persistent context chaining across requests. |
| Push notifications | No outbound webhook support for callers who want async callbacks. |
| Structured or binary artifact parts (`data`, `raw`, `file`) | Only `kind: "text"` is handled. Binary attachments, structured JSON payloads, and file URIs would need MIME handling, size limits, and security review. |
| Persistent task store | In-memory only — tasks are lost on daemon restart. Production would use SQLite (already a zeroclaw dep) or the existing session backend. |
| Agent capabilities negotiation | Agent Card advertises static capability tags; no runtime negotiation of supported features. |
| DNS resolution + SSRF hardening | `is_local_or_private` is a string-prefix guard only. Does not resolve DNS, doesn't catch `192-168-1-1.sslip.io` style bypasses. Adequate for closed-trust same-host use; **not** adequate for public-internet peers. |

### Tool availability per gateway entry point (pre-existing, not introduced by this patch)

The gateway has two chat code paths and they expose different tool surfaces:

| Entry point | Code path | Tools available to LLM | Can trigger `a2a_delegate`? |
|---|---|---|---|
| `POST /webhook` | `run_gateway_chat_simple` (`tools: &[]` hard-coded) | None | No |
| `POST /a2a/v1/rpc` (this patch) | `process_message` | None (text-only artifact response) | No (inbound side has no need) |
| `GET /ws/chat` (WebSocket) | `run_gateway_chat_with_tools` | All registered | Yes |
| `POST /whatsapp` / `/linq` / `/wati` / `/nextcloud-talk` | `run_gateway_chat_with_tools` | All registered | Yes |
| `agent` CLI (`-m` or interactive) | `loop_::run` → `run_tool_call_loop` | All registered | Yes |
| Channel handlers (Slack / Discord / Telegram / etc.) | `run_gateway_chat_with_tools` via channel orchestrator | All registered | Yes |

So `a2a_delegate` cannot be triggered from a `POST /webhook` request — that limitation is unrelated to A2A and predates this patch (master decision to keep `/webhook` as a tool-less synchronous endpoint). Use `/ws/chat`, the agent CLI, or any channel handler instead.

### Features from PR #4166 (5queezer) intentionally NOT carried

These were in the original patch but **not ported** here:

| #4166 feature | Why not ported |
|---|---|
| Telegram group notifications for inbound A2A tasks | Specific to 5queezer's deployment; irrelevant to our infra-bot/aiops scenario |
| LAN peer discovery (#4643, closed) | Only useful for zero-config multi-host discovery; our peers are config-file registered |
| Onboard wizard A2A section | Config can be edited directly; saves one interactive wizard step and its translations |
| Full security hardening (redirect hop validation, DNS resolution, SSRF guard) | Simplified to string-prefix allow-local check; same-host trust model |
| 40 test suite from original PR | Those tests were for the monolithic old `src/` layout; the port uses the new crate layout and writes scope-appropriate tests (14) |

### Architectural choices that differ from #4166

| Concern | #4166 | This port |
|---|---|---|
| Crate location | `src/gateway/a2a.rs` + `src/tools/a2a.rs` (pre-crate-split layout) | `crates/zeroclaw-gateway/src/a2a.rs` + `crates/zeroclaw-tools/src/a2a.rs` (current layout) |
| Agent loop integration | `crate::agent::process_message(...)` (old monolithic path) | `zeroclaw_runtime::agent::loop_::process_message(...)` |
| Telegram coupling | Config field `a2a.notify_chat_id` directly embedded | Removed entirely |
| Peer registry | Unstructured | `[a2a.peers.<name>]` typed map; prevents LLM-arbitrary-URL |
| Task store | Bounded HashMap, per-commit refinement | Bounded HashMap + LRU-terminal eviction from the start |

---

## Future-work roadmap (if this is upstreamed or extended)

Short list of the commits / PRs that would turn this MVP into a proper A2A server, in rough priority order:

1. **Persistent task store** — swap `RwLock<HashMap>` for SQLite-backed store. Enables restart survival and audit.
2. **`message/stream` (SSE)** — for long agent turns where the caller wants token-level streaming.
3. **`tasks/cancel`** — requires async task execution (tokio spawned task with cancellation token), currently `message/send` is blocking.
4. **`input-required` multi-turn** — store pending context keyed by `contextId`, resume when caller sends follow-up.
5. **Proper SSRF hardening** — DNS resolve + IP-range check + redirect-hop limits. Critical before allowing non-localhost peers.
6. **Onboard wizard integration** — interactive `[a2a]` section in `zeroclaw onboard` wizard for first-time users.
7. **Agent card skill schemas** — currently just tags; spec allows per-skill input/output JSON schemas.
8. **Observability** — emit events via the gateway's existing observer for inbound A2A calls (track who/what/when).

A PR that lands 1–3 of these would make this suitable for multi-host / cross-organization use. Current scope is fine for single-host demos and for technology evaluation.

---

## How to enable this locally

### Producer side (aiops, serves knowledge)

`agent-zeroclaw-aiops/config.toml`:

```toml
[channels.slack]
enabled = false   # aiops no longer responds to Slack directly

[gateway]
host = "127.0.0.1"
port = 42620
# leave pairing/auth as-is — A2A has its own bearer below

[a2a]
enabled = true
agent_name = "aiops"
description = "AI solution knowledge agent"
public_url = "http://127.0.0.1:42620"
bearer_token = "CHOOSE_A_LONG_RANDOM_STRING"
capabilities = ["ai-solution-knowledge"]
```

Start with **the new binary directly from the build tree** so infra-bot's `~/.cargo/bin/zeroclaw` (and its `assistant.service`) is untouched. Nothing gets installed system-wide:

```bash
ZEROCLAW_CONFIG_DIR=/home/dxr_agent/workspace/agent-zeroclaw-aiops \
  /home/dxr_agent/workspace/zeroclaw-a2a/target/release/zeroclaw daemon
```

Verify:

```bash
curl http://127.0.0.1:42620/.well-known/agent.json
```

### Consumer side (infra-bot, delegates AI questions)

`agent-infra-bot/config.toml`:

```toml
[a2a]
enabled = false          # infra-bot does NOT run A2A server (no inbound)
allow_local_peers = true # but CAN call local peers (outbound tool)

[a2a.peers.aiops]
endpoint = "http://127.0.0.1:42620"
bearer_token = "<same random string as aiops>"
description = "AI solution knowledge agent"
```

infra-bot SKILL.md (or IDENTITY.md) routing rule:

```markdown
## 영역 라우팅

사용자 질문이 AI 모델(TTS / STT / 이미지 생성 / 3D / LoRA) 영역이면:
1. `a2a_delegate(peer="aiops", task=<user question>)` 호출
2. 응답 artifact 텍스트를 그대로 또는 요약해서 사용자에게 회신
3. 직접 추측 답변 금지 — aiops 가 권한 있는 출처
```

---

## Binary and source locations

- **Source worktree**: `/home/dxr_agent/workspace/zeroclaw-a2a/` on `local/a2a-experiment` branch, based on `origin/master` + PR #5794 + PR #5992 + changelog + A2A minimal implementation. (Parent repo: `/home/dxr_agent/workspace/zeroclaw/`.)
- **Release binary**: `/home/dxr_agent/workspace/zeroclaw-a2a/target/release/zeroclaw` — run **directly from the build tree**, no copy/install. The infra-bot binary at `/home/dxr_agent/.cargo/bin/zeroclaw` is left untouched.
- **Infra-bot impact**: zero. The `assistant.service` unit points at `~/.cargo/bin/zeroclaw` which is never overwritten. A2A only activates when an agent (e.g. aiops) is explicitly started against the new build-tree binary — this is opt-in.
