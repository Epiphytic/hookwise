use std::fs;
use std::path::PathBuf;

use crate::error::Result;

/// Initialize .captain-hook/ in the current repo.
pub async fn run() -> Result<()> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let hook_dir = cwd.join(".captain-hook");

    if hook_dir.exists() {
        eprintln!(
            "captain-hook: .captain-hook/ already exists in {}",
            cwd.display()
        );
        return Ok(());
    }

    // Create directory structure
    fs::create_dir_all(hook_dir.join("rules"))?;
    fs::create_dir_all(hook_dir.join(".index"))?;
    fs::create_dir_all(hook_dir.join(".user"))?;

    // Write default policy.yml
    let policy_content = r#"# captain-hook project policy
# See docs for full configuration reference.

sensitive_paths:
  ask_write:
    - ".claude/**"
    - ".captain-hook/**"
    - ".env*"
    - "**/.env*"
    - ".git/hooks/**"
    - "**/secrets/**"
    - "~/.claude/**"
    - "~/.config/**"

confidence:
  org: 0.9
  project: 0.7
  user: 0.6

similarity:
  jaccard_threshold: 0.7
  embedding_threshold: 0.85
  jaccard_min_tokens: 3

human_timeout_secs: 60
registration_timeout_secs: 5

supervisor:
  backend: socket
"#;
    fs::write(hook_dir.join("policy.yml"), policy_content)?;

    // Write default roles.yml
    let roles_content = r#"# captain-hook role definitions
# Each role has path policies and a description for the LLM supervisor.
#
# Categories define semantic path groups. Override them to match your project:
#   categories:
#     source:
#       - "app/**"
#       - "services/**"
#
# Use {{category_name}} in role path lists to reference categories.

# Override built-in defaults here (optional). Omit to use defaults.
# categories:
#   source:
#     - "src/**"
#     - "lib/**"

roles:
  coder:
    name: coder
    description: "Implementation role: writes source code and project config"
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

  tester:
    name: tester
    description: "Testing role: writes tests and test fixtures"
    paths:
      allow_write:
        - "{{tests}}"
        - "{{test_config}}"
      deny_write:
        - "{{source}}"
        - "{{docs}}"
        - "{{ci}}"
        - "{{infra}}"
      allow_read:
        - "**"

  maintainer:
    name: maintainer
    description: "Full-access role: unrestricted permissions"
    paths:
      allow_write:
        - "**"
      deny_write: []
      allow_read:
        - "**"
"#;
    fs::write(hook_dir.join("roles.yml"), roles_content)?;

    // Write .gitignore for local-only directories
    let gitignore_content = ".index/\n.user/\n";
    fs::write(hook_dir.join(".gitignore"), gitignore_content)?;

    // Create empty rule files
    fs::write(hook_dir.join("rules").join("allow.jsonl"), "")?;
    fs::write(hook_dir.join("rules").join("deny.jsonl"), "")?;
    fs::write(hook_dir.join("rules").join("ask.jsonl"), "")?;

    eprintln!(
        "captain-hook: initialized .captain-hook/ in {}",
        cwd.display()
    );
    eprintln!("  policy.yml  -- project policy (sensitive paths, thresholds)");
    eprintln!("  roles.yml   -- role definitions with path policies");
    eprintln!("  rules/      -- cached decisions (allow.jsonl, deny.jsonl, ask.jsonl)");
    eprintln!("  .index/     -- HNSW vector indexes (gitignored)");
    eprintln!("  .user/      -- personal preferences (gitignored)");

    Ok(())
}
