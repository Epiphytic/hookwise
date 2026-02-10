# Category-Based Path Mapping

**Date**: 2026-02-10
**Status**: Approved

## Problem

Role path policies use hardcoded directory globs (`src/**`, `tests/**`, `.github/**`). When a project uses non-standard structure (`app/` for source, `spec/` for tests), the plugin silently fails — roles allow/deny the wrong paths.

Additionally, decisions stored in the vector/cache layer include raw file paths. If a project restructures, all historical decisions become stale.

## Design

### Category System

A new top-level `categories:` section in `roles.yml` defines named groups of globs. Each category maps a semantic concept to the project's actual directory structure.

```yaml
categories:
  source:
    - "src/**"
    - "lib/**"
  tests:
    - "tests/**"
    - "test-fixtures/**"
    - "*.test.*"
    - "*.spec.*"
    - "*_test.go"
    - "test_*.py"
    - "**/test_*.py"
    - "**/*_test.go"
  docs:
    - "docs/**"
  ci:
    - ".github/**"
    - ".gitlab-ci.yml"
    - ".circleci/**"
    - "Jenkinsfile"
    - ".buildkite/**"
  infra:
    - "*.tf"
    - "*.tfvars"
    - "terraform/**"
    - "infra/**"
    - "pulumi/**"
  config_files:
    - "Cargo.toml"
    - "Cargo.lock"
    - "package.json"
    - "package-lock.json"
    - "go.mod"
    - "go.sum"
    - "pyproject.toml"
    - "requirements*.txt"
  devops:
    - "Dockerfile*"
    - "docker-compose*"
    - ".dockerignore"
    - "Makefile"
    - ".*rc"
    - ".*rc.*"
    - ".tool-versions"
    - ".nvmrc"
    - ".python-version"
    - ".ruby-version"
    - "rust-toolchain.toml"
  test_config:
    - "jest.config.*"
    - "pytest.ini"
    - "vitest.config.*"
    - ".coveragerc"
    - "codecov.yml"
  research_output:
    - "docs/research/**"
  architecture_output:
    - "docs/architecture/**"
    - "docs/adr/**"
  plans_output:
    - "docs/plans/**"
  reviews_output:
    - "docs/reviews/**"
  security_reviews_output:
    - "docs/reviews/security/**"
  docs_output:
    - "docs/**"
    - "*.md"
    - "*.aisp"
    - "CHANGELOG.md"
    - "LICENSE"
```

Projects override categories to match their structure:

```yaml
categories:
  source:
    - "app/**"
    - "services/**"
  tests:
    - "spec/**"
    - "**/*_spec.py"
```

### Macro Expansion

Role definitions use `{{category_name}}` in glob lists. At load time, these expand to the category's patterns:

```yaml
roles:
  coder:
    name: coder
    description: |
      Autonomous implementation specialist...
    paths:
      allow_write:
        - "{{source}}"
        - "{{config_files}}"
      deny_write:
        - "{{tests}}"
        - "{{docs}}"
        - "{{ci}}"
        - "{{infra}}"
      allow_read:
        - "**"
```

After expansion, the coder's `allow_write` becomes `["src/**", "lib/**", "Cargo.toml", "Cargo.lock", ...]` — identical to today's format for GlobSet compilation.

**Expansion rules:**
- `{{name}}` entries replaced with category's glob list
- Plain strings pass through unchanged (raw globs still work alongside categories)
- Unknown `{{name}}` → hard error at load time (fail-fast)
- No nesting: categories cannot reference other categories

### Path Normalization for Storage

When persisting `DecisionRecord` entries and building cache keys / embedding text, file paths are normalized to `<category>:<relative_path>`:

- `src/main.rs` → `source:main.rs`
- `docs/reviews/security/audit.md` → `security_reviews_output:audit.md`
- `random-file.txt` → `random-file.txt` (no matching category, passed through)

This makes decisions portable across projects with different directory structures but the same semantic categories.

**Specificity ordering**: Categories are matched most-specific-first by glob pattern count/depth. `security_reviews_output` (`docs/reviews/security/**`) matches before `reviews_output` (`docs/reviews/**`) which matches before `docs` (`docs/**`).

### Backward Compatibility

- `categories:` section is optional. Existing `roles.yml` files without it work unchanged.
- Roles can mix `{{category}}` references with raw globs.
- Existing JSONL rules with raw paths continue to work. Normalization applies to new decisions only.
- `captain-hook build` could optionally re-normalize old records in a future version.

### Security

- `.captain-hook/**` is on the sensitive `ask_write` list, so `roles.yml` (containing categories) requires human approval to modify.
- Category names validated at load time: `[a-z][a-z0-9_]*` only.
- `PathNormalizer` derived from categories at load time — cannot be influenced by runtime tool input.

## Implementation

### Step 1: Add categories to RolesConfig

**File**: `src/config/roles.rs`

- Add `categories: Option<HashMap<String, Vec<String>>>` to `RolesConfig`
- Add `default_categories() -> HashMap<String, Vec<String>>` with built-in defaults
- Add `expand_categories(&mut self)` method that:
  1. Merges user categories over defaults (user overrides win)
  2. Walks each role's `PathPolicyConfig` lists
  3. Replaces `{{name}}` entries with category globs
  4. Errors on unknown `{{name}}`
- Call `expand_categories()` in `RolesConfig::load_from()` after YAML parse

### Step 2: Add PathNormalizer

**File**: `src/config/roles.rs` (or new `src/config/normalizer.rs`)

- `PathNormalizer` struct: holds `Vec<(String, GlobSet)>` sorted most-specific-first
- `PathNormalizer::new(categories: &HashMap<String, Vec<String>>) -> Result<Self>`
- `PathNormalizer::normalize(&self, path: &str) -> String`: returns `category:relative` or raw path
- Specificity: sort by longest glob prefix / most pattern segments

### Step 3: Integrate normalization into cascade

**File**: `src/cascade/mod.rs`, `src/decision.rs`

- `CascadeRunner` holds a `PathNormalizer`
- Before building `CacheKey`, normalize `file_path` and `sanitized_input` file references
- `DecisionRecord.file_path` stores the normalized form
- `ExactCache` and `TokenJaccard` see normalized paths

### Step 4: Update init command

**File**: `src/cli/init.rs`

- Generated `roles.yml` uses `categories:` section + `{{macro}}` syntax
- Default categories included

### Step 5: Update existing roles.yml

**File**: `.captain-hook/roles.yml`

- Convert this project's roles.yml to use categories + macros

### Step 6: Tests

- Unit tests for category expansion (happy path, unknown category error, empty category, mixed raw+macro)
- Unit tests for PathNormalizer (specificity ordering, no-match passthrough, relative path output)
- Integration test: load roles.yml with categories, verify compiled GlobSet matches correctly
- Existing path_policy_tests and cascade_integration tests must still pass
