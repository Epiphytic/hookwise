# Gemini Extension Support for captain-hook

**Date**: 2026-02-09
**Status**: Approved

## Overview

Add Gemini CLI extension support alongside the existing Claude Code plugin. The same `captain-hook` binary serves both ecosystems via a `--format` flag on the hook commands and a new MCP server subcommand for Gemini management tools.

## Design Decisions

1. **Adapter in binary**: `--format=claude|gemini` flag on `check` and `session-check`. Binary detects input format and outputs the correct JSON shape. Single binary serves both.
2. **Hooks + MCP**: Hooks handle gating (BeforeTool), MCP server exposes management commands (register, status, queue, etc.) as tools the model can call.
3. **Sibling directories**: `gemini-extension.json` at repo root alongside `.claude-plugin/`. Skills and agents are duplicated (not symlinked) — each system manages its own copy.
4. **Native installation**: Claude installs via `claude plugin install`, Gemini via `gemini extensions install`. No custom file copying.
5. **Binary self-update**: `captain-hook self-update` downloads latest release from GitHub. Periodic version check on the hot path (once/day, stderr warning).
6. **Crate publication**: `cargo install captain-hook` as alternate install path.

## Hook Protocol Differences

| Aspect | Claude Code | Gemini CLI |
|--------|-------------|------------|
| Pre-tool event | `PreToolUse` | `BeforeTool` |
| Session event | `user_prompt_submit` | `BeforeAgent` |
| Output format | `{"hookSpecificOutput":{"permissionDecision":"allow"}}` | `{"decision":"allow"}` |
| Deny exit code | 1 | 2 (emergency block) |
| Extra input fields | `permission_mode` | `hook_event_name`, `timestamp`, `transcript_path`, `mcp_context` |

The `HookInput` struct accepts both formats via `#[serde(default)]` on extra fields.

## File Layout

### New files

| File | Purpose |
|------|---------|
| `gemini-extension.json` | Gemini extension manifest (MCP server + context file) |
| `GEMINI.md` | Model context for Gemini (overview, MCP tools, role system) |
| `hooks/gemini-hooks.json` | Gemini hook definitions (BeforeTool, BeforeAgent) |
| `commands/captain-hook/register.toml` | Gemini slash command delegating to MCP tool |
| `commands/captain-hook/disable.toml` | " |
| `commands/captain-hook/enable.toml` | " |
| `commands/captain-hook/switch.toml` | " |
| `commands/captain-hook/status.toml` | " |
| `src/cli/mcp_server.rs` | MCP stdio server exposing management tools via rmcp |
| `src/cli/self_update.rs` | Binary self-update from GitHub releases |

### Modified files

| File | Change |
|------|--------|
| `src/hook_io.rs` | Add `GeminiHookOutput`, format-aware output, relax `HookInput` with `#[serde(default)]` |
| `src/lib.rs` | Add `HookFormat` enum, `--format` on Check/SessionCheck, McpServer + SelfUpdate subcommands |
| `src/cli/mod.rs` | Dispatch new subcommands |
| `src/cli/check.rs` | Accept format param, format-aware output, exit code 2 for Gemini deny |
| `src/cli/session_check.rs` | Accept format param, format-aware output |
| `Cargo.toml` | Add `rmcp` dep, publish-ready metadata |
| `scripts/install.sh` | Simplify to bootstrap (binary only), print native install instructions |
| `.github/workflows/release.yml` | Add `cargo publish` step, remove plugin-files tarball |

### Unchanged

- Entire cascade engine (src/cascade/, src/session/, src/sanitize/, src/storage/, src/scope/, src/config/)
- Existing Claude plugin files (.claude-plugin/plugin.json, hooks/hooks.json, skills/, agents/)
- CI workflow (.github/workflows/ci.yml)

## Gemini Extension Manifest

```json
{
  "name": "captain-hook",
  "version": "0.1.1",
  "description": "Intelligent permission gating for AI coding assistants",
  "mcpServers": {
    "captain-hook": {
      "command": "captain-hook",
      "args": ["mcp-server"]
    }
  },
  "contextFileName": "GEMINI.md"
}
```

## Gemini Hooks

```json
{
  "hooks": {
    "BeforeAgent": [
      {
        "matcher": ".*",
        "hooks": [{
          "name": "session-check",
          "type": "command",
          "command": "captain-hook session-check --format gemini"
        }]
      }
    ],
    "BeforeTool": [
      {
        "matcher": ".*",
        "hooks": [{
          "name": "permission-check",
          "type": "command",
          "command": "captain-hook check --format gemini"
        }]
      }
    ]
  }
}
```

## MCP Server Tools

| MCP Tool | Maps to CLI | Purpose |
|----------|-------------|---------|
| `captain_hook_register` | `captain-hook register` | Register session with a role |
| `captain_hook_disable` | `captain-hook disable` | Disable for session |
| `captain_hook_enable` | `captain-hook enable` | Re-enable after disable |
| `captain_hook_status` | `captain-hook stats` | Show role, cache stats, path policy |
| `captain_hook_queue` | `captain-hook queue` | List pending decisions |
| `captain_hook_approve` | `captain-hook approve` | Approve a pending decision |
| `captain_hook_deny` | `captain-hook deny` | Deny a pending decision |

## Installation

Three independent install paths, each using native tooling:

**Binary** (standalone):
```bash
captain-hook self-update          # auto-update from GitHub releases
cargo install captain-hook        # alternate: from crates.io
./scripts/install.sh              # alternate: bootstrap script
```

**Claude plugin** (via Claude marketplace):
```bash
claude plugin marketplace add /path/to/captain-hook   # or GitHub URL
claude plugin install captain-hook@captain-hook-local
```

**Gemini extension** (via Gemini extension system):
```bash
gemini extensions install /path/to/captain-hook   # or GitHub URL
```

## Binary Auto-Update

New `SelfUpdate` subcommand:

- `captain-hook self-update --check` — query GitHub releases API, compare against compiled-in version
- `captain-hook self-update` — download latest binary, verify SHA-256, replace in-place

Periodic background check: the `check` subcommand writes `~/.config/captain-hook/update-check.json` with last-checked timestamp. If >24h stale, prints stderr warning about available updates. Non-blocking.

## Dependency Addition

- `rmcp` — Rust MCP SDK for stdio JSON-RPC server (server + transport-io features)
