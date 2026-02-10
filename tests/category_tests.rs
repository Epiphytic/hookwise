//! Tests for the category system: macro expansion, PathNormalizer,
//! and RolesConfig loading with categories.

use std::collections::HashMap;

use captain_hook::config::roles::{default_categories, PathNormalizer, RolesConfig};

// ---------------------------------------------------------------------------
// default_categories()
// ---------------------------------------------------------------------------

#[test]
fn default_categories_contains_expected_keys() {
    let cats = default_categories();
    let expected = [
        "source",
        "tests",
        "docs",
        "ci",
        "infra",
        "config_files",
        "devops",
        "test_config",
        "research_output",
        "architecture_output",
        "plans_output",
        "reviews_output",
        "security_reviews_output",
        "docs_output",
    ];
    for key in &expected {
        assert!(cats.contains_key(*key), "missing category: {}", key);
    }
}

#[test]
fn default_categories_source_contains_src() {
    let cats = default_categories();
    let source = &cats["source"];
    assert!(source.contains(&"src/**".to_string()));
    assert!(source.contains(&"lib/**".to_string()));
}

// ---------------------------------------------------------------------------
// Macro expansion via RolesConfig::load_from()
// ---------------------------------------------------------------------------

#[test]
fn roles_config_expands_macros() {
    let yaml = r#"
roles:
  coder:
    name: coder
    description: "test coder"
    paths:
      allow_write:
        - "{{source}}"
        - "{{config_files}}"
      deny_write:
        - "{{tests}}"
      allow_read:
        - "**"
"#;
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), yaml).unwrap();

    let config = RolesConfig::load_from(tmp.path()).unwrap();
    let coder = config.get_role("coder").unwrap();

    // allow_write should have expanded {{source}} and {{config_files}}
    assert!(
        coder.paths.allow_write.contains(&"src/**".to_string()),
        "should contain src/** from {{{{source}}}} expansion"
    );
    assert!(
        coder.paths.allow_write.contains(&"lib/**".to_string()),
        "should contain lib/** from {{{{source}}}} expansion"
    );
    assert!(
        coder.paths.allow_write.contains(&"Cargo.toml".to_string()),
        "should contain Cargo.toml from {{{{config_files}}}} expansion"
    );

    // deny_write should have expanded {{tests}}
    assert!(
        coder.paths.deny_write.contains(&"tests/**".to_string()),
        "should contain tests/** from {{{{tests}}}} expansion"
    );
}

#[test]
fn roles_config_preserves_raw_globs_alongside_macros() {
    let yaml = r#"
roles:
  custom:
    name: custom
    description: "test"
    paths:
      allow_write:
        - "{{source}}"
        - "custom-dir/**"
      deny_write: []
      allow_read:
        - "**"
"#;
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), yaml).unwrap();

    let config = RolesConfig::load_from(tmp.path()).unwrap();
    let custom = config.get_role("custom").unwrap();

    assert!(custom.paths.allow_write.contains(&"src/**".to_string()));
    assert!(
        custom
            .paths
            .allow_write
            .contains(&"custom-dir/**".to_string()),
        "raw globs should pass through unchanged"
    );
}

#[test]
fn roles_config_unknown_macro_is_hard_error() {
    let yaml = r#"
roles:
  bad:
    name: bad
    description: "test"
    paths:
      allow_write:
        - "{{nonexistent_category}}"
      deny_write: []
      allow_read:
        - "**"
"#;
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), yaml).unwrap();

    let result = RolesConfig::load_from(tmp.path());
    assert!(result.is_err(), "unknown category should be a hard error");
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("nonexistent_category"),
        "error should mention the unknown category: {}",
        err
    );
}

#[test]
fn roles_config_user_categories_override_defaults() {
    let yaml = r#"
categories:
  source:
    - "app/**"
    - "services/**"

roles:
  coder:
    name: coder
    description: "test"
    paths:
      allow_write:
        - "{{source}}"
      deny_write: []
      allow_read:
        - "**"
"#;
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), yaml).unwrap();

    let config = RolesConfig::load_from(tmp.path()).unwrap();
    let coder = config.get_role("coder").unwrap();

    // User override should win -- no "src/**", should have "app/**"
    assert!(
        !coder.paths.allow_write.contains(&"src/**".to_string()),
        "default src/** should be overridden"
    );
    assert!(
        coder.paths.allow_write.contains(&"app/**".to_string()),
        "user override app/** should be present"
    );
    assert!(
        coder.paths.allow_write.contains(&"services/**".to_string()),
        "user override services/** should be present"
    );
}

#[test]
fn roles_config_empty_categories_section_uses_defaults() {
    let yaml = r#"
categories: {}

roles:
  coder:
    name: coder
    description: "test"
    paths:
      allow_write:
        - "{{source}}"
      deny_write: []
      allow_read:
        - "**"
"#;
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), yaml).unwrap();

    let config = RolesConfig::load_from(tmp.path()).unwrap();
    let coder = config.get_role("coder").unwrap();

    // With empty categories section, defaults should still be used
    assert!(coder.paths.allow_write.contains(&"src/**".to_string()));
}

#[test]
fn roles_config_no_categories_section_uses_defaults() {
    let yaml = r#"
roles:
  coder:
    name: coder
    description: "test"
    paths:
      allow_write:
        - "{{source}}"
      deny_write: []
      allow_read:
        - "**"
"#;
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), yaml).unwrap();

    let config = RolesConfig::load_from(tmp.path()).unwrap();
    let coder = config.get_role("coder").unwrap();

    // No categories section at all: defaults should be used
    assert!(coder.paths.allow_write.contains(&"src/**".to_string()));
}

#[test]
fn roles_config_missing_file_returns_empty() {
    let result =
        RolesConfig::load_from(std::path::Path::new("/nonexistent/path/roles.yml")).unwrap();
    assert!(result.roles.is_empty());
}

// ---------------------------------------------------------------------------
// PathNormalizer
// ---------------------------------------------------------------------------

#[test]
fn normalizer_source_category() {
    let cats = default_categories();
    let normalizer = PathNormalizer::new(&cats).unwrap();

    assert_eq!(normalizer.normalize("src/main.rs"), "source:main.rs");
    assert_eq!(
        normalizer.normalize("src/config/mod.rs"),
        "source:config/mod.rs"
    );
    assert_eq!(normalizer.normalize("lib/utils.rs"), "source:utils.rs");
}

#[test]
fn normalizer_tests_category() {
    let cats = default_categories();
    let normalizer = PathNormalizer::new(&cats).unwrap();

    assert_eq!(normalizer.normalize("tests/unit.rs"), "tests:unit.rs");
    assert_eq!(
        normalizer.normalize("test-fixtures/data.json"),
        "tests:data.json"
    );
}

#[test]
fn normalizer_docs_specificity_ordering() {
    let cats = default_categories();
    let normalizer = PathNormalizer::new(&cats).unwrap();

    // Most specific category should match first
    assert_eq!(
        normalizer.normalize("docs/reviews/security/audit.md"),
        "security_reviews_output:audit.md"
    );
    assert_eq!(
        normalizer.normalize("docs/reviews/code-quality.md"),
        "reviews_output:code-quality.md"
    );
    assert_eq!(
        normalizer.normalize("docs/research/findings.md"),
        "research_output:findings.md"
    );
    assert_eq!(
        normalizer.normalize("docs/architecture/design.md"),
        "architecture_output:design.md"
    );
    assert_eq!(
        normalizer.normalize("docs/adr/0001-decision.md"),
        "architecture_output:0001-decision.md"
    );
    assert_eq!(
        normalizer.normalize("docs/plans/sprint-1.md"),
        "plans_output:sprint-1.md"
    );
}

#[test]
fn normalizer_no_match_passthrough() {
    let cats = default_categories();
    let normalizer = PathNormalizer::new(&cats).unwrap();

    // A file that doesn't match any category should pass through unchanged
    assert_eq!(normalizer.normalize("random-file.txt"), "random-file.txt");
}

#[test]
fn normalizer_ci_category() {
    let cats = default_categories();
    let normalizer = PathNormalizer::new(&cats).unwrap();

    assert_eq!(
        normalizer.normalize(".github/workflows/ci.yml"),
        "ci:workflows/ci.yml"
    );
}

#[test]
fn normalizer_infra_category() {
    let cats = default_categories();
    let normalizer = PathNormalizer::new(&cats).unwrap();

    assert_eq!(normalizer.normalize("terraform/main.tf"), "infra:main.tf");
    assert_eq!(normalizer.normalize("infra/network.tf"), "infra:network.tf");
}

#[test]
fn normalizer_file_level_patterns() {
    let cats = default_categories();
    let normalizer = PathNormalizer::new(&cats).unwrap();

    // File-level patterns (e.g., Cargo.toml, *.tf) match but return path as-is
    // since there's no directory prefix to strip
    let result = normalizer.normalize("Cargo.toml");
    assert!(
        result.starts_with("config_files:"),
        "Cargo.toml should match config_files category: {}",
        result
    );
}

#[test]
fn normalizer_custom_categories() {
    let mut cats = HashMap::new();
    cats.insert("mycode".into(), vec!["app/**".into(), "services/**".into()]);

    let normalizer = PathNormalizer::new(&cats).unwrap();

    assert_eq!(normalizer.normalize("app/main.rs"), "mycode:main.rs");
    assert_eq!(
        normalizer.normalize("services/auth/handler.rs"),
        "mycode:auth/handler.rs"
    );
    // No match
    assert_eq!(normalizer.normalize("src/main.rs"), "src/main.rs");
}

#[test]
fn normalizer_empty_categories() {
    let cats = HashMap::new();
    let normalizer = PathNormalizer::new(&cats).unwrap();

    // Everything passes through
    assert_eq!(normalizer.normalize("src/main.rs"), "src/main.rs");
}

// ---------------------------------------------------------------------------
// Integration: load project roles.yml with categories
// ---------------------------------------------------------------------------

#[test]
fn project_roles_yml_loads_with_categories() {
    // Load the actual project's roles.yml and verify it works
    let project_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let config = RolesConfig::load_project(project_root).unwrap();

    // Verify coder role has expanded categories
    let coder = config.get_role("coder").unwrap();
    assert!(
        coder.paths.allow_write.contains(&"src/**".to_string()),
        "coder allow_write should contain src/** after {{{{source}}}} expansion"
    );
    assert!(
        coder.paths.deny_write.contains(&"tests/**".to_string()),
        "coder deny_write should contain tests/** after {{{{tests}}}} expansion"
    );

    // Verify tester role
    let tester = config.get_role("tester").unwrap();
    assert!(
        tester.paths.allow_write.contains(&"tests/**".to_string()),
        "tester allow_write should contain tests/**"
    );

    // Verify maintainer still has raw "**"
    let maintainer = config.get_role("maintainer").unwrap();
    assert!(maintainer.paths.allow_write.contains(&"**".to_string()));

    // Verify normalizer can be built from config
    let normalizer = config.normalizer().unwrap();
    assert_eq!(normalizer.normalize("src/main.rs"), "source:main.rs");
}
