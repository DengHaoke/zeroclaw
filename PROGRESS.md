# Project Progress

> Last updated: 2026-04-27 16:55 KST | Branch: local/a2a-experiment | Latest commit: eac83a1a

## Current state summary

A minimal port of Google's A2A (Agent-to-Agent) protocol on top of the
post-crate-split ZeroClaw layout. Single commit `eac83a1a` adds an inbound
A2A server (Agent Card + JSON-RPC `message/send` / `tasks/get`) plus an
outbound `a2a_delegate` tool, with a typed peer registry under
`[a2a.peers.<name>]` for SSRF defense. End-to-end Slack→consumer→A2A→
producer→Slack delegation has been validated twice on the live infra-bot-dev
and aiops daemons. The second validation (Path C v2) uses
`[autonomy] level = "supervised"` + `auto_approve = ["file_read",
"a2a_delegate"]` on the consumer, which preserves the readonly-equivalent
safety story using existing ZeroClaw primitives — no further runtime patch
required to deploy this branch.

## Recent work log

| Date | What & why | Commit | Notes |
|------|------------|--------|-------|
| 2026-04-27 | Path C v2 validated end-to-end. Slack disambiguation test (TTS model names that exist only in aiops's KB) confirms real A2A delegation under the consumer's `level = "supervised"` + minimal `auto_approve` list. Reverse case (file_write request) auto-denied by `ApprovalManager`, proving the allowlist is tight. Aiops's own Slack remained silent throughout — confirms A2A inbound and Slack inbound are independent paths on the producer. | (no commit in this repo; configs live in agent-infra-bot-dev and agent-zeroclaw-aiops) | Replay procedure stored in Claude memory: `project_zeroclaw_a2a_path_c_replay.md` |
| 2026-04-27 | Path C v1 validated then rolled back. Used `level = "full"` on the consumer for the test, which violated the consumer repo's CLAUDE.md invariant #1. Investigation of `ApprovalManager` showed `Supervised + auto_approve` works in non-interactive Slack contexts (tools NOT in `auto_approve` are auto-denied because `request_approval()` returns `None`), so v2 obsoletes the earlier "need a SecurityPolicy whitelist patch" plan. | — | Key finding: no zeroclaw runtime patch needed for safe A2A deployment |
| 2026-04-27 | Initial commit pushed to `dhk:experiment/a2a-minimal-port`. Inbound server, outbound `a2a_delegate` tool with `send` / `discover` / `status` actions, in-memory `TaskStore` with LRU-terminal eviction (cap 10000), bearer auth via `constant_time_eq`, typed peer registry, and 14 unit tests passing. Tests 1 (unit), 2 (curl inbound), 3 (CLI consumer→producer) all green prior to this commit. | eac83a1a | See [docs/a2a-minimal-port-gaps.md](docs/a2a-minimal-port-gaps.md) for the deliberate-out-of-scope list vs the full A2A spec and vs PR #4166 |

## In progress

- [ ] Decision: whether to upstream this as a PR. Currently held — see "Next session TODO" for the gating items. Gating concerns are not blockers on the code itself, but on production-readiness signaling and the gh-account mismatch.

## Completed milestones

- [x] Inbound A2A server (`crates/zeroclaw-gateway/src/a2a.rs`) — Agent Card, `message/send`, `tasks/get`, REST `tasks/{id}` convenience, bearer auth
- [x] Outbound `a2a_delegate` tool (`crates/zeroclaw-tools/src/a2a.rs`) with `send` / `discover` / `status` actions
- [x] Schema additions for `[a2a]` and `[a2a.peers.<name>]` (`crates/zeroclaw-config/src/schema.rs`); `bearer_token` redacted in `Debug`
- [x] 14 unit tests passing (`cargo test -p zeroclaw-gateway -p zeroclaw-tools --lib a2a::`)
- [x] Test 1 (unit), Test 2 (curl inbound), Test 3 (CLI consumer→producer), Path C v1 (real Slack with `level = "full"`), Path C v2 (real Slack with `level = "supervised"` + `auto_approve` — readonly-equivalent)
- [x] Gap analysis at [docs/a2a-minimal-port-gaps.md](docs/a2a-minimal-port-gaps.md)

## Known issues / caveats

- **Branch is 33 commits behind `origin/master`.** Long-running feature branch — periodic rebase will be required if upstream lands changes that touch the gateway / runtime / tools / config crates. Last rebase included PR #5794 + PR #5992.
- **SSRF guard is string-prefix only** — does not resolve DNS, vulnerable to bypasses like `192-168-1-1.sslip.io`. Adequate for closed-trust same-host use; not adequate for public-internet peers. Hardening (DNS resolve + IP-range check + redirect-hop limits) is on the future-work roadmap in the gap doc.
- **Persistent task store not implemented** — in-memory `RwLock<HashMap>` only, tasks lost on daemon restart. Needed before async / long-running task scenarios.
- **`/webhook` cannot trigger `a2a_delegate`.** Pre-existing master limitation: `run_gateway_chat_simple` at `crates/zeroclaw-gateway/src/lib.rs:1340` hard-codes `tools: &[]`. Not introduced by this branch but blocks one obvious entry point. Workable entry points: `/ws/chat`, channel handlers (Slack / Discord / Telegram / etc.), `agent` CLI.
- **build-tree binary, not installed.** Consumer/producer must be run as `/home/dxr_agent/workspace/zeroclaw-a2a/target/release/zeroclaw daemon`. The `infra-bot-dev.service` unit in the sibling repo still points at `~/.cargo/bin/zeroclaw` (master, no A2A) — switching it requires a one-line `ExecStart` edit + `daemon-reload` + `restart`. Decision deferred.

## Next session TODO

1. **Decide on PR upstream.** Two viable pre-PR options:
   - Add 1–2 future-work items first (persistent task store, DNS-resolving SSRF) to lift this from "demo grade" to "production-considerable" before opening the PR.
   - Or comment on upstream issue #3566 with branch URL + gap doc link to gauge maintainer interest before committing to a PR.
   - Constraint: `gh` CLI on this host is logged in as `jinhy0417`, not `denghaoke` — any PR / issue write must be done manually via the web UI from the right account.
2. **Optional: switch `infra-bot-dev.service` ExecStart** to this build-tree binary so the dev bot runs the A2A-enabled daemon persistently (`sudo systemctl edit --full infra-bot-dev.service` → change `ExecStart` → `daemon-reload` → `restart`). Currently the dev bot has been Ctrl-C'd after manual Path C v2 testing.
3. **Optional: domain-routing rule** in `agent-infra-bot-dev/workspace/skills/knowledge-lookup/SKILL.md` Step 0 so the LLM autonomously delegates AI-model questions (TTS / STT / image / 3D / LoRA) to aiops, instead of needing an explicit `a2a_delegate` keyword in the user prompt.
4. **Optional: rebase against `origin/master`** to stay current. Branch is 33 commits behind as of this entry.

## Environment / dependency changes

- No new crate dependencies introduced. Reuses `reqwest`, `tokio`, `serde`, `constant_time_eq` already in the workspace.
- Config schema gains `[a2a]` and `[a2a.peers.<name>]` sections. All defaults are off; existing deployments are unaffected unless they opt in.
- `a2a_delegate` is registered ONLY when `a2a.enabled && !peers.is_empty()` — empty peer list means no tool exposure, avoiding misleading the LLM with an unusable tool.
- Consumer-side autonomy recommendation: `level = "supervised"` + minimal explicit `auto_approve` list including `a2a_delegate`. This pattern is documented inline in `agent-infra-bot-dev/config.toml` and `agent-infra-bot-dev/CLAUDE.md` invariant #1.

## Memory references

- `project_zeroclaw_a2a_minimal.md` — A2A minimal port summary (commit hash, branch layout, test status, PR-held reasoning, key design realization that Supervised + auto_approve obsoletes the whitelist-patch plan)
- `project_zeroclaw_a2a_path_c_replay.md` — step-by-step replay procedure for the Path C end-to-end test (config diffs, daemon commands, Slack disambiguation prompt, success signals, gotchas)
- `feedback_no_chinese_in_project_files.md` — project files (this PROGRESS.md included) must be English or Korean; never Chinese
- `feedback_workspace_containment.md` — no `/tmp` worktrees, no copies into `~/bin` or `~/.cargo/bin`; run zeroclaw binaries from this build tree
- `feedback_user_owns_verification.md` — present "what was built + how to test it"; let the user run the verification step
- `feedback_gh_account_mismatch.md` — `gh` CLI on this host posts as `jinhy0417`, not `denghaoke`; never use it for ZeroClaw PR / issue write operations
- `project_shared_user_bots.md` — multiple teammates run bots as `dxr_agent` on this host; never `pkill zeroclaw` broadly, always check cwd / `ZEROCLAW_CONFIG_DIR` first
