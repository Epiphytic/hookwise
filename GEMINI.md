# captain-hook

Intelligent permission gating for AI coding assistants. Every tool call is evaluated through a 6-tier decision cascade before execution.

## Available MCP Tools

Use these tools to manage captain-hook sessions and permissions:

- **captain_hook_register** — Register this session with a role (coder, tester, devops, maintainer, etc.). Required before tool calls are permitted.
- **captain_hook_disable** — Disable captain-hook for this session (all tools permitted).
- **captain_hook_enable** — Re-enable captain-hook after disabling.
- **captain_hook_status** — Show current role, path policies, cache stats, and sensitive paths.
- **captain_hook_queue** — List pending permission decisions waiting for human approval.
- **captain_hook_approve** — Approve a pending permission decision.
- **captain_hook_deny** — Deny a pending permission decision.

## Role System

Each session must register a role that determines file access permissions:

**Implementation roles** (write to specific directories):
- `coder` — src/, lib/, project config files
- `tester` — tests/, test configs
- `integrator` — terraform, pulumi, helm, ansible files
- `devops` — CI/CD, Dockerfiles, tooling config

**Knowledge roles** (write to docs/ subdirectories only):
- `researcher`, `architect`, `planner`, `reviewer`, `security-reviewer`, `docs`

**Full-access roles** (unrestricted):
- `maintainer`, `troubleshooter`

## How It Works

Tool calls flow through the cascade:
1. **Path policy** — deterministic glob matching per role
2. **Exact cache** — previously seen identical commands
3. **Token similarity** — Jaccard similarity on command tokens
4. **Embedding similarity** — semantic vector search
5. **Supervisor** — LLM-based evaluation
6. **Human** — manual approval queue

Decisions are `allow`, `deny`, or `ask` (always prompts human). Sensitive paths like `.env`, `.gemini/`, config files default to `ask` regardless of role.
