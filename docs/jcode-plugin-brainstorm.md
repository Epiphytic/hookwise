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
- Startup amortization: hookwise already compiles regex sets (aho-corasick literal prefixes, ~150 gitleaks patterns into RegexSet), globsets (`CompiledPathPolicy` for each role), and loads the HNSW embedding index at startup. Paying this cost per-invocation is wasteful.
- State loss: each invocation is stateless. The `DashMap<String, SessionContext>` (static `SESSIONS`), in-memory `ExactCache` HashMap, `TokenJaccard` sorted token sets, and `EmbeddingSimilarity` HNSW index are all rebuilt from disk every time.
- This is exactly hookwise's current model (it's a subprocess hook for Claude Code). The performance characteristics are known and acceptable for Claude Code's use case (which also pays subprocess overhead), but jcode may want tighter integration.

**Fit:** Highest compatibility, lowest integration effort. This is the "just make it work" option. hookwise already speaks this protocol (with a format adapter for jcode's JSON shape).

### Option A4: Long-Running Sidecar Daemon (Unix Socket IPC)

**Mechanism:** The plugin runs as a persistent daemon process, started when jcode starts and communicating via a Unix domain socket. jcode sends tool call metadata to the socket before execution and reads back allow/deny/ask. The daemon maintains in-memory state (caches, indexes, compiled patterns) across all tool calls.

**Optimized for:** Performance with persistence. Startup cost paid once. In-memory caches survive across tool calls. The daemon can also serve multiple jcode sessions simultaneously (swarm scenario).

**Drawbacks:**
- Lifecycle management: who starts the daemon? Who stops it? What happens if it crashes? jcode needs daemon supervision or the user needs to start it manually.
- Protocol design: need a well-defined request/response protocol over the socket. More engineering than subprocess stdio.
- hookwise already has a Unix socket server (`ipc/socket_server.rs`) and client (`ipc/socket_client.rs`). The `IpcServer::serve` method accepts an async handler function and manages connections in spawned tokio tasks. The `IpcRequest`/`IpcResponse` types define the wire format. This infrastructure is designed for the supervisor agent communication, but the pattern can be repurposed for the entire cascade.

**Key observation from source code:** The existing `IpcServer::serve` signature is:
```rust
pub async fn serve<F>(&self, handler: F) -> Result<()>
where
    F: Fn(IpcRequest) -> Pin<Box<dyn Future<Output = Result<IpcResponse>> + Send>>
        + Send + Sync + 'static,
```
This is already the daemon architecture. The `IpcRequest` struct has `session_id`, `tool_name`, `tool_input`, `role`, `file_path`, `task_description`, `cwd` - nearly everything hookwise needs for a full cascade evaluation. The gap is that this currently only serves the supervisor tier, not the full cascade entry point.

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

### Option A7: MCP Server as Permission Oracle

**Mechanism:** hookwise runs as an MCP (Model Context Protocol) server. jcode connects to it via its `~/.jcode/mcp.json` config. The LLM is instructed via AGENTS.md to call a `hookwise_check` MCP tool before executing file writes or shell commands. hookwise already has an MCP server implementation (`src/cli/mcp_server.rs`).

**Optimized for:** Zero jcode changes. Uses existing MCP infrastructure. The hookwise MCP server can run as a long-lived process (stdio-based MCP servers are already long-running). Gives hookwise a formal tool interface within jcode's agent loop.

**Drawbacks:**
- Advisory, not enforced. The LLM is told to call `hookwise_check`, but there's no binary enforcement. A sufficiently creative prompt injection or a model error could bypass it.
- Adds a tool call to every tool call (meta-overhead). The LLM must generate a `hookwise_check` call, wait for the response, then decide whether to proceed.
- MCP tool calls are visible in the conversation context and consume tokens.
- Cannot block tool execution at the binary level - only the LLM's judgment stands between a denied hookwise check and the tool executing anyway.

**Fit:** Best path for immediate integration with zero jcode changes. Works today. The advisory limitation is real but acceptable for initial deployment.

---

## Part A: Plugin Lifecycle Design

Regardless of which integration mechanism is chosen, the plugin lifecycle has these phases:

| Phase | When | What happens | hookwise equivalent |
|-------|------|-------------|-------------------|
| **init** | jcode session start / daemon start | Load config, compile globsets, load caches, load HNSW index, connect to supervisor | `CascadeRunner` construction |
| **session_register** | New session appears (or first tool call) | Map session_id to role, compile path policies for that role | `SessionManager::get_or_populate` |
| **pre_tool** | Before each tool execution | Run the 6-tier cascade, return allow/deny/ask | `CascadeRunner::evaluate_with_cwd` |
| **post_tool** | After each tool execution | Audit logging, update statistics | Not yet implemented |
| **teardown** | jcode session end / daemon shutdown | Flush caches, close sockets, clean up registration files | `IpcServer::shutdown` |

The `pre_tool` phase is the critical path. The cascade evaluates in order:
1. `PathPolicyEngine::evaluate` - globset matching against `CompiledPathPolicy`
2. `ExactCache::evaluate` - HashMap lookup by `CacheKey{sanitized_input, tool, role}`
3. `TokenJaccard::evaluate` - Jaccard coefficient on deduplicated token sets
4. `EmbeddingSimilarity::evaluate` - HNSW nearest-neighbor via instant-distance
5. `SupervisorTier::evaluate` - via `SupervisorBackend` trait (socket or API)
6. `HumanTier::evaluate` - enqueue to `DecisionQueue`, poll for response

The `post_tool` phase would enable audit trails - logging what the tool actually did after hookwise allowed it.

---

## Part A: Unknowns
- [ ] Does jcode's `Agent::execute_tool` have any hook point where external code can intercept before execution?
- [ ] Can jcode's `[permissions]` config support external commands as decision sources?
- [ ] Does the debug socket fire events *before* tool execution or only *after*?
- [ ] What's the actual overhead of subprocess spawn on the target deployment environment?
- [ ] Is jcode's author open to adding a plugin/extension API? What form would they prefer?
- [ ] Does jcode's `SafetyConfig` struct have any extensibility beyond the current `shell`/`file_read`/`file_write` fields?
- [ ] Does jcode's MCP server support expose tool call metadata (session_id, cwd) to MCP tools?

## Part A: Recommendation

**Primary: Option A4 (Long-Running Sidecar Daemon)** for production.
**Immediate: Option A7 (MCP Server)** as the zero-change integration path.
**Bootstrap: Option A5 (Config-Driven Wrapper)** as the minimum viable enforced path.

The layered strategy:
1. **Now:** MCP server. hookwise already has `mcp_server.rs`. Add jcode-specific tool definitions. Advisory enforcement via AGENTS.md instructions. Works today.
2. **Soon:** Daemon mode (`hookwise serve`). Repurpose the existing `IpcServer` to serve full cascade evaluations, not just supervisor requests. A thin `hookwise-shim` binary connects to the daemon socket and bridges to jcode's subprocess or config-driven permission check.
3. **Later:** Native jcode hook support (feature request). jcode's `[permissions]` config supports `external:unix-socket:/tmp/hookwise.sock` or similar. Zero subprocess overhead, mandatory enforcement.

The sidecar daemon model is the best fit because:
1. hookwise already has a Unix socket server/client implementation (`IpcServer`, `IpcClient`)
2. In-memory caches (the entire point of the cascade) only work if the process persists - the `DashMap<String, SessionContext>`, `ExactCache`, `TokenJaccard` index, and `EmbeddingSimilarity` HNSW index all need process lifetime
3. Multiple jcode sessions (swarm) can share a single hookwise daemon
4. The daemon can manage its own lifecycle (HNSW rebuilds, session registration, pending queue)
5. The existing `IpcRequest` struct already carries `session_id`, `tool_name`, `tool_input`, `role`, `file_path`, `cwd` - nearly everything needed

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
// hook_io.rs
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

**Drawbacks:** Subprocess overhead per call. No persistent caches. Every invocation pays: aho-corasick compilation, RegexSet compilation (~150 patterns), GlobSet compilation per role, JSONL cache loading and HashMap construction, HNSW index deserialization. This is the costliest possible integration path per-call.

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

**Concrete changes to hookwise:**

The existing `IpcRequest` struct needs to be promoted from supervisor-only to cascade-entry-point:

```rust
// Current IpcRequest (ipc/mod.rs) - serves supervisor tier only
pub struct IpcRequest {
    pub session_id: String,
    pub tool_name: String,
    pub tool_input: String,     // serialized JSON string
    pub role: String,
    pub file_path: Option<String>,
    pub task_description: Option<String>,
    pub prompt_path: Option<String>,
    pub cwd: String,
}

// New: FullCascadeRequest - serves the entire cascade
pub struct FullCascadeRequest {
    pub session_id: String,
    pub tool_name: String,           // jcode tool name (shell_exec, file_write, etc.)
    pub tool_input: serde_json::Value, // structured, not serialized string
    pub cwd: String,
    pub role: Option<String>,        // optional - hookwise can auto-assign
    pub host: String,                // "claude", "gemini", "jcode"
}

// New: FullCascadeResponse
pub struct FullCascadeResponse {
    pub decision: Decision,          // allow/deny/ask
    pub tier: DecisionTier,          // which tier resolved
    pub confidence: f64,
    pub reason: String,
    pub cached: bool,                // was this a cache hit?
}
```

**The daemon's serve loop:**
```rust
// hookwise serve --socket /tmp/hookwise.sock
// 1. Load all config (policy.yml, roles.yml)
// 2. Build SanitizePipeline (aho-corasick, RegexSet, entropy)
// 3. Load ExactCache from JSONL
// 4. Build TokenJaccard index from cache entries
// 5. Load/build HNSW index
// 6. Start IpcServer on the socket
// 7. For each connection: deserialize FullCascadeRequest, run CascadeRunner::evaluate, return FullCascadeResponse
```

**Drawbacks:** Requires daemon lifecycle management. More complex deployment.

**Fit:** Production architecture. This is what we should build toward.

### Option B3: MCP Server with jcode Tool Definitions

**Mechanism:** hookwise runs as an MCP server (already exists: `src/cli/mcp_server.rs`). jcode connects to it via `~/.jcode/mcp.json`. The MCP server exposes tools like `hookwise_check`, `hookwise_register`, `hookwise_status`. The LLM is instructed via AGENTS.md to call `hookwise_check` before executing tools.

**What changes:**
- MCP server adds jcode-specific tool definitions with proper input schemas
- AGENTS.md instructions tell the LLM to call `hookwise_check` with `{tool_name, tool_input, cwd}` before executing certain tools
- The MCP server runs the full cascade and returns a structured decision

**Optimized for:** Immediate integration. Zero jcode changes. The MCP server is long-running (stdio-based), so caches persist within a session. Already partially built.

**Drawbacks:** Advisory only. LLM can ignore. Adds token overhead (extra tool call per tool call). Does not protect against prompt injection that tells the LLM to skip the check.

**Fit:** Best immediate path. Build this first, layer daemon enforcement on top.

### Option B4: jcode Subagent as Supervisor

**Mechanism:** Instead of hookwise making its own LLM calls for the supervisor tier, it delegates to a jcode subagent. jcode spawns a "hookwise supervisor" subagent session that has the policy context, role definitions, and cached decisions in its system prompt. When hookwise's cascade reaches tier 3, it sends the request to this subagent via jcode's swarm communication system.

**What this changes:**
- hookwise's `SupervisorBackend` trait gets a new implementation: `JcodeSubagentSupervisor`
- The subagent is spawned via jcode's `communicate` tool with `action: "spawn"`
- Communication happens via jcode's inter-session messaging, not a raw Unix socket
- The supervisor agent inherits jcode's provider/model configuration, token budget, etc.

**Drawbacks:** Tightly couples to jcode's swarm system. Adds latency (message routing through jcode server). The supervisor loses the ability to be a standalone process. Circular dependency risk: hookwise gating the tool calls that hookwise needs to communicate with the supervisor.

**Fit:** Elegant for the LLM tier specifically, but doesn't help with tiers 0-2b which are pure Rust computation.

---

### Part B: Decision Cascade Mapping to jcode's Tool Model

The cascade maps onto jcode tool calls with these specifics:

**Tier 0 - Path Policy:**
- jcode's `file_write`, `file_edit`, `multiedit`, `apply_patch`: extract `file_path` field directly. For `multiedit`, iterate each edit's target. For `apply_patch`, parse the diff to extract modified file paths.
- jcode's `shell_exec`: use hookwise's existing Bash path extraction regex (redirect targets, `rm`, `mv`, `cp`, `sed -i`, etc.)
- jcode's `file_read`, `file_grep`, `file_glob`: match against `allow_read` globs
- jcode's `webfetch`, `websearch`, `memory`, `communicate`, `schedule`: no file path policy applies; skip to tier 1

**Tier 1 - Exact Cache:**
- Cache key: `CacheKey { sanitized_input, tool, role }` where `tool` uses hookwise's canonical name (after normalization from jcode's tool name)
- Example: `shell_exec` with `{"command": "cargo test"}` becomes `CacheKey { sanitized_input: "{\"command\":\"cargo test\"}", tool: "Bash", role: "coder" }`

**Tier 2a - Token Jaccard:**
- Works identically. Token extraction is tool-input-agnostic (split on whitespace and punctuation).

**Tier 2b - Embedding Similarity:**
- Works identically. Embedding is computed on the sanitized tool input string.

**Tier 3 - LLM Supervisor:**
- The `SupervisorRequest` struct already carries all needed fields. The `role_description` comes from `roles.yml`. The `sanitized_input` is post-sanitization. The `cwd` provides repository context.
- For jcode: the existing `ApiSupervisor` implementation (direct API call) is the right choice. No circular dependency.

**Tier 4 - Human-in-the-loop:**
- This is where the model diverges most. See "HITL in jcode Context" below.

### Part B: Tool Name Mapping

jcode's tool names differ from Claude Code's. hookwise needs a normalization layer:

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

**Implementation approach:** Normalization at the boundary. Add a `normalize_tool_name` function that maps jcode tool names to hookwise's canonical names. This runs once when deserializing the input, before the cascade. The cascade logic never needs to know about jcode vs. Claude tool names.

```rust
fn normalize_tool_name(host: &str, tool_name: &str) -> &str {
    match (host, tool_name) {
        ("jcode", "shell_exec") => "Bash",
        ("jcode", "file_write") => "Write",
        ("jcode", "file_edit") | ("jcode", "multiedit") => "Edit",
        ("jcode", "file_read") => "Read",
        ("jcode", "file_grep") => "Grep",
        ("jcode", "file_glob") => "Glob",
        ("jcode", "task_runner") => "Task",
        ("jcode", "apply_patch") => "Write",
        _ => tool_name,
    }
}
```

**jcode-only tools** (`communicate`, `schedule`, `memory`, `webfetch`, `websearch`) need their own policy categories. These don't exist in Claude Code, so hookwise needs new policy entries:

```yaml
roles:
  coder:
    tools:
      allow: ["memory", "websearch", "webfetch"]
      deny: ["communicate:spawn", "schedule"]
      ask: ["communicate:broadcast"]
```

This is a new dimension - tool-level (not just path-level) policy. Currently hookwise only has path-level deterministic policy (tier 0). Tool-level policy is a generalization worth building.

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
| N/A | N/A | `description` | Optional description field on `shell_exec` |

**Tool input field differences:**

| Tool | Claude Code field | jcode field | Mapping |
|------|------------------|-------------|---------|
| Write | `file_path`, `content` | `file_path`, `content` | Identical |
| Edit | `file_path`, `old_text`, `new_text` | `file_path`, `old_string`, `new_string` | Rename |
| Bash | `command` | `command` | Identical |
| Read | `file_path` | `file_path` | Identical |
| Grep | `pattern`, `path` | `pattern`, `path` | Identical |
| Glob | `pattern` | `pattern`, `path` | Compatible |
| Task | `prompt` | `prompt`, `description` | Extra field |

The `CascadeRunner::extract_file_path` method (in `cascade/mod.rs`) needs updating to handle jcode's field names. Currently it handles `Write|Edit|Read` -> `file_path`, `Glob|Grep` -> `path`, `Bash` -> `None`. For jcode, `multiedit` needs to extract `file_path` from the top-level, and `apply_patch` needs to parse the patch text.

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
| **MCP tool response** | Hookwise MCP returns "ask" status, LLM uses request_permission | Same | Same |

**How the HITL flow works mechanically:**

The existing `DecisionQueue` in `cascade/human.rs` uses a file-backed queue (`pending_queue_path()`) and polls every 200ms for a response. This works cross-process: `hookwise check` enqueues a pending decision, and `hookwise approve <id>` (run by a human in another terminal) writes the response.

For jcode, the flow becomes:
1. hookwise cascade reaches tier 4
2. `HumanTier::evaluate` calls `queue.enqueue(pending_decision)`, which writes to the pending queue file
3. hookwise fires a `notify_command` (configurable) to alert the human
4. hookwise blocks, polling the queue file every 200ms for a response (up to `timeout_secs`)
5. Human responds via `hookwise approve <id>` / `hookwise deny <id>` (from any terminal, Telegram bot, Discord bot, or web UI)
6. The response appears in the queue file, hookwise's poll picks it up, decision is returned

**The notify_command approach:**
```yaml
# .hookwise/policy.yml
hitl:
  notify_command: "ntfy publish hookwise-permissions '{message}'"
  timeout_secs: 120
  timeout_default: deny
```

The `{message}` placeholder gets replaced with a human-readable summary:
```
hookwise: Permission request
Session: jcode-abc-123 (role: coder)
Tool: shell_exec
Command: npm publish --tag latest
Recommendation: deny (supervisor confidence: 0.85)
Respond: hookwise approve abc123 / hookwise deny abc123
```

**For Telegram/Discord integration:** hookwise could include a lightweight bot that listens for reply messages containing approval/denial commands. This is a separate binary (`hookwise-bot`) or daemon mode feature.

**For MCP integration:** When the cascade returns `ask`, the MCP tool response tells the LLM "permission denied, needs human approval." The LLM can then use jcode's `request_permission` tool to queue the request through jcode's own notification system. This is elegant but advisory - hookwise doesn't enforce the block, it just advises.

**Recommendation:** hookwise should integrate with jcode's existing notification channels rather than building its own. Use the configurable `notify_command` approach. For the MCP path, return a structured response that the LLM can use to invoke jcode's `request_permission`.

### Part B: Minimum Viable Integration

The smallest thing that proves the concept:

1. **hookwise MCP server with jcode tool definitions**: Add `hookwise_check` and `hookwise_register` tools to the existing MCP server
2. **AGENTS.md instructions**: Tell the LLM to call `hookwise_check` before `shell_exec`, `file_write`, `file_edit`, `multiedit`, `apply_patch`, and `task_runner`
3. **~/.jcode/mcp.json entry**: Point jcode to hookwise's MCP server
4. **Tool name normalization**: Map jcode tool names to hookwise's canonical names in the MCP server handler
5. **Test with a single session**: doing file writes and shell commands, verifying the cascade resolves decisions correctly

This requires:
- ~200 lines of Rust (MCP tool definitions, jcode-specific normalization)
- Zero jcode changes
- An AGENTS.md file with hookwise instructions
- The existing MCP server infrastructure

For the enforced path (phase 2):
- `hookwise serve` daemon mode (~300 lines, repurposing existing IpcServer)
- `hookwise-shim` tiny binary (~50 lines) that jcode calls as subprocess, forwards to daemon socket
- jcode supporting `external:command` in permissions config (feature request)

---

## Part C: Infrastructure Gaps

### Gap 1: LLM Supervisor for jcode

**Option C1-a: Direct API call from hookwise**
hookwise calls the Anthropic API (or any LLM) directly using its existing `ApiSupervisor` backend. The API key comes from environment or hookwise config.

- Pro: Independent of jcode. Works when jcode is not running. No circular dependency.
- Con: Separate API key / billing. Doesn't benefit from jcode's provider routing, token budget tracking, or multi-provider fallback.

**Option C1-b: jcode subagent via swarm**
hookwise asks jcode to spawn a subagent for the supervisor evaluation. Uses jcode's `communicate` tool.

- Pro: Reuses jcode's auth, provider selection, and token tracking. The supervisor inherits jcode's context about what work is happening.
- Con: Circular dependency risk: hookwise gates jcode's tool calls, but also needs jcode to make LLM calls. Need to ensure the supervisor's own tool calls are not gated by hookwise (infinite loop). Specifically: hookwise -> "ask jcode to spawn subagent" -> jcode spawns subagent -> subagent uses tools -> hookwise gates those tools -> hookwise needs to know "this is the supervisor's session, auto-allow".

**Option C1-c: Dedicated lightweight LLM call from hookwise daemon**
hookwise daemon has its own HTTP client and makes a simple `messages` API call with a focused prompt. No tool use, no agent loop - just "given this tool call and policy, what's your decision?"

- Pro: Simple, fast, no dependency on jcode's agent loop. No circular dependency. The `ApiSupervisor` implementation already does exactly this: it builds a system prompt from policy config, sends the tool call context as a user message, and parses a JSON response. The existing `ApiSupervisor::build_system_prompt` and `ApiSupervisor::build_user_message` methods are the template.
- Con: Need to manage API keys separately. No benefit from jcode's provider abstraction.

**Recommendation:** C1-c for the initial implementation. A single API call with a focused prompt is the simplest path. The existing `ApiSupervisor` is already this implementation. For jcode integration, just ensure the API key is configurable via environment variable (`HOOKWISE_API_KEY` or `ANTHROPIC_API_KEY`) and policy.yml.

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

**Recommendation:** C2-d. A configurable notify command is the most flexible and least coupled. For jcode integration specifically, the notify command could be `jcode debug message 'hookwise: permission request pending - run hookwise queue'`, which injects a message into the active jcode session. For headless/ambient: `ntfy publish <topic>` or a Discord webhook.

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

**C3-d: MCP-based registration**
The first `hookwise_check` MCP call from a new session triggers auto-registration. If no role is specified, hookwise asks the LLM to choose one (via MCP response), or falls back to the default.

**Recommendation:** C3-c for single-session use, C3-d for MCP integration, C3-b for swarm use. Most jcode users will be single-session interactive; a sensible default role eliminates the registration friction. For swarm, hookwise should observe `assign_role` events and auto-register.

The key insight: jcode's swarm `assign_role` action (`communicate` tool with `action: "assign_role"`, `role: "agent|coordinator|worktree_manager"`) maps to hookwise's role system, but the role names are different. jcode's roles are structural (agent, coordinator), while hookwise's are functional (coder, tester, reviewer). The mapping needs to be configurable:

```yaml
jcode:
  role_mapping:
    agent: coder        # default jcode agent -> hookwise coder
    coordinator: maintainer
    worktree_manager: maintainer
```

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
- jcode-specific integration config: `~/.config/hookwise/jcode.yml`
- The `ScopeResolver` hierarchy in `scope/hierarchy.rs` and `scope/merge.rs` already handles multi-level scope resolution. No changes needed to the scope system.

### Gap 5: HNSW Index Rebuild Timing

When should the embedding index be rebuilt in a jcode workflow?

**Current design:** `hookwise build` rebuilds manually. Lazy rebuild on first miss if index is stale.

**jcode-specific considerations:**
- Ambient mode: hookwise daemon could rebuild during idle periods between ambient cycles
- Session start: rebuild if rules have changed since last build (check file modification time)
- After N new decisions: rebuild when the JSONL files have N entries not yet in the index
- The `EmbeddingSimilarity::insert` method (called by `CascadeRunner::persist_decision`) already does incremental inserts. But incremental HNSW inserts may degrade index quality over time - a full rebuild periodically is wise.

**Recommendation:** Lazy rebuild on cache miss + periodic rebuild every N new decisions (e.g., every 50 new entries). The daemon can do this in a background tokio task without blocking permission checks (serve from the old index while the new one builds, then atomically swap). The `Arc<EmbeddingSimilarity>` wrapping allows atomic replacement.

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

**C6-e: Unix socket permission protocol**
Feature request to jcode: when `[permissions]` specifies `shell = "socket:/tmp/hookwise.sock"`, jcode sends the tool call as JSON over the socket and reads back allow/deny. This is the highest-performance enforced option, and maps directly to hookwise's existing `IpcServer`/`IpcClient` infrastructure.

**Recommendation:** C6-c (MCP server) for the initial integration. hookwise already has an MCP server implementation (`src/cli/mcp_server.rs`). jcode already supports MCP servers via `~/.jcode/mcp.json`. The LLM is instructed (via AGENTS.md) to call `hookwise_check` before executing tools.

Long term: C6-e (Unix socket permission protocol) for enforcement. This is the cleanest enforced integration: jcode natively sends permission checks to a Unix socket, hookwise daemon responds. No subprocess overhead, mandatory enforcement, type-safe protocol.

### Gap 7: Tool-Level Policy (Beyond Path Policy)

hookwise's current Tier 0 is path-only: "this role can write to `src/**`, not `tests/**`". But jcode has tools that don't involve file paths: `communicate`, `schedule`, `memory`, `webfetch`, `websearch`.

These need tool-level policy:
```yaml
roles:
  coder:
    tools:
      allow: ["file_read", "file_grep", "file_glob", "memory", "websearch", "webfetch"]
      ask: ["shell_exec", "file_write", "file_edit", "task_runner"]
      deny: ["communicate:spawn", "schedule"]
    paths:
      allow_write: ["src/**"]
      deny_write: ["tests/**"]
```

This is a new cascade tier (or an extension of Tier 0) that checks tool-level policy before path-level policy:
1. Tool-level policy: is this tool allowed for this role at all?
2. Path-level policy: for file tools, is this path allowed?
3. Cache, Jaccard, HNSW, supervisor, HITL (unchanged)

### Gap 8: multiedit and apply_patch File Path Extraction

hookwise's `CascadeRunner::extract_file_path` handles simple cases but jcode's `multiedit` and `apply_patch` are multi-file tools:

- `multiedit`: has `file_path` (single file, multiple edits within it) - can use existing extraction
- `apply_patch`: has `patch_text` containing a unified diff that may modify multiple files. Need to parse the diff to extract all modified file paths (lines starting with `---` and `+++`).

```rust
fn extract_patch_paths(patch_text: &str) -> Vec<String> {
    patch_text.lines()
        .filter(|line| line.starts_with("+++ ") || line.starts_with("--- "))
        .filter_map(|line| {
            let path = line.trim_start_matches("+++ ").trim_start_matches("--- ");
            let path = path.trim_start_matches("b/").trim_start_matches("a/");
            if path == "/dev/null" { None } else { Some(path.to_string()) }
        })
        .collect()
}
```

For multi-file tools, the path policy should check ALL extracted paths. If any path is denied, the entire tool call is denied.

---

## Top 10 Design Decisions

These decisions must be made before any code is written. They are ordered by dependency - later decisions depend on earlier ones.

### 1. Integration Model: MCP Server (Immediate) -> Daemon + Socket (Production)

**Decision:** How does hookwise integrate with jcode's tool execution?

- **MCP server** (via jcode's MCP support): Zero jcode changes, advisory not enforced, already partially built. Works today.
- **Daemon + socket** (long-running): Best performance, persistent caches, enforced gating. Requires jcode feature request.
- **Subprocess** (per-call): Simplest, highest latency, no persistent state. Fallback if daemon isn't viable.

**Recommendation:** Start with MCP server (works today), build daemon mode in parallel, migrate to daemon + native socket when jcode adds socket-based permission support. The MCP server and daemon share 95% of the cascade code - the difference is only the entry point (MCP tool handler vs. socket connection handler).

### 2. Enforcement vs. Advisory

**Decision:** Can hookwise actually block tool execution, or can the LLM choose to ignore it?

- **Enforced** (binary-level hook): Requires jcode to support external permission checks in its tool execution loop. hookwise can truly block a tool call.
- **Advisory** (MCP tool + system prompt): The LLM is told to call `hookwise_check`, but there's no binary enforcement.

**Recommendation:** Advisory initially (MCP), enforced when jcode adds hook support. Document the advisory limitation clearly. For high-security deployments, the daemon + `external:command` path is necessary.

### 3. Tool Name Normalization Strategy

**Decision:** How does hookwise handle jcode's different tool names?

- **Normalization at the boundary**: The jcode format adapter maps tool names before the cascade. The cascade logic never changes.
- **Native multi-scheme support**: hookwise natively understands both naming schemes.
- **Config-driven alias map**: User-configurable mapping.

**Recommendation:** Normalization at the boundary. A `normalize_tool_name("jcode", tool_name)` function in the I/O layer. The cascade operates on canonical names (`Bash`, `Write`, `Edit`, `Read`, etc.). jcode-only tools (`communicate`, `schedule`, `memory`) pass through unmapped and hit the new tool-level policy tier.

### 4. HITL Channel for Headless/Autonomous jcode

**Decision:** When hookwise needs a human decision and jcode is running headless, how does the human get notified and respond?

**Recommendation:** Configurable `notify_command` in hookwise policy. The command receives a JSON payload on stdin with the decision context. Default examples:
- Interactive: `hookwise monitor` in a separate terminal
- ntfy: `ntfy publish hookwise '{message}'`
- Discord webhook: `curl -X POST https://discord.com/api/webhooks/... -d '{message}'`
- jcode debug socket: `jcode debug message '{message}'`

The pending queue file (`/tmp/hookwise-pending*.json`) remains the universal response interface. Human responds via `hookwise approve <id>` / `hookwise deny <id>` from any channel.

### 5. LLM Supervisor: Independent API Call

**Decision:** When hookwise needs an LLM evaluation, does it call the API directly or delegate to jcode?

**Recommendation:** Direct API call from hookwise daemon/MCP server using the existing `ApiSupervisor`. Independent billing, independent API key, no circular dependency. Use a cheap/fast model (Haiku-class). The prompt is a single-turn classification task - it doesn't need agent capabilities, tool use, or multi-turn conversation.

### 6. Session Registration: Hybrid (Default + Explicit Override)

**Decision:** Must every jcode session be explicitly registered with a role, or should hookwise auto-assign?

**Recommendation:** Hybrid approach:
- Default role from config (`jcode.default_role = "coder"`) - auto-register on first contact
- Explicit override via `hookwise register --session-id ... --role tester` or MCP tool call
- jcode swarm: auto-register from `assign_role` events with configurable role mapping
- The existing `SessionManager::get_or_populate` method already supports env var fallback (`HOOKWISE_ROLE`); extend this with config file fallback

### 7. Where jcode-Specific Config Lives

**Decision:** Where does the jcode integration configuration live?

**Recommendation:** `~/.config/hookwise/jcode.yml` for global settings:
```yaml
default_role: coder
auto_register: true
role_mapping:
  agent: coder
  coordinator: maintainer
supervisor:
  backend: api
  api_key_env: ANTHROPIC_API_KEY
  model: claude-haiku-4-5-20250929
hitl:
  notify_command: "ntfy publish hookwise '{message}'"
  timeout_secs: 120
```
Per-project settings stay in `.hookwise/policy.yml` (unchanged).

### 8. Circular Dependency Prevention

**Decision:** hookwise gates jcode's tools. But hookwise may need to use jcode's tools (LLM calls, notifications). How to prevent infinite loops?

**Recommendation:** Complete independence. hookwise daemon/MCP server has its own HTTP client, its own API keys, its own notification channels. It never calls jcode's tools. jcode calls hookwise; hookwise never calls jcode. One-way dependency.

The only exception: the `notify_command` may invoke jcode's debug socket for convenience (`jcode debug message ...`). This is fire-and-forget (no response expected, no tool gating involved). It's a shell command, not a jcode tool call.

### 9. Swarm Support: Single Authority Daemon

**Decision:** In a jcode swarm (multiple agents working together), is there one hookwise daemon for all agents, or one per agent?

**Recommendation:** Single authority (daemon). One hookwise daemon serves all swarm members via its Unix socket. Benefits:
- Shared cache: a decision for one agent benefits all agents
- Unified pending queue: human reviews all requests in one place
- Cross-session learning: the coder agent's approved `cargo test` also pre-approves it for the tester agent (if roles permit)

The existing `IpcServer::serve` already handles concurrent connections with spawned tokio tasks. The `DashMap<String, SessionContext>` is thread-safe. No architectural changes needed for concurrency.

### 10. Multi-Target Compatibility: No Fork

**Decision:** Should hookwise maintain backward compatibility with Claude Code and Gemini CLI, or fork for jcode?

**Recommendation:** Multi-target, existing approach. The cascade logic, cache, sanitization, and policy evaluation are host-agnostic. Only the I/O layer differs:
- Claude: `HookInput` -> cascade -> `HookOutput` (with `hookSpecificOutput.permissionDecision`)
- Gemini: `HookInput` -> cascade -> `GeminiHookOutput` (with flat `decision`)
- jcode: `JcodeHookInput` -> cascade -> `JcodeHookOutput` (with `decision` + `reason` + `tier`)

Adding jcode support is ~200 lines in `hook_io.rs` plus tool name normalization. No fork needed. The `--format jcode` flag selects the I/O adapter. The daemon mode's `FullCascadeRequest`/`FullCascadeResponse` is format-agnostic by design.

---

## Next Steps

1. **Validate jcode MCP integration**: Add hookwise as an MCP server in `~/.jcode/mcp.json`, verify jcode can call hookwise tools
2. **Build jcode tool definitions for MCP server**: `hookwise_check`, `hookwise_register`, `hookwise_status` tools with proper input schemas
3. **Implement tool name normalization**: `normalize_tool_name("jcode", ...)` in `hook_io.rs`
4. **Write AGENTS.md instructions**: System prompt additions that instruct the LLM to call `hookwise_check` before executing gated tools
5. **Build the daemon mode**: `hookwise serve` command using existing `IpcServer`, serving full cascade evaluations
6. **Build the shim binary**: `hookwise-shim` tiny binary for subprocess-based integration with the daemon
7. **Feature request to jcode**: Native `external:command` or `socket:` support in `[permissions]` config for enforced gating
8. **Test with ambient mode**: Validate HITL works when jcode is running headless, using `notify_command`
9. **Test with swarm**: Validate single-daemon, multi-session scenario with role mapping

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

## Appendix: hookwise Source Architecture Reference

Key types and their locations for implementers:

| Type | File | Purpose |
|------|------|---------|
| `CascadeRunner` | `src/cascade/mod.rs` | Orchestrates the 6-tier cascade |
| `CascadeTier` trait | `src/cascade/mod.rs` | Interface each tier implements |
| `CascadeInput` | `src/cascade/mod.rs` | Input passed to each tier |
| `Decision` enum | `src/decision.rs` | Allow / Deny / Ask |
| `DecisionTier` enum | `src/decision.rs` | PathPolicy / ExactCache / TokenJaccard / ... |
| `DecisionRecord` | `src/decision.rs` | Complete persisted decision with metadata |
| `CacheKey` | `src/decision.rs` | (sanitized_input, tool, role) |
| `HookFormat` enum | `src/hook_io.rs` | Claude / Gemini / (Jcode) |
| `HookInput` | `src/hook_io.rs` | JSON payload from host agent |
| `HookOutput` | `src/hook_io.rs` | JSON response to host agent |
| `IpcRequest` | `src/ipc/mod.rs` | Socket wire format (request) |
| `IpcResponse` | `src/ipc/mod.rs` | Socket wire format (response) |
| `IpcServer` | `src/ipc/socket_server.rs` | Unix socket server (tokio) |
| `SessionContext` | `src/session/mod.rs` | In-memory session state |
| `SessionManager` | `src/session/mod.rs` | Registration, lookup, lifecycle |
| `SESSIONS` static | `src/session/mod.rs` | `DashMap<String, SessionContext>` global |
| `SupervisorBackend` trait | `src/cascade/supervisor.rs` | Pluggable LLM evaluation |
| `ApiSupervisor` | `src/cascade/supervisor.rs` | Direct Anthropic API supervisor |
| `UnixSocketSupervisor` | `src/cascade/supervisor.rs` | Socket-based supervisor |
| `DecisionQueue` | `src/cascade/human.rs` | File-backed HITL queue |
| `HumanTier` | `src/cascade/human.rs` | Tier 4 implementation |
| `SanitizePipeline` | `src/sanitize/mod.rs` | 3-layer secret detection |
| `CompiledPathPolicy` | `src/config/roles.rs` | GlobSet-compiled path rules |
| `PolicyConfig` | `src/config/mod.rs` | Top-level policy settings |
| `Commands` enum | `src/lib.rs` | CLI subcommands (clap) |
