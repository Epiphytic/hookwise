# captain-hook

Intelligent permission gating for AI coding assistants. A Rust binary that works as both a Claude Code plugin and Gemini CLI extension, providing learned permission decisions across multi-session and multi-agent environments.

## Architecture

6-tier decision cascade for every tool call:

```
Tool call -> Sanitize -> Path Policy -> Cache -> Token Sim -> Embed Sim -> Supervisor -> Human
               ~5us        ~1us        ~100ns     ~500ns      ~1-5ms      ~1-2s       interrupt
```

- **Tier 0**: Path policy — deterministic globset matching per role (~1us)
- **Tier 1**: Exact cache match (HashMap, ~100ns) — auto-resolves allow/deny, escalates ask
- **Tier 2a**: Token-level Jaccard similarity (~500ns) — fast approximate matching
- **Tier 2b**: Embedding similarity via fastembed + instant-distance HNSW (~1-5ms)
- **Tier 3**: LLM supervisor agent via Unix domain socket or Anthropic API (~1-2s)
- **Tier 4**: Human-in-the-loop with file-backed decision queue (variable)

Similarity only auto-approves, never auto-denies. Similarity propagates `ask`. Timeout defaults to deny.

Design document for more details: docs/captain-hook-design.md

## Tri-State Decision Model

Three decision states, not two:
- **allow** — permit, cached, auto-resolves on future matches
- **deny** — block, cached, auto-resolves on future matches
- **ask** — always prompt a human, cached as `ask` so it never auto-resolves

`ask` is for operations the user wants to stay aware of regardless of frequency (writing `.claude/`, `.env`, settings files, etc.). Sensitive paths default to `ask`.

Precedence: DENY > ASK > ALLOW > silent

## Project Structure

```
src/
  main.rs                     # CLI entry point (clap)
  lib.rs                      # Library root, re-exports
  error.rs                    # CaptainHookError enum (thiserror)
  decision.rs                 # Decision, DecisionRecord, CacheKey, DecisionTier
  hook_io.rs                  # Hook input/output JSON types (stdin/stdout)
  config/
    mod.rs                    # Config loading orchestration
    policy.rs                 # PolicyConfig, sensitive paths, YAML deserialization
    roles.rs                  # RoleDefinition, PathPolicy with GlobSet compilation
  sanitize/
    mod.rs                    # SanitizePipeline (chains all 4 layers)
    aho.rs                    # Layer 1: aho-corasick literal prefix matching
    regex_san.rs              # Layer 2: RegexSet positional patterns
    entropy.rs                # Layer 3: Shannon entropy detector
    encoding.rs               # Layer 4: encoding-aware (base64, URL-decode)
  storage/
    mod.rs                    # StorageBackend trait
    jsonl.rs                  # JSONL read/write for decision records
    index.rs                  # instant-distance HNSW index wrapper
  scope/
    mod.rs                    # ScopeResolver
    hierarchy.rs              # ScopeLevel enum + file loading
    merge.rs                  # DENY > ASK > ALLOW merge logic
  session/
    mod.rs                    # SessionManager
    context.rs                # SessionContext + DashMap cache
    registration.rs           # Registration file read/write/poll
  cascade/
    mod.rs                    # CascadeRunner orchestrator (runs all tiers in sequence)
    path_policy.rs            # Tier 0: globset path matching
    cache.rs                  # Tier 1: exact HashMap cache (tri-state)
    token_sim.rs              # Tier 2a: token-level Jaccard similarity
    embed_sim.rs              # Tier 2b: fastembed + instant-distance
    supervisor.rs             # Tier 3: SupervisorBackend trait + implementations
    human.rs                  # Tier 4: file-backed pending queue
  cli/
    mod.rs                    # Subcommand dispatch
    check.rs                  # `captain-hook check`: reads JSON from stdin
    session_check.rs          # `captain-hook session-check`: registration prompt
    register.rs               # register/disable/enable subcommands
    queue.rs                  # queue/approve/deny subcommands
    monitor.rs                # monitor/stats subcommands
    build.rs                  # build/invalidate subcommands
    override_cmd.rs           # override subcommand
    init.rs                   # init subcommand (creates .captain-hook/)
    scan.rs                   # scan --staged subcommand
  ipc/
    mod.rs                    # IPC types
    socket_server.rs          # tokio Unix domain socket server
    socket_client.rs          # Sync client (hook binary -> supervisor)
    pending_queue.rs          # File-backed pending decision queue
tests/
  sanitize_tests.rs           # 3-layer sanitization pipeline tests
  path_policy_tests.rs        # Globset path matching tests
  cache_tests.rs              # Tri-state cache behavior tests
  token_sim_tests.rs          # Jaccard similarity tests
  session_tests.rs            # Session registration tests
  cascade_integration.rs      # Full cascade integration tests
  cli_integration.rs          # CLI binary invocation tests
  ipc_integration.rs          # Unix socket round-trip tests
.captain-hook/                # Project-level config (checked into git)
  policy.yml                  # Project policy, sensitive paths, thresholds
  roles.yml                   # Role definitions with path policies
  rules/                      # Cached decisions (sanitized JSONL)
  .gitignore                  # Ignores .index/ and .user/
.claude-plugin/
  plugin.json                 # Claude Code plugin manifest
hooks/
  hooks.json                  # Hook definitions (PreToolUse, user_prompt_submit)
skills/                       # Slash command skill definitions
  register/SKILL.md
  disable/SKILL.md
  enable/SKILL.md
  switch/SKILL.md
  status/SKILL.md
agents/
  supervisor.md               # Supervisor agent instructions
docs/
  captain-hook-design.md      # Full design specification
  adr/                        # Architecture decision records
  architecture/               # Module interfaces spec
  research/                   # Gitleaks patterns, bash path extraction
  reviews/                    # Code and security review reports
```

Global config lives at `~/.config/captain-hook/`.

## Roles

Twelve built-in roles in three categories. Deterministic path globs (tier 0) + natural language descriptions (tier 3 LLM).

**Implementation roles** — write to specific code/config directories:

| Role | Writes to | Denied from |
|------|-----------|-------------|
| coder | src/, lib/, project config (Cargo.toml, package.json, etc.) | tests/, docs/, .github/, *.tf |
| tester | tests/, test-fixtures/, *.test.*, *_test.go, test configs | src/, lib/, docs/, .github/ |
| integrator | *.tf, *.tfvars, terraform/, infra/, pulumi/, helm/, ansible/ | src/, lib/, tests/, docs/ |
| devops | .github/, Dockerfile*, docker-compose*, .*rc, tool version files | src/, lib/, tests/, docs/ |

**Knowledge roles** — read codebase, write artifacts to docs/ subdirectories:

| Role | Writes to | Denied from |
|------|-----------|-------------|
| researcher | docs/research/ | src/, lib/, tests/, .github/ |
| architect | docs/architecture/, docs/adr/ | src/, lib/, tests/, .github/ |
| planner | docs/plans/ | src/, lib/, tests/, .github/ |
| reviewer | docs/reviews/ (not security/) | src/, lib/, tests/, .github/ |
| security-reviewer | docs/reviews/security/ | src/, lib/, tests/, .github/ |
| docs | docs/, *.md, *.aisp | src/, lib/, tests/, .github/ |

**Full-access roles** — unrestricted:

| Role | Writes to | Denied from |
|------|-----------|-------------|
| maintainer | ** | (none) |
| troubleshooter | ** | (none) |

Knowledge roles produce artifacts that implementation roles consume:
researcher -> architect -> planner -> coder/tester -> reviewer -> maintainer.

## Session Registration

Every session must register a role before tool calls are permitted. Three mechanisms:

1. **Interactive** — `user_prompt_submit` hook prompts user via AskUserQuestion on first prompt
2. **CLI** — `captain-hook register --session-id <id> --role <role>`
3. **Env var fallback** — `CAPTAIN_HOOK_ROLE=coder` for CI/scripted use

Unregistered sessions: hook waits 5s for registration, then blocks with instructions.

## Key Components

### Secret Sanitization
Four layers, compiled at startup:
1. **aho-corasick** — literal prefix matching (sk-ant-, sk-proj-, ghp_, AKIA, etc.)
2. **RegexSet** — positional/contextual patterns (bearer tokens, api keys, connection strings)
3. **Shannon entropy** — catch unknown formats (20+ char tokens with entropy > 4.0, also scans bare tokens)
4. **Encoding-aware** — decodes base64 and URL-encoded values before re-scanning

All tool input is sanitized before any cache/vector/storage operation.

### Hook I/O
The `captain-hook check` command reads JSON from **stdin** (matching Claude Code's hook protocol) and outputs JSON to stdout with `hookSpecificOutput` containing the `permissionDecision`.

### Path Policy (Tier 0)
Deterministic globset matching per role. Runs before cache/vector/LLM. Hard gate — cannot be overridden by cached decisions or LLM. Sensitive paths (`.claude/**`, `.env*`, etc.) default to `ask` regardless of role.

### Token Similarity (Tier 2a)
Token-level Jaccard similarity provides fast approximate matching (~500ns). Splits commands into tokens, computes set intersection/union ratio. Minimum 3-token threshold to avoid false matches on short commands.

### Embedding Similarity (Tier 2b)
fastembed generates embeddings, instant-distance provides HNSW-indexed nearest neighbor search. Rebuilds full index via `captain-hook build`.

### Supervisor (Tier 3)
Pluggable supervisor with two backends:
- **Unix socket** — communicates with Claude Code subagent via `/tmp/captain-hook-<team-id>.sock`
- **Anthropic API** — standalone mode using `ANTHROPIC_API_KEY` env var

### Human-in-the-Loop (Tier 4)
File-backed decision queue at `/tmp/captain-hook-pending.json` (or `$XDG_RUNTIME_DIR/captain-hook-pending.json`). Enables cross-process communication between the hook binary and CLI approve/deny commands.

### Scope Hierarchy
Deny > Ask > Allow. Precedence: Org > Project > User > Role.

### IPC
Unix domain socket at `/tmp/captain-hook-<team-id>.sock` for supervisor agent communication.

## Slash Commands

- `/captain-hook register` — pick a role interactively
- `/captain-hook disable` — opt out for this session
- `/captain-hook enable` — re-enable after disable
- `/captain-hook switch` — change role mid-session
- `/captain-hook status` — show current role, path policy, cache stats

## Dependencies

| Crate | Purpose |
|-------|---------|
| `clap` | CLI argument parsing with derive macros |
| `serde` / `serde_json` / `serde_yaml` | JSONL and YAML serialization |
| `aho-corasick` | Literal prefix matching for secret detection |
| `regex` | RegexSet for positional secret patterns |
| `globset` | Path policy glob pattern matching |
| `dashmap` | Concurrent session context cache |
| `fastembed` | Embedding generation for semantic similarity |
| `instant-distance` | HNSW-indexed vector similarity search |
| `tokio` | Async runtime for socket server and polling |
| `thiserror` / `anyhow` | Error handling |
| `sha2` | Hashing for cache keys |
| `chrono` | Timestamps for decision records |
| `tracing` / `tracing-subscriber` | Structured logging |
| `async-trait` | Async trait support for cascade tiers |
| `reqwest` | HTTP client for Anthropic API supervisor backend |
| `libc` | Unix socket permission management |

## CLI Modes

- **Hook mode**: `captain-hook check` — reads JSON from stdin, outputs permissionDecision JSON
- **Session check**: `captain-hook session-check` — registration prompt for `user_prompt_submit` hook
- **Queue mode**: `captain-hook queue/approve/deny` — human interface, supports `--always-ask`
- **Registration**: `captain-hook register/disable/enable` — session management
- **Monitor mode**: `captain-hook monitor/stats` — observe decisions, ask frequency
- **Cache management**: `captain-hook build/invalidate` — rebuild indexes or clear decisions
- **Overrides**: `captain-hook override --allow|--deny|--ask` — explicit per-role overrides
- **Init**: `captain-hook init` — creates `.captain-hook/` directory in a repo
- **Scan**: `captain-hook scan --staged` — pre-commit secret detection

## Building

```bash
cargo build --release
cargo test
cargo clippy -- -D warnings
cargo fmt --check
```

## Conventions

- Binary name: `captain-hook`
- Config directory: `.captain-hook/` in repos, `~/.config/captain-hook/` globally
- Socket path: `/tmp/captain-hook-<team-id>.sock`
- Pending queue: `/tmp/captain-hook-pending.json` or `$XDG_RUNTIME_DIR/captain-hook-pending.json`
- Rules are sanitized JSONL, checked into git, reviewable in PRs
- Vector indexes and user preferences are gitignored (derived/local artifacts)
- Pre-commit hook runs `captain-hook scan --staged` to prevent accidental secret commits
- Ships as a Claude Code plugin (binary + hooks + agent instructions + slash commands)
