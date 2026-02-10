#!/usr/bin/env bash
# ═══════════════════════════════════════════════════════════════════════
# captain-hook End-to-End Tests
# ═══════════════════════════════════════════════════════════════════════
#
# Tests the full plugin lifecycle:
#   1. Validates marketplace manifest
#   2. Installs the plugin via Claude CLI (marketplace add + plugin install)
#   3. Initializes captain-hook in a test project
#   4. Registers sessions with various roles
#   5. Simulates Claude Code tool calls and verifies hook decisions
#   6. Cleans up and reports results
#
# Usage:
#   ./scripts/e2e-test.sh            # full suite
#   ./scripts/e2e-test.sh --skip-build  # skip cargo build (use existing binary)
#   ./scripts/e2e-test.sh --skip-install # skip plugin install via Claude CLI
#
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
E2E_DIR="$REPO_ROOT/captain-hook-e2e-tests"
BINARY="$REPO_ROOT/target/release/captain-hook"
MARKETPLACE_NAME="captain-hook-dev"
PLUGIN_NAME="captain-hook"

PASS=0
FAIL=0
SKIP=0
ERRORS=()

SKIP_BUILD=false
SKIP_INSTALL=false
for arg in "$@"; do
	case "$arg" in
	--skip-build) SKIP_BUILD=true ;;
	--skip-install) SKIP_INSTALL=true ;;
	esac
done

# ── Colors ──────────────────────────────────────────────────────────
GREEN='\033[0;32m'
RED='\033[0;31m'
YELLOW='\033[0;33m'
CYAN='\033[0;36m'
BOLD='\033[1m'
NC='\033[0m'

pass() {
	PASS=$((PASS + 1))
	echo -e "  ${GREEN}✔ PASS${NC}: $1"
}
fail() {
	FAIL=$((FAIL + 1))
	echo -e "  ${RED}✘ FAIL${NC}: $1"
	ERRORS+=("$1")
}
skip() {
	SKIP=$((SKIP + 1))
	echo -e "  ${YELLOW}⊘ SKIP${NC}: $1"
}
header() { echo -e "\n${CYAN}${BOLD}═══ $1 ═══${NC}"; }

# ── Helper: simulate a PreToolUse hook call ─────────────────────────
# Pipes hook input JSON to `captain-hook check --format=gemini` and
# returns the decision ("allow", "deny", or "ask").
check_tool() {
	local session_id="$1"
	local tool_name="$2"
	local tool_input="$3"
	local cwd="${4:-$E2E_DIR}"

	local json="{\"session_id\":\"${session_id}\",\"tool_name\":\"${tool_name}\",\"tool_input\":${tool_input},\"cwd\":\"${cwd}\"}"
	local output exit_code
	output=$(echo "$json" | "$BINARY" check --format=gemini 2>/dev/null) || exit_code=$?
	echo "$output"
}

# Extract decision field from Gemini-format JSON
get_decision() {
	echo "$1" | sed -n 's/.*"decision":"\([^"]*\)".*/\1/p'
}

# Assert that a tool call produces the expected decision
assert_decision() {
	local label="$1"
	local expected="$2"
	local session_id="$3"
	local tool_name="$4"
	local tool_input="$5"

	local output decision
	output=$(check_tool "$session_id" "$tool_name" "$tool_input")
	decision=$(get_decision "$output")

	if [ "$decision" = "$expected" ]; then
		pass "$label → $expected"
	else
		fail "$label → expected '$expected', got '$decision' (raw: $output)"
	fi
}

# ═══════════════════════════════════════════════════════════════════════
# Phase 1: Build
# ═══════════════════════════════════════════════════════════════════════
header "Phase 1: Build"

if [ "$SKIP_BUILD" = true ]; then
	if [ -x "$BINARY" ]; then
		skip "build (--skip-build, using existing binary)"
	else
		echo -e "${RED}ERROR: --skip-build but no binary at $BINARY${NC}"
		exit 1
	fi
else
	echo "  Building captain-hook (release)..."
	if cargo build --release --manifest-path "$REPO_ROOT/Cargo.toml" 2>&1 | tail -1; then
		pass "cargo build --release"
	else
		fail "cargo build --release"
		echo -e "${RED}Cannot continue without binary. Aborting.${NC}"
		exit 1
	fi
fi

# Ensure binary is findable for hook commands
export PATH="$REPO_ROOT/target/release:$PATH"

# ═══════════════════════════════════════════════════════════════════════
# Phase 2: Plugin Installation via Claude CLI
# ═══════════════════════════════════════════════════════════════════════
header "Phase 2: Plugin Installation via Claude CLI"

if [ "$SKIP_INSTALL" = true ]; then
	skip "plugin installation (--skip-install)"
elif ! command -v claude &>/dev/null; then
	skip "plugin installation (claude CLI not in PATH)"
else
	# 2a. Validate marketplace manifest
	echo "  Validating marketplace manifest..."
	if claude plugin validate "$REPO_ROOT" 2>&1 | grep -q "Validation passed"; then
		pass "marketplace.json validates cleanly"
	else
		fail "marketplace.json validation"
	fi

	# 2b. Add the repo as a marketplace
	echo "  Adding marketplace..."
	# Remove existing entry if present, to get a clean state
	claude plugin marketplace remove "$MARKETPLACE_NAME" 2>/dev/null || true

	if claude plugin marketplace add "$REPO_ROOT" 2>&1; then
		pass "marketplace added: $MARKETPLACE_NAME"
	else
		fail "marketplace add $REPO_ROOT"
	fi

	# Verify marketplace appears in list
	if claude plugin marketplace list 2>&1 | grep -q "$MARKETPLACE_NAME"; then
		pass "marketplace visible in 'claude plugin marketplace list'"
	else
		fail "marketplace not visible in list"
	fi

	# 2c. Install the plugin into the e2e test project
	echo "  Installing plugin into test project..."
	# Uninstall any previous version first
	(cd "$E2E_DIR" && claude plugin uninstall "$PLUGIN_NAME" --scope project 2>/dev/null) || true

	if (cd "$E2E_DIR" && claude plugin install "${PLUGIN_NAME}@${MARKETPLACE_NAME}" --scope project 2>&1); then
		pass "plugin installed via 'claude plugin install ${PLUGIN_NAME}@${MARKETPLACE_NAME} --scope project'"
	else
		fail "plugin install via Claude CLI"
	fi

	# 2d. Verify the plugin is listed as installed
	if claude plugin list 2>&1 | grep -q "$PLUGIN_NAME.*$MARKETPLACE_NAME"; then
		pass "plugin appears in 'claude plugin list'"
	else
		# Check without marketplace qualifier
		if claude plugin list 2>&1 | grep -q "$PLUGIN_NAME"; then
			pass "plugin appears in 'claude plugin list' (without marketplace qualifier)"
		else
			fail "plugin not found in 'claude plugin list'"
		fi
	fi

	# 2e. Verify the project has .claude/ settings referencing the plugin
	if [ -f "$E2E_DIR/.claude/settings.json" ] || [ -f "$E2E_DIR/.claude/settings.local.json" ]; then
		pass "project .claude/ settings created by plugin install"
	else
		skip "no project-level .claude/ settings found (plugin may be user-scoped)"
	fi
fi

# ═══════════════════════════════════════════════════════════════════════
# Phase 3: Project Setup
# ═══════════════════════════════════════════════════════════════════════
header "Phase 3: Test Project Setup"

# Ensure e2e directory exists and is a git repo
mkdir -p "$E2E_DIR"
if [ ! -d "$E2E_DIR/.git" ]; then
	git -C "$E2E_DIR" init -q
fi

# Clean previous captain-hook state (but preserve .git and .claude)
rm -rf "$E2E_DIR/.captain-hook"
rm -rf "$E2E_DIR/src" "$E2E_DIR/tests" "$E2E_DIR/docs" "$E2E_DIR/lib"
rm -f "$E2E_DIR/.env" "$E2E_DIR/.env.local" "$E2E_DIR/Cargo.toml"

# Create test file structure that matches category patterns
mkdir -p "$E2E_DIR/src/config"
mkdir -p "$E2E_DIR/tests"
mkdir -p "$E2E_DIR/docs/research"
mkdir -p "$E2E_DIR/docs/reviews/security"
mkdir -p "$E2E_DIR/lib"
echo 'fn main() {}' >"$E2E_DIR/src/main.rs"
echo 'pub mod config;' >"$E2E_DIR/src/lib.rs"
echo '#[test] fn t() {}' >"$E2E_DIR/tests/test.rs"
echo '# Docs' >"$E2E_DIR/docs/README.md"
echo 'SECRET=password123' >"$E2E_DIR/.env"
echo '[package]' >"$E2E_DIR/Cargo.toml"
pass "test project file structure created"

# Initialize captain-hook
echo "  Running captain-hook init..."
(cd "$E2E_DIR" && "$BINARY" init 2>&1) || true

if [ -f "$E2E_DIR/.captain-hook/policy.yml" ] && [ -f "$E2E_DIR/.captain-hook/roles.yml" ]; then
	pass "captain-hook init created config files"
else
	fail "captain-hook init failed to create config files"
	echo -e "${RED}Cannot continue without config. Aborting.${NC}"
	exit 1
fi

# Copy full roles.yml from project (init only generates coder/tester/maintainer)
cp "$REPO_ROOT/.captain-hook/roles.yml" "$E2E_DIR/.captain-hook/roles.yml"
pass "copied full roles.yml (all 12 roles) to test project"

# Reduce timeouts for fast testing
if [[ "$OSTYPE" == "darwin"* ]]; then
	sed -i '' 's/human_timeout_secs: 60/human_timeout_secs: 2/' "$E2E_DIR/.captain-hook/policy.yml"
else
	sed -i 's/human_timeout_secs: 60/human_timeout_secs: 2/' "$E2E_DIR/.captain-hook/policy.yml"
fi
pass "human_timeout_secs set to 2 for testing"

# ═══════════════════════════════════════════════════════════════════════
# Phase 4: Session Registration
# ═══════════════════════════════════════════════════════════════════════
header "Phase 4: Session Registration"

register_session() {
	local sid="$1" role="$2"
	if "$BINARY" register --session-id "$sid" --role "$role" 2>&1; then
		pass "registered session '$sid' as '$role'"
	else
		fail "register session '$sid' as '$role'"
	fi
}

register_session "e2e-coder" "coder"
register_session "e2e-tester" "tester"
register_session "e2e-maintainer" "maintainer"
register_session "e2e-researcher" "researcher"
register_session "e2e-docs" "docs"

# Disabled session
if "$BINARY" disable --session-id "e2e-disabled" 2>&1; then
	pass "disabled session 'e2e-disabled'"
else
	fail "disable session 'e2e-disabled'"
fi

# ═══════════════════════════════════════════════════════════════════════
# Phase 5: Hook Behavior Tests — Coder Role
# ═══════════════════════════════════════════════════════════════════════
header "Phase 5: Coder Role Tests"

# Coder should be ALLOWED to write to src/ ({{source}} category)
assert_decision \
	"coder Write src/main.rs" "allow" \
	"e2e-coder" "Write" '{"file_path":"src/main.rs","content":"fn main() {}"}'

# Coder should be ALLOWED to write to Cargo.toml ({{config_files}} category)
assert_decision \
	"coder Write Cargo.toml" "allow" \
	"e2e-coder" "Write" '{"file_path":"Cargo.toml","content":"[package]"}'

# Coder should be ALLOWED to edit src/lib.rs
assert_decision \
	"coder Edit src/lib.rs" "allow" \
	"e2e-coder" "Edit" '{"file_path":"src/lib.rs","old_string":"pub","new_string":"pub(crate)"}'

# Coder should be DENIED writing to tests/ ({{tests}} in deny_write)
assert_decision \
	"coder Write tests/test.rs" "deny" \
	"e2e-coder" "Write" '{"file_path":"tests/test.rs","content":"#[test] fn t() {}"}'

# Coder should be DENIED writing to docs/ ({{docs}} in deny_write)
assert_decision \
	"coder Write docs/README.md" "deny" \
	"e2e-coder" "Write" '{"file_path":"docs/README.md","content":"# Docs"}'

# Coder should be DENIED writing to .github/ ({{ci}} in deny_write)
assert_decision \
	"coder Write .github/workflows/ci.yml" "deny" \
	"e2e-coder" "Write" '{"file_path":".github/workflows/ci.yml","content":"on: push"}'

# Coder should get ASK for sensitive .env file
assert_decision \
	"coder Write .env" "ask" \
	"e2e-coder" "Write" '{"file_path":".env","content":"SECRET=x"}'

# Coder should get ASK for .env.local (matches .env* pattern)
assert_decision \
	"coder Write .env.local" "ask" \
	"e2e-coder" "Write" '{"file_path":".env.local","content":"DB_HOST=localhost"}'

# Coder should get ASK for .captain-hook/ (sensitive path)
assert_decision \
	"coder Write .captain-hook/roles.yml" "ask" \
	"e2e-coder" "Write" '{"file_path":".captain-hook/roles.yml","content":"roles:"}'

# ═══════════════════════════════════════════════════════════════════════
# Phase 6: Hook Behavior Tests — Tester Role
# ═══════════════════════════════════════════════════════════════════════
header "Phase 6: Tester Role Tests"

# Tester should be ALLOWED to write to tests/ ({{tests}} in allow_write)
assert_decision \
	"tester Write tests/test.rs" "allow" \
	"e2e-tester" "Write" '{"file_path":"tests/test.rs","content":"#[test] fn t() {}"}'

# Tester should be DENIED writing to src/ ({{source}} in deny_write)
assert_decision \
	"tester Write src/main.rs" "deny" \
	"e2e-tester" "Write" '{"file_path":"src/main.rs","content":"fn main() {}"}'

# Tester should be DENIED writing to docs/ ({{docs}} in deny_write)
assert_decision \
	"tester Write docs/README.md" "deny" \
	"e2e-tester" "Write" '{"file_path":"docs/README.md","content":"# docs"}'

# ═══════════════════════════════════════════════════════════════════════
# Phase 7: Hook Behavior Tests — Maintainer Role
# ═══════════════════════════════════════════════════════════════════════
header "Phase 7: Maintainer Role Tests"

# Maintainer has allow_write: **, should be allowed everywhere (non-sensitive)
assert_decision \
	"maintainer Write src/main.rs" "allow" \
	"e2e-maintainer" "Write" '{"file_path":"src/main.rs","content":"fn main() {}"}'

assert_decision \
	"maintainer Write tests/test.rs" "allow" \
	"e2e-maintainer" "Write" '{"file_path":"tests/test.rs","content":"#[test] fn t() {}"}'

assert_decision \
	"maintainer Write docs/README.md" "allow" \
	"e2e-maintainer" "Write" '{"file_path":"docs/README.md","content":"# docs"}'

# Even maintainer should get ASK for sensitive paths
assert_decision \
	"maintainer Write .env" "ask" \
	"e2e-maintainer" "Write" '{"file_path":".env","content":"SECRET=x"}'

# ═══════════════════════════════════════════════════════════════════════
# Phase 8: Hook Behavior Tests — Knowledge Roles
# ═══════════════════════════════════════════════════════════════════════
header "Phase 8: Knowledge Role Tests"

# Researcher should be ALLOWED to write to docs/research/
assert_decision \
	"researcher Write docs/research/findings.md" "allow" \
	"e2e-researcher" "Write" '{"file_path":"docs/research/findings.md","content":"# Findings"}'

# Researcher should be DENIED writing to src/
assert_decision \
	"researcher Write src/main.rs" "deny" \
	"e2e-researcher" "Write" '{"file_path":"src/main.rs","content":"fn main() {}"}'

# Docs role should be ALLOWED to write to docs/ ({{docs_output}})
assert_decision \
	"docs Write docs/README.md" "allow" \
	"e2e-docs" "Write" '{"file_path":"docs/README.md","content":"# Documentation"}'

# Docs role should be DENIED writing to src/
assert_decision \
	"docs Write src/main.rs" "deny" \
	"e2e-docs" "Write" '{"file_path":"src/main.rs","content":"fn main() {}"}'

# ═══════════════════════════════════════════════════════════════════════
# Phase 9: Special Cases
# ═══════════════════════════════════════════════════════════════════════
header "Phase 9: Special Cases"

# Disabled session should ALWAYS allow (bypasses entire cascade)
assert_decision \
	"disabled session Write src/main.rs" "allow" \
	"e2e-disabled" "Write" '{"file_path":"src/main.rs","content":"fn main() {}"}'

assert_decision \
	"disabled session Write .env" "allow" \
	"e2e-disabled" "Write" '{"file_path":".env","content":"SECRET=x"}'

# Unregistered session should DENY
assert_decision \
	"unregistered session Write src/main.rs" "deny" \
	"e2e-unregistered-$(date +%s)" "Write" '{"file_path":"src/main.rs","content":"fn main() {}"}'

# ═══════════════════════════════════════════════════════════════════════
# Phase 10: Hook Output Format Tests
# ═══════════════════════════════════════════════════════════════════════
header "Phase 10: Output Format Tests"

# Claude format: should have hookSpecificOutput.permissionDecision
claude_output=$(echo '{"session_id":"e2e-coder","tool_name":"Write","tool_input":{"file_path":"src/main.rs","content":"x"},"cwd":"'"$E2E_DIR"'"}' |
	"$BINARY" check --format=claude 2>/dev/null) || true

if echo "$claude_output" | grep -q '"permissionDecision"'; then
	pass "Claude format includes permissionDecision"
else
	fail "Claude format missing permissionDecision (output: $claude_output)"
fi

# Gemini format: should have flat {"decision":"..."}
gemini_output=$(echo '{"session_id":"e2e-coder","tool_name":"Write","tool_input":{"file_path":"src/main.rs","content":"x"},"cwd":"'"$E2E_DIR"'"}' |
	"$BINARY" check --format=gemini 2>/dev/null) || true

if echo "$gemini_output" | grep -q '"decision"'; then
	pass "Gemini format includes decision field"
else
	fail "Gemini format missing decision field (output: $gemini_output)"
fi

# Deny exit code: Claude format uses exit 1
deny_exit=0
echo '{"session_id":"e2e-coder","tool_name":"Write","tool_input":{"file_path":"tests/test.rs","content":"x"},"cwd":"'"$E2E_DIR"'"}' |
	"$BINARY" check --format=claude 2>/dev/null || deny_exit=$?

if [ "$deny_exit" -eq 1 ]; then
	pass "Claude deny exit code is 1"
else
	fail "Claude deny exit code expected 1, got $deny_exit"
fi

# Deny exit code: Gemini format also uses exit 2
deny_exit=0
echo '{"session_id":"e2e-coder","tool_name":"Write","tool_input":{"file_path":"tests/test.rs","content":"x"},"cwd":"'"$E2E_DIR"'"}' |
	"$BINARY" check --format=gemini 2>/dev/null || deny_exit=$?

if [ "$deny_exit" -eq 2 ]; then
	pass "Gemini deny exit code is 2"
else
	fail "Gemini deny exit code expected 2, got $deny_exit"
fi

# ═══════════════════════════════════════════════════════════════════════
# Phase 11: Plugin Structure Verification
# ═══════════════════════════════════════════════════════════════════════
header "Phase 11: Plugin Structure Verification"

# Verify marketplace.json exists and is valid JSON
if [ -f "$REPO_ROOT/.claude-plugin/marketplace.json" ] && python3 -m json.tool "$REPO_ROOT/.claude-plugin/marketplace.json" >/dev/null 2>&1; then
	pass "marketplace.json exists and is valid JSON"
else
	fail "marketplace.json missing or invalid"
fi

# Verify plugin.json exists
if [ -f "$REPO_ROOT/.claude-plugin/plugin.json" ]; then
	pass "plugin.json exists"
else
	fail "plugin.json missing"
fi

# Verify hooks/hooks.json exists and references captain-hook
if [ -f "$REPO_ROOT/hooks/hooks.json" ] && grep -q "captain-hook" "$REPO_ROOT/hooks/hooks.json"; then
	pass "hooks/hooks.json exists and references captain-hook"
else
	fail "hooks/hooks.json missing or doesn't reference captain-hook"
fi

# Verify skills exist
for skill in register disable enable switch status; do
	if [ -f "$REPO_ROOT/skills/$skill/SKILL.md" ]; then
		pass "skills/$skill/SKILL.md exists"
	else
		fail "skills/$skill/SKILL.md missing"
	fi
done

# Verify agents/supervisor.md exists
if [ -f "$REPO_ROOT/agents/supervisor.md" ]; then
	pass "agents/supervisor.md exists"
else
	fail "agents/supervisor.md missing"
fi

# ═══════════════════════════════════════════════════════════════════════
# Phase 12: Category Expansion Verification
# ═══════════════════════════════════════════════════════════════════════
header "Phase 12: Category System Verification"

# Verify the generated roles.yml uses {{macro}} syntax
if grep -q '{{source}}' "$E2E_DIR/.captain-hook/roles.yml"; then
	pass "generated roles.yml uses {{source}} macro"
else
	fail "generated roles.yml missing {{source}} macro"
fi

if grep -q '{{tests}}' "$E2E_DIR/.captain-hook/roles.yml"; then
	pass "generated roles.yml uses {{tests}} macro"
else
	fail "generated roles.yml missing {{tests}} macro"
fi

if grep -q '{{config_files}}' "$E2E_DIR/.captain-hook/roles.yml"; then
	pass "generated roles.yml uses {{config_files}} macro"
else
	fail "generated roles.yml missing {{config_files}} macro"
fi

# ═══════════════════════════════════════════════════════════════════════
# Phase 13: Cleanup
# ═══════════════════════════════════════════════════════════════════════
header "Phase 13: Cleanup"

# Uninstall plugin from test project (clean state)
if command -v claude &>/dev/null && [ "$SKIP_INSTALL" != true ]; then
	(cd "$E2E_DIR" && claude plugin uninstall "$PLUGIN_NAME" --scope project 2>/dev/null) || true
	pass "plugin uninstalled from test project"
fi

# Remove captain-hook state from test project
rm -rf "$E2E_DIR/.captain-hook"
pass "test project cleaned up"

# ═══════════════════════════════════════════════════════════════════════
# Results
# ═══════════════════════════════════════════════════════════════════════
header "Results"
echo ""
echo -e "  ${GREEN}Passed:  $PASS${NC}"
echo -e "  ${RED}Failed:  $FAIL${NC}"
echo -e "  ${YELLOW}Skipped: $SKIP${NC}"
TOTAL=$((PASS + FAIL + SKIP))
echo -e "  ${BOLD}Total:   $TOTAL${NC}"

if [ "$FAIL" -gt 0 ]; then
	echo ""
	echo -e "${RED}${BOLD}Failed tests:${NC}"
	for err in "${ERRORS[@]}"; do
		echo -e "  ${RED}• $err${NC}"
	done
	echo ""
	exit 1
else
	echo ""
	echo -e "${GREEN}${BOLD}All tests passed!${NC}"
	echo ""
	exit 0
fi
