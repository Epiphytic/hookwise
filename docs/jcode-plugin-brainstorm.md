# Brainstorm: hookwise as a jcode Plugin - Plugin Architecture & Integration Design

**Date:** 2026-03-23
**Author:** Liam Helmer / Epiphytic
**Status:** Brainstorm (pre-decision)

## Problem Statement

hookwise is an intelligent permission gating system currently designed as a Claude Code plugin. It implements a 6-tier decision cascade (path policy -> cache -> Jaccard -> HNSW -> LLM supervisor -> HITL) that learns and caches permission decisions over time.

jcode is a Rust-native coding agent (v0.7.3) that is NOT Claude Code. It has no `hooks.json`, no `PreToolUse` hook system, and no `.claude-plugin/` infrastructure. jcode has its own tool execution loop (`Agent::execute_tool`), its own permission model (`[permissions]` in config.toml with `shell = "ask"`, `file_read = "allow"`, `file_write = "ask"`), its own session system, its own ambient mode with `request_permission` for headless operation, and its own notification channels (Telegram, Discord, ntfy, email).

We need to figure out three things:
1. **What plugin/extension primitives should jcode have at all?**
2. **How does hookwise map onto those primitives?**
3. **What infrastructure gaps exist in both projects?**

### Assumptions
- jcode source is not available to modify (closed-source binary), so plugin integration must work through jcode's existing external interfaces (config, debug socket, subagents, ambient tools) or through feature requests to the jcode author
- hookwise is fully open-source Rust and can be modified freely
- The goal is to bring hookwise's learned permission cascade to jcode's tool execution, not to rewrite jcode
- jcode already has a `SafetyConfig`, `ToolExecutionMode`, and `request_permission` tool - these are the seams we can hook into

---

## Part A: How Should Plugins Work in jcode?

### Option A1: Compiled Rust Crate Plugins (Dynamic Loading)

**Mechanism:** Plugins are compiled Rust crates that expose a standard trait (`JcodePlugin`). jcode loads them as dynamic libraries (`.so`/`.dylib`) at startup via `dlopen`. Each plugin implements lifecycle hooks: `on_before_tool`, `on_after_tool`, `on_session_start`, etc.

**Optimized for:** Performance, type safety, deep integration. Plugin code runs in-process with zero serialization overhead.

**Drawbacks:**
- ABI instability: Rust has no stable ABI. Plugins must be compiled with the exact same Rust toolchain version as jcode. A jcode update breaks all plugins.
- Distribution nightmare: users need a Rust toolchain to build plugins, or you need per-platform prebuilt binaries
- Security: in-process plugins can crash jcode or read its memory. No sandboxing.
- jcode is closed-source. The trait definitions would need to be published as a separate crate and kept in sync.

**Fit:** Poor for an external project like hookwise. This is the pattern for internal jcode extensions, not community plugins.

### Option A2: WASM Module Plugins (Component Model)

**Mechanism:** Plugins are WASM components compiled to `wasm32-wasip2` with WIT interfaces. jcode hosts a WASM runtime (wasmtime) and loads plugin components. The WIT interface defines the tool lifecycle hooks.

**Optimized for:** Sandboxing, portability, language independence. A plugin compiled on any OS runs everywhere. WASM capability model provides natural security boundaries.

**Drawbacks:**
- WASM Component Model is still maturing. WIT tooling is usable but rough.
- Performance overhead: WASM function calls have non-trivial overhead (~microseconds for boundary crossings). For a permission check that should be <100ns on cache hit, this matters.
- No network access from WASM without explicit capability grants. hookwise needs filesystem access (JSONL, indexes), network access (Unix sockets, API calls), and process spawning (fastembed ONNX runtime).
- fastembed/ONNX runtime cannot run inside WASM. The embedding tier would need to be a host-provided capability.
- Liam's stack includes GIRT (WASM tool factory), so there's existing infrastructure, but hookwise's requirements (filesystem, network, subprocess) push well beyond typical WASM sandboxing.

**Fit:** Architecturally appealing but practically difficult for hookwise specifically. The embedding tier and IPC requirements don't fit the WASM sandbox model without extensive host-function plumbing.

### Option A3: Subprocess Hooks (External Process Protocol)

**Mechanism:** Plugins are external binaries. jcode invokes them as subprocesses at defined hook points, passing context as JSON on stdin and reading decisions from JSON on stdout. This is exactly how Claude Code's hook system works, and it's how hookwise already operates.

**Optimized for:** Language independence, isolation, simplicity. Any binary that reads JSON stdin and writes JSON stdout is a plugin. Zero coupling to jcode internals.

**Drawbacks:**
- Process spawn overhead: ~1-5ms per fork+exec on Linux. For every tool call, this adds latency. hookwise's fast tiers (cache hit <100ns, Jaccard <500ns) are dominated by process startup cost.
- Startup amortization: hookwise already compiles regex sets, globsets, and loads indexes at startup. Paying this cost per-invocation is wasteful.
- State loss: each invocation is stateless. In-memory caches (DashMap sessions, compiled patterns) are rebuilt from disk every time.
- This is exactly hookwise's current model (it's a subprocess hook for Claude Code). The performance characteristics are known and acceptable for Claude Code's use case, but jcode may want tighter integration.

**Fit:** Highest compatibility, lowest integration effort. This is the "just make it work" option. hookwise already speaks this protocol (with a format adapter for jcode's JSON shape).

### Option A4: Long-Running Sidecar Daemon (Unix Socket IPC)

**Mechanism:** The plugin runs as a persistent daemon process, started when jcode starts and communicating via a Unix domain socket. jcode sends tool call metadata to the socket before execution and reads back allow/deny/ask. The daemon maintains in-memory state (caches, indexes, compiled patterns) across all tool calls.

**Optimized for:** Performance with persistence. Startup cost paid once. In-memory caches survive across tool calls. The daemon can also serve multiple jcode sessions simultaneously (swarm scenario).

**Drawbacks:**
- Lifecycle management: who starts the daemon? Who stops it? What happens if it crashes? jcode needs daemon supervision or the user needs to start it manually.
- Protocol design: need a well-defined request/response protocol over the socket. More engineering than subprocess stdio.
- hookwise already has a Unix socket server (`ipc/socket_server.rs`) for its supervisor agent. This could be repurposed.

**Fit:** Best performance characteristics for hookwise specifically. hookwise already has socket server infrastructure. The daemon model maps naturally to hookwise's "permission authority" role.

### Option A5: Config-Driven Wrappers (No Plugin System)

**Mechanism:** Instead of a plugin system, jcode's existing `[permissions]` config and `[safety]` config become more expressive. hookwise is just a pre-flight check that jcode's config points to:

```toml
[permissions]
shell = "external:hookwise check"
file_write = "external:hookwise check"
file_read = "allow"
```

jcode's tool execution loop sees `external:...` and shells out to the command before executing the tool.

**Optimized for:** Zero new infrastructure. Works with jcode as-is (or with a minimal config extension). No plugin API to design or maintain.

**Drawbacks:**
- Still subprocess overhead per invocation (same as A3)
- Very limited hook points - only the permissions check, not session lifecycle or post-tool events
- No way to inject context back into the LLM conversation (hookwise's session-check registration prompt needs to inject text that the LLM sees)

**Fit:** Minimum viable approach if jcode adds a single config feature (`external:command` permission mode). Could be a stepping stone to a richer plugin system.

### Option A6: Debug Socket Integration (Observe + Inject)

**Mechanism:** jcode has a debug socket that "broadcasts all TUI state changes" and accepts commands like `server:state`, `server:history`, `server:tools`, `server:message`, `server:tool execution`. hookwise connects to this socket, observes tool execution events, and injects allow/deny decisions back through the debug command interface.

**Optimized for:** Zero changes to jcode. Works with the binary as-is. The debug socket is an existing external API.

**Drawbacks:**
- The debug socket is a debug interface, not a production plugin API. It may not fire events at the right granularity (before tool execution starts, not after).
- Race conditions: by the time hookwise sees a tool execution event on the debug socket, the tool may already be running.
- No guarantee of stable API. Debug socket commands can change between jcode versions.
- The debug socket may not support blocking tool execution while waiting for a permission decision.

**Fit:** Clever hack, not a production architecture. Useful for prototyping and proving the concept before building real infrastructure.

---

## Part A: Unknowns
- [ ] Does jcode's `Agent::execute_tool` have any hook point where external code can intercept before execution?
- [ ] Can jcode's `[permissions]` config support external commands as decision sources?
- [ ] Does the debug socket fire events *before* tool execution or only *after*?
- [ ] What's the actual overhead of subprocess spawn on the target deployment environment?
- [ ] Is jcode's author open to adding a plugin/extension API? What form would they prefer?
- [ ] Does jcode's `SafetyConfig` struct have any extensibility beyond the current `shell`/`file_read`/`file_write` fields?

## Part A: Recommendation

**Primary: Option A4 (Long-Running Sidecar Daemon)** for production.
**Bootstrap: Option A5 (Config-Driven Wrapper)** as the minimum viable path.

The sidecar daemon model is the best fit because:
1. hookwise already has a Unix socket server/client implementation
2. In-memory caches (the entire point of the cascade) only work if the process persists
3. Multiple jcode sessions (swarm) can share a single hookwise daemon
4. The daemon can manage its own lifecycle (HNSW rebuilds, session registration, pending queue)

But building the daemon requires jcode to support calling out to a Unix socket for permission decisions. The minimum viable path is to get jcode to support `external:command` in its permissions config, prove the concept with subprocess calls, then optimize to a socket protocol.

---

## Part B: hookwise as a jcode Plugin - What Specifically Needs to Be Built

### Option B1: Subprocess Adapter (Minimal Changes)

**Mechanism:** Add a `Jcode` variant to hookwise's `HookFormat` enum. When invoked as `hookwise check --format jcode`, it reads jcode-formatted JSON from stdin and writes jcode-formatted JSON to stdout. jcode calls hookwise as a subprocess on each tool execution.

**Core mechanism:**
- New `HookFormat::Jcode` variant in `hook_io.rs`
- New `JcodeHookInput` struct matching jcode's tool call metadata
- New `JcodeHookOutput` struct matching what jcode expects back
- The cascade logic is unchanged - only the I/O serialization layer adapts

**What changes in hookwise:**
```rust
pub enum HookFormat {
    Claude,
    Gemini,
    Jcode,  // NEW
}
```

**jcode-specific input format** (needs discovery - what does jcode actually send?):
```json
{
  "session_id": "jcode-session-abc",
  "tool_name": "shell_exec",
  "tool_input": {"command": "cargo test", "description": "run tests"},
  "cwd": "/home/user/project",
  "role": "agent"
}
```

**jcode-specific output format:**
```json
{
  "decision": "allow",
  "reason": "cache hit: cargo test previously allowed for agent role"
}
```

**Optimized for:** Speed of development. Could be built in a day.

**Drawbacks:** Subprocess overhead per call. No persistent caches. Startup cost repeated.

**Fit:** Good for proving the concept. Inadequate for production performance.

### Option B2: Sidecar Daemon with jcode Socket Integration

**Mechanism:** hookwise runs as a long-lived daemon. A thin shim (either built into jcode or as a tiny subprocess) sends tool call metadata to hookwise's Unix socket and returns the decision to jcode's tool execution loop.

**What needs to exist:**
1. **hookwise daemon mode**: `hookwise serve` starts the daemon, loads all indexes, opens the permission socket
2. **jcode integration shim**: a tiny binary or script that jcode calls as a subprocess, which connects to hookwise's socket, sends the request, reads the response, and exits. Amortizes hookwise startup but still has subprocess overhead for the shim.
3. **Or: native jcode socket support**: jcode connects directly to hookwise's socket from its tool execution loop. Zero subprocess overhead. Requires jcode changes.

**Architecture:**
```
jcode tool execution loop
    |
    v
[permission check point]
    |
    v  (Unix socket: /tmp/hookwise-<project>.sock)
hookwise daemon
    |
    +-- path policy (compiled globsets, in-memory)
    +-- exact cache (HashMap, in-memory)
    +-- Jaccard (sorted token sets, in-memory)
    +-- HNSW (instant-distance index, in-memory)
    +-- LLM supervisor (API call or sub-agent)
    +-- HITL (pending queue)
    |
    v
decision response back to jcode
```

**Drawbacks:** Requires daemon lifecycle management. More complex deployment.

**Fit:** Production architecture. This is what we should build toward.

### Option B3: jcode Subagent as Supervisor

**Mechanism:** Instead of hookwise making its own LLM calls for the supervisor tier, it delegates to a jcode subagent. jcode spawns a "hookwise supervisor" subagent session that has the policy context, role definitions, and cached decisions in its system prompt. When hookwise's cascade reaches tier 3, it sends the request to this subagent via jcode's swarm communication system.

**What this changes:**
- hookwise's `SupervisorBackend` trait gets a new implementation: `JcodeSubagentSupervisor`
- The subagent is spawned via jcode's `communicate` tool with `action: "spawn"`
- Communication happens via jcode's inter-session messaging, not a raw Unix socket
- The supervisor agent inherits jcode's provider/model configuration, token budget, etc.

**Drawbacks:** Tightly couples to jcode's swarm system. Adds latency (message routing through jcode server). The supervisor loses the ability to be a standalone process.

**Fit:** Elegant for the LLM tier specifically, but doesn't help with tiers 0-2b which are pure Rust computation.

### Part B: Tool Name Mapping

jcode's tool names differ from Claude Code's:

| jcode tool | Claude Code equivalent | hookwise treatment |
|-----------|----------------------|-------------------|
| `shell_exec` | `Bash` | Command analysis, path extraction |
| `file_write` | `Write` | Path policy, file-level caching |
| `file_edit` | `Edit` | Path policy, file-level caching |
| `file_read` | `Read` | Path policy (read globs) |
| `file_grep` | `Grep` | Read-only, usually auto-allow |
| `file_glob` | `Glob` | Read-only, usually auto-allow |
| `task_runner` | `Task` | Subagent spawn, high-risk |
| `multiedit` | N/A (jcode-specific) | Path policy per edit target |
| `apply_patch` | N/A | Path policy per patched file |
| `communicate` | N/A (jcode swarm) | Swarm-specific rules needed |
| `schedule` | N/A (jcode ambient) | Ambient-specific rules needed |
| `request_permission` | N/A | Meta: hookwise gating a permission request tool |
| `memory` | N/A | Usually auto-allow, low risk |
| `webfetch` | N/A | URL-based policy possible |
| `websearch` | N/A | Usually auto-allow |

hookwise needs a tool name normalization layer. Option: a `[tool_aliases]` config section:

```yaml
tool_aliases:
  shell_exec: Bash
  file_write: Write
  file_edit: Edit
  file_read: Read
  file_grep: Grep
  file_glob: Glob
  task_runner: Task
  multiedit: Edit  # treat as multi-file edit
  apply_patch: Write  # treat as write
```

Or: hookwise natively understands both naming schemes and normalizes internally.

### Part B: jcode-Specific Metadata

jcode provides different metadata than Claude Code:

| Field | Claude Code | jcode | Notes |
|-------|------------|-------|-------|
| `session_id` | UUID string | UUID string | Compatible |
| `tool_name` | `Bash`, `Write`, etc. | `shell_exec`, `file_write`, etc. | Needs mapping |
| `tool_input` | JSON object | JSON object | Compatible (field names may differ) |
| `cwd` | Working directory | Working directory | Compatible |
| `permission_mode` | `"default"` | N/A | Not present in jcode |
| N/A | N/A | `role` (from swarm) | jcode swarm has roles via `assign_role` |
| N/A | N/A | `model` | Which LLM model is running |
| N/A | N/A | `provider` | Which provider (claude, openai, etc.) |

### Part B: HITL in jcode Context

This is the hardest problem. Claude Code has a terminal UI where it can prompt the user with `[a]llow / [d]eny`. jcode has three modes:

1. **Interactive TUI session**: User is at the terminal. Could potentially inject a prompt, but jcode's TUI is a custom terminal renderer, not a simple readline prompt.

2. **Headless/ambient mode**: No user present. jcode already solves this with `request_permission` tool, which queues a permission request for later review via `jcode permissions` command, Telegram, Discord, or email.

3. **Swarm worker**: Running as part of a coordinated agent team. No direct user access.

**HITL options for jcode:**

| Option | Interactive TUI | Headless/Ambient | Swarm |
|--------|----------------|-----------------|-------|
| **hookwise queue** (terminal) | User runs `hookwise queue` in another terminal | User runs `hookwise queue` when notified | Coordinator runs `hookwise queue` |
| **jcode's request_permission** | Inject via debug socket or swarm message | Natural fit - already exists | Natural fit for coordinator |
| **Notification channels** | Not needed | Telegram/Discord/ntfy | Telegram/Discord/ntfy |
| **hookwise pending queue file** | Poll-based | Poll-based with notifications | Poll-based |

**Recommendation:** hookwise should integrate with jcode's existing notification channels rather than building its own. When a HITL decision is needed:
1. Write to hookwise's pending queue (existing behavior)
2. Also send a notification via jcode's configured channels (Telegram, Discord, ntfy)
3. The notification includes the decision context and a way to respond
4. For Telegram/Discord: hookwise could listen for reply messages as decisions

### Part B: Minimum Viable Integration

The smallest thing that proves the concept:

1. **hookwise check --format jcode**: read jcode-shaped JSON from stdin, run the cascade, write jcode-shaped JSON to stdout
2. **A wrapper script** that jcode can call as an external permission check:
   ```bash
   #!/bin/bash
   # hookwise-jcode-shim.sh
   # Translates jcode's tool call into hookwise's expected format
   echo "$JCODE_TOOL_CALL_JSON" | hookwise check --format jcode
   ```
3. **jcode config** pointing to the shim:
   ```toml
   [permissions]
   shell = "external:hookwise check --format jcode"
   file_write = "external:hookwise check --format jcode"
   ```
4. **Test with a single session** doing file writes and shell commands, verifying the cascade resolves decisions correctly.

This requires:
- ~100 lines of Rust in hookwise (new format variant + I/O types)
- jcode supporting `external:command` in permissions config (feature request)
- A test harness that simulates jcode tool calls

---

## Part C: Infrastructure Gaps

### Gap 1: LLM Supervisor for jcode

**Option C1-a: Direct API call from hookwise**
hookwise calls the Anthropic API (or any LLM) directly using its existing `ApiSupervisor` backend. The API key comes from environment or hookwise config.

- Pro: Independent of jcode. Works when jcode is not running.
- Con: Separate API key / billing. Doesn't benefit from jcode's provider routing, token budget tracking, or multi-provider fallback.

**Option C1-b: jcode subagent via swarm**
hookwise asks jcode to spawn a subagent for the supervisor evaluation. Uses jcode's `communicate` tool.

- Pro: Reuses jcode's auth, provider selection, and token tracking. The supervisor inherits jcode's context about what work is happening.
- Con: Circular dependency risk: hookwise gates jcode's tool calls, but also needs jcode to make LLM calls. Need to ensure the supervisor's own tool calls are not gated by hookwise (infinite loop).

**Option C1-c: Dedicated lightweight LLM call from hookwise daemon**
hookwise daemon has its own HTTP client and makes a simple `messages` API call with a focused prompt. No tool use, no agent loop - just "given this tool call and policy, what's your decision?"

- Pro: Simple, fast, no dependency on jcode's agent loop. No circular dependency.
- Con: Need to manage API keys separately. No benefit from jcode's provider abstraction.

**Recommendation:** C1-c for the initial implementation. A single API call with a focused prompt is the simplest path. The prompt includes the policy, role definition, and tool call context. No agent loop overhead. Avoids the circular dependency problem entirely.

### Gap 2: Human Escalation Channel

jcode already has a rich notification system: Telegram, Discord, ntfy, email. hookwise should integrate with these rather than building its own.

**Option C2-a: hookwise writes to jcode's notification channels directly**
hookwise reads jcode's config, connects to the same Telegram bot / Discord bot, and sends permission request messages.

- Pro: Single notification channel for the user.
- Con: Tight coupling to jcode's config format. Duplicated bot connection logic. Two processes fighting over the same bot token.

**Option C2-b: hookwise delegates to jcode's request_permission tool**
When HITL is needed, hookwise sends a request to jcode (via socket or subagent) that triggers jcode's `request_permission` tool. jcode handles notification routing.

- Pro: Clean separation. hookwise doesn't need to know about Telegram/Discord. jcode's existing permission review UI works.
- Con: Circular dependency again - hookwise is gating jcode's tools, now it needs jcode to send a notification.

**Option C2-c: hookwise has its own notification via simple webhook/CLI**
hookwise sends a notification via a configurable webhook URL, or via `ntfy publish`, or via a simple Telegram API call (independent bot token).

- Pro: Fully independent. No jcode coupling.
- Con: User manages two notification sources. Two bot tokens.

**Option C2-d: hookwise writes pending queue + fires external notification command**
hookwise writes to its pending queue (existing) and then runs a configurable `notify_command`:

```yaml
# .hookwise/policy.yml
hitl:
  notify_command: "ntfy publish hookwise-permissions"
  # or: "curl -X POST https://discord.com/api/webhooks/..."
  # or: "jcode debug message 'Permission request pending'"
  timeout_secs: 60
```

- Pro: Flexible. Works with any notification system. User configures once.
- Con: User needs to set up the notification integration.

**Recommendation:** C2-d. A configurable notify command is the most flexible and least coupled. For jcode integration specifically, the notify command could be `jcode debug message 'hookwise: permission request pending - run hookwise queue'`, which injects a message into the active jcode session.

### Gap 3: Session Identity Mapping

jcode has session IDs (UUIDs). hookwise has session registration with roles. How do they connect?

**Current state:**
- jcode sessions have: `session_id`, `model`, `provider`, `working_dir`, optional `display_role`
- jcode swarm has: `assign_role` action, plan items with `assigned_to`
- hookwise expects: `session_id` -> `(role, task, prompt_hash)`

**Options:**

**C3-a: Manual registration via CLI**
User runs `hookwise register --session-id <jcode-session-id> --role coder` manually or via a jcode skill/slash command.

**C3-b: Auto-registration from jcode swarm events**
hookwise daemon watches jcode's swarm events (via debug socket or communication channel) and auto-registers sessions when they're spawned with roles.

**C3-c: Default role from config**
hookwise falls back to a configured default role when a session is unregistered:
```yaml
jcode:
  default_role: coder
  auto_register: true
```

**Recommendation:** C3-c for single-session use, C3-b for swarm use. Most jcode users will be single-session interactive; a sensible default role eliminates the registration friction. For swarm, hookwise should observe `assign_role` events and auto-register.

### Gap 4: Rule Storage Location

Where does `.hookwise/` live?

**Options:**
- **In the project repo** (current design): `.hookwise/` alongside code. Checked into git. Shared across contributors. This is correct and should not change.
- **In `~/.jcode/`**: jcode-specific configuration. Not shared.
- **In `~/.config/hookwise/`** (current design for org/user rules): Global rules. Not shared.

**Recommendation:** No change needed. The existing storage layout works for jcode:
- Project rules: `<repo>/.hookwise/` (checked into git)
- User rules: `~/.config/hookwise/user/` (local)
- Org rules: `~/.config/hookwise/org/<org>/` (synced)
- jcode-specific config (default role, tool aliases): `~/.config/hookwise/jcode.yml` or `~/.jcode/hookwise.toml`

### Gap 5: HNSW Index Rebuild Timing

When should the embedding index be rebuilt in a jcode workflow?

**Current design:** `hookwise build` rebuilds manually. Lazy rebuild on first miss if index is stale.

**jcode-specific considerations:**
- Ambient mode: hookwise daemon could rebuild during idle periods between ambient cycles
- Session start: rebuild if rules have changed since last build (check file modification time)
- After N new decisions: rebuild when the JSONL files have N entries not yet in the index

**Recommendation:** Lazy rebuild on cache miss + periodic rebuild every N new decisions (e.g., every 50 new entries). The daemon can do this in a background thread without blocking permission checks (serve from the old index while the new one builds).

### Gap 6: jcode Has No Pre-Tool Hook Point

This is the biggest gap. jcode's tool execution happens inside `Agent::execute_tool` in the binary. There is no documented way to inject a permission check before a tool runs, other than the `[permissions]` config which only supports `"allow"`, `"ask"`, and (implied) `"deny"` - all static.

**Options to create a hook point:**

**C6-a: Feature request to jcode author**
Ask for `external:command` support in `[permissions]` config. When jcode sees `external:hookwise check`, it shells out to the command with the tool call as JSON on stdin, reads the decision from stdout.

**C6-b: System prompt injection**
Add instructions to jcode's system prompt (via AGENTS.md or CLAUDE.md) that tell the LLM to "check with hookwise before executing tools." This is advisory, not enforced - the LLM could ignore it.

**C6-c: MCP server as permission oracle**
hookwise runs as an MCP server. jcode connects to it via its `mcp.json` config. The LLM is instructed to call a `hookwise_check` MCP tool before executing file writes or shell commands. This gives hookwise a formal tool interface within jcode's agent loop.

**C6-d: Wrapper binary**
Ship a `jcode-hookwise` wrapper that intercepts jcode's invocation, modifies the system prompt, and delegates to the real jcode binary. Fragile but works without any jcode changes.

**Recommendation:** C6-c (MCP server) for the initial integration. hookwise already has an MCP server implementation (`src/cli/mcp_server.rs`). jcode already supports MCP servers via `~/.jcode/mcp.json`. The LLM is instructed (via AGENTS.md) to call `hookwise_check` before executing tools. This is not enforced at the binary level, but it works with zero jcode changes and gives hookwise full access to tool call metadata.

Long term: C6-a (native permission hook) for enforcement. The MCP approach is advisory; the native hook is mandatory.

---

## Summary: 10 Most Important Design Decisions

These decisions must be made before any code is written. They are ordered by dependency - later decisions depend on earlier ones.

### 1. Integration Model: Subprocess vs. Daemon vs. MCP Server

**Decision:** How does hookwise integrate with jcode's tool execution?

- **Subprocess** (per-call): Simplest, highest latency, no persistent state
- **Daemon** (long-running): Best performance, persistent caches, complex lifecycle
- **MCP server** (via jcode's MCP support): Zero jcode changes, advisory not enforced, already partially built

**Recommendation:** Start with MCP server (works today), migrate to daemon with native hook support when available.

### 2. Enforcement vs. Advisory

**Decision:** Can hookwise actually block tool execution, or can the LLM choose to ignore it?

- **Enforced** (binary-level hook): Requires jcode to support external permission checks in its tool execution loop. hookwise can truly block a tool call.
- **Advisory** (MCP tool + system prompt): The LLM is told to call `hookwise_check`, but there's no binary enforcement. A sufficiently "creative" prompt or a model bug could bypass it.

**Recommendation:** Advisory initially (MCP), enforced when jcode adds hook support. Document the advisory limitation clearly.

### 3. Tool Name Normalization Strategy

**Decision:** How does hookwise handle jcode's different tool names?

- **Alias map**: Config-driven mapping from jcode names to hookwise's canonical names
- **Native support**: hookwise natively understands both naming schemes
- **Normalization at the boundary**: The jcode format adapter normalizes before the cascade

**Recommendation:** Normalization at the boundary. The `JcodeHookInput` deserializer maps jcode tool names to hookwise's internal canonical names. The cascade logic never changes.

### 4. HITL Channel for Headless/Autonomous jcode

**Decision:** When hookwise needs a human decision and jcode is running headless, how does the human get notified and respond?

- Configurable `notify_command` in hookwise policy
- Integration with jcode's notification system (Telegram, Discord, ntfy)
- Dedicated hookwise notification channel (separate bot, separate webhook)

**Recommendation:** Configurable `notify_command` that defaults to `jcode debug message` for interactive sessions and a webhook/ntfy URL for headless. The pending queue remains the universal decision interface.

### 5. LLM Supervisor: Independent vs. jcode-Delegated

**Decision:** When hookwise needs an LLM evaluation, does it call the API directly or delegate to jcode?

- Direct API call: Independent, no circular dependency, separate billing
- jcode subagent: Reuses jcode's auth and providers, but creates circular dependency risk
- jcode MCP tool call: hookwise asks jcode to evaluate via an MCP response, which could trigger another hookwise check (loop)

**Recommendation:** Direct API call from the hookwise daemon. Use a cheap/fast model (Haiku/Sonnet). The focused prompt doesn't need agent capabilities - it's a single-turn classification task.

### 6. Session Registration: Explicit vs. Implicit

**Decision:** Must every jcode session be explicitly registered with a role, or should hookwise auto-assign?

- Explicit: `hookwise register --session-id ... --role coder` before first tool call
- Implicit: Default role from config, auto-register on first tool call
- Hybrid: Auto-register with default, allow explicit override

**Recommendation:** Hybrid. Default role in config (`jcode.default_role = "coder"`), auto-register on first contact, allow explicit override via `hookwise register` or MCP tool call.

### 7. Where hookwise Config Lives for jcode Users

**Decision:** Project rules in `.hookwise/` (existing). But where does the jcode-specific integration config live?

- `~/.jcode/hookwise.toml`: jcode-centric, discovered automatically
- `~/.config/hookwise/jcode.yml`: hookwise-centric, part of hookwise's existing config hierarchy
- `.hookwise/jcode.yml`: per-project jcode integration settings

**Recommendation:** `~/.config/hookwise/jcode.yml` for global settings (default role, tool aliases, LLM API key for supervisor, notify command). `.hookwise/policy.yml` already handles per-project settings and doesn't need to change.

### 8. Circular Dependency Prevention

**Decision:** hookwise gates jcode's tools. But hookwise may need to use jcode's tools (LLM calls, notifications). How to prevent infinite loops?

- Whitelist: hookwise's own tool calls are always allowed (identified by session ID or process identity)
- Separate channel: hookwise daemon makes its own API calls, never goes through jcode
- No sharing: hookwise is completely independent of jcode's agent loop

**Recommendation:** Complete independence. hookwise daemon has its own HTTP client, its own API keys, its own notification channels. It never calls jcode's tools. jcode calls hookwise; hookwise never calls jcode. The only exception is the `notify_command`, which may invoke jcode's debug socket for convenience, but this is fire-and-forget (no response expected).

### 9. Swarm Support: Single Authority vs. Per-Session

**Decision:** In a jcode swarm (multiple agents working together), is there one hookwise daemon for all agents, or one per agent?

- **Single authority**: One hookwise daemon serves all swarm members via its Unix socket. Shared cache, shared decisions, single pending queue.
- **Per-session**: Each agent has its own hookwise subprocess. Decisions may diverge.

**Recommendation:** Single authority (daemon). This is hookwise's designed architecture. The daemon's socket server already handles concurrent connections. Shared cache means a decision for one agent benefits all agents. The pending queue is unified, so the human reviews all requests in one place.

### 10. Migration Path: hookwise Today vs. hookwise for jcode

**Decision:** Should hookwise maintain backward compatibility with Claude Code and Gemini CLI, or fork for jcode?

- **Multi-target** (existing approach): hookwise supports Claude, Gemini, and jcode via `--format` flag. Single binary, multiple I/O adapters.
- **Fork**: Separate `hookwise-jcode` binary optimized for jcode's model.
- **Abstraction layer**: hookwise core is a library; thin binaries for each target.

**Recommendation:** Multi-target, existing approach. The cascade logic, cache, sanitization, and policy evaluation are host-agnostic. Only the I/O layer differs. Adding `HookFormat::Jcode` is ~100 lines. No fork needed.

---

## Next Steps

1. **Validate jcode MCP integration**: Add hookwise as an MCP server in `~/.jcode/mcp.json`, verify jcode can call hookwise tools
2. **Build `HookFormat::Jcode`**: New I/O adapter for jcode's tool call shape
3. **Write system prompt instructions**: AGENTS.md additions that instruct the LLM to call `hookwise_check` before tool execution
4. **Build the daemon mode**: `hookwise serve` as a long-running process with the existing socket server
5. **Feature request to jcode**: Native `external:command` support in `[permissions]` config for enforced gating
6. **Test with ambient mode**: Validate HITL works when jcode is running headless

## Appendix: jcode Internal Architecture (Observed)

Based on binary analysis (jcode v0.7.3, closed-source):

- **Tool execution**: `jcode::agent::Agent::execute_tool` - the core tool dispatch
- **Tool execution mode**: `jcode::tool::ToolExecutionMode` enum (details unknown)
- **Config**: `~/.jcode/config.toml` with `[permissions]`, `[safety]`, `[ambient]`, `[gateway]`, `[display]`, `[swarm]` sections
- **Permissions**: `shell = "ask"`, `file_read = "allow"`, `file_write = "ask"` - static per-category
- **Ambient permissions**: `request_permission` tool for headless permission requests, reviewed via `jcode permissions` TUI
- **Debug socket**: Broadcasts TUI state changes, accepts `server:*` commands including `tool execution`
- **Swarm**: `communicate` tool with `assign_role`, `spawn`, `dm`, `broadcast` actions
- **MCP support**: `~/.jcode/mcp.json` for external MCP servers
- **Sessions**: UUID-based, stored in `~/.jcode/sessions/`
- **Notification channels**: Telegram, Discord, ntfy, email - configured in `config.toml`
