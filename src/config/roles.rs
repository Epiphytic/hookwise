use globset::GlobSet;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::error::{CaptainHookError, Result};

/// A role definition from `roles.yml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoleDefinition {
    /// Role name (e.g., "coder", "tester", "maintainer").
    pub name: String,

    /// Natural language description of the role.
    pub description: String,

    /// Deterministic path policies for this role.
    pub paths: PathPolicyConfig,
}

/// Raw path policy from YAML (string globs, before compilation).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PathPolicyConfig {
    pub allow_write: Vec<String>,
    pub deny_write: Vec<String>,
    pub allow_read: Vec<String>,
}

/// Compiled path policy -- globset instances ready for matching.
/// GlobSet doesn't implement Debug, so we implement it manually.
pub struct CompiledPathPolicy {
    pub allow_write: GlobSet,
    pub deny_write: GlobSet,
    pub allow_read: GlobSet,
    pub sensitive_ask_write: GlobSet,
}

impl std::fmt::Debug for CompiledPathPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompiledPathPolicy")
            .field("allow_write", &"<GlobSet>")
            .field("deny_write", &"<GlobSet>")
            .field("allow_read", &"<GlobSet>")
            .field("sensitive_ask_write", &"<GlobSet>")
            .finish()
    }
}

impl CompiledPathPolicy {
    /// Compile a PathPolicyConfig into GlobSet instances.
    pub fn compile(config: &PathPolicyConfig, sensitive_patterns: &[String]) -> Result<Self> {
        let allow_write = build_globset(&config.allow_write)?;
        let deny_write = build_globset(&config.deny_write)?;
        let allow_read = build_globset(&config.allow_read)?;
        let sensitive_ask_write = build_globset(sensitive_patterns)?;

        Ok(Self {
            allow_write,
            deny_write,
            allow_read,
            sensitive_ask_write,
        })
    }
}

fn build_globset(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = globset::GlobSetBuilder::new();
    for pattern in patterns {
        let glob = globset::Glob::new(pattern).map_err(|e| CaptainHookError::GlobPattern {
            pattern: pattern.clone(),
            reason: e.to_string(),
        })?;
        builder.add(glob);
    }
    builder.build().map_err(|e| CaptainHookError::GlobPattern {
        pattern: String::new(),
        reason: e.to_string(),
    })
}

// ---------------------------------------------------------------------------
// Categories: semantic path groups with {{macro}} expansion
// ---------------------------------------------------------------------------

/// Built-in default categories. Projects can override any of these
/// in the `categories:` section of `roles.yml`.
pub fn default_categories() -> HashMap<String, Vec<String>> {
    let mut m = HashMap::new();
    m.insert("source".into(), vec!["src/**".into(), "lib/**".into()]);
    m.insert(
        "tests".into(),
        vec![
            "tests/**".into(),
            "test-fixtures/**".into(),
            "*.test.*".into(),
            "*.spec.*".into(),
            "*_test.go".into(),
            "test_*.py".into(),
            "**/test_*.py".into(),
            "**/*_test.go".into(),
        ],
    );
    m.insert("docs".into(), vec!["docs/**".into()]);
    m.insert(
        "ci".into(),
        vec![
            ".github/**".into(),
            ".gitlab-ci.yml".into(),
            ".circleci/**".into(),
            "Jenkinsfile".into(),
            ".buildkite/**".into(),
        ],
    );
    m.insert(
        "infra".into(),
        vec![
            "*.tf".into(),
            "*.tfvars".into(),
            "*.hcl".into(),
            "terraform/**".into(),
            "infra/**".into(),
            "pulumi/**".into(),
            "cdk/**".into(),
            "cloudformation/**".into(),
            "ansible/**".into(),
            "helm/**".into(),
            ".terraform.lock.hcl".into(),
        ],
    );
    m.insert(
        "config_files".into(),
        vec![
            "Cargo.toml".into(),
            "Cargo.lock".into(),
            "package.json".into(),
            "package-lock.json".into(),
            "go.mod".into(),
            "go.sum".into(),
            "pyproject.toml".into(),
            "requirements*.txt".into(),
        ],
    );
    m.insert(
        "devops".into(),
        vec![
            "Dockerfile*".into(),
            "docker-compose*".into(),
            ".dockerignore".into(),
            "Makefile".into(),
            ".eslintrc*".into(),
            ".prettierrc*".into(),
            ".editorconfig".into(),
            "tsconfig*".into(),
            ".*rc".into(),
            ".*rc.*".into(),
            ".tool-versions".into(),
            ".nvmrc".into(),
            ".python-version".into(),
            ".ruby-version".into(),
            "rust-toolchain.toml".into(),
            "lefthook.yml".into(),
            ".husky/**".into(),
            ".pre-commit-config.yaml".into(),
        ],
    );
    m.insert(
        "test_config".into(),
        vec![
            "jest.config.*".into(),
            "pytest.ini".into(),
            "vitest.config.*".into(),
            ".coveragerc".into(),
            "codecov.yml".into(),
        ],
    );
    m.insert("research_output".into(), vec!["docs/research/**".into()]);
    m.insert(
        "architecture_output".into(),
        vec!["docs/architecture/**".into(), "docs/adr/**".into()],
    );
    m.insert("plans_output".into(), vec!["docs/plans/**".into()]);
    m.insert("reviews_output".into(), vec!["docs/reviews/**".into()]);
    m.insert(
        "security_reviews_output".into(),
        vec!["docs/reviews/security/**".into()],
    );
    m.insert(
        "docs_output".into(),
        vec![
            "docs/**".into(),
            "*.md".into(),
            "*.aisp".into(),
            "CHANGELOG.md".into(),
            "LICENSE".into(),
        ],
    );
    m
}

/// Regex for matching `{{category_name}}` macros in glob lists.
fn macro_regex() -> regex::Regex {
    regex::Regex::new(r"^\{\{([a-z][a-z0-9_]*)\}\}$").expect("macro regex is valid")
}

/// Expand `{{category_name}}` macros in a list of glob patterns.
fn expand_macros(
    patterns: &[String],
    categories: &HashMap<String, Vec<String>>,
    role_name: &str,
) -> Result<Vec<String>> {
    let re = macro_regex();
    let mut expanded = Vec::new();

    for pattern in patterns {
        if let Some(caps) = re.captures(pattern) {
            let name = &caps[1];
            match categories.get(name) {
                Some(cat_patterns) => expanded.extend(cat_patterns.iter().cloned()),
                None => {
                    return Err(CaptainHookError::ConfigParse {
                        path: PathBuf::from("roles.yml"),
                        reason: format!(
                            "role '{}': unknown category '{{{{{}}}}}'. Available: {:?}",
                            role_name,
                            name,
                            categories.keys().collect::<Vec<_>>()
                        ),
                    })
                }
            }
        } else {
            expanded.push(pattern.clone());
        }
    }

    Ok(expanded)
}

// ---------------------------------------------------------------------------
// PathNormalizer: maps raw file paths to category:relative form
// ---------------------------------------------------------------------------

/// Normalizes file paths to `category:relative` form for portable storage.
///
/// Categories are matched most-specific-first (by glob pattern depth).
/// For example, `docs/reviews/security/audit.md` normalizes to
/// `security_reviews_output:audit.md` rather than `docs:reviews/security/audit.md`.
pub struct PathNormalizer {
    /// (category_name, GlobSet, patterns) sorted most-specific-first.
    categories: Vec<(String, GlobSet, Vec<String>)>,
}

impl PathNormalizer {
    pub fn new(categories: &HashMap<String, Vec<String>>) -> Result<Self> {
        let mut entries = Vec::new();

        for (name, patterns) in categories {
            if patterns.is_empty() {
                continue;
            }
            let globset = build_globset(patterns)?;
            entries.push((name.clone(), globset, patterns.clone()));
        }

        // Sort by specificity: max slash-depth of glob patterns, descending.
        // "docs/reviews/security/**" (depth 3) before "docs/**" (depth 1).
        entries.sort_by(|a, b| {
            let depth_a =
                a.2.iter()
                    .map(|p| p.matches('/').count())
                    .max()
                    .unwrap_or(0);
            let depth_b =
                b.2.iter()
                    .map(|p| p.matches('/').count())
                    .max()
                    .unwrap_or(0);
            depth_b.cmp(&depth_a).then_with(|| a.0.cmp(&b.0))
        });

        Ok(Self {
            categories: entries,
        })
    }

    /// Normalize a file path to `category:relative` form.
    /// Returns the original path if no category matches.
    pub fn normalize(&self, path: &str) -> String {
        for (name, globset, patterns) in &self.categories {
            if globset.is_match(path) {
                let relative = Self::strip_category_prefix(path, patterns);
                return format!("{}:{}", name, relative);
            }
        }
        path.to_string()
    }

    /// Strip the category's directory prefix from the path.
    /// For `src/**` patterns, strips `src/` prefix.
    /// For file-level patterns (e.g. `Cargo.toml`, `*.test.*`), returns path as-is.
    fn strip_category_prefix(path: &str, patterns: &[String]) -> String {
        let mut best_relative = path.to_string();
        let mut best_prefix_len = 0;

        for pattern in patterns {
            // Extract directory prefix from "dir/**" or "dir/sub/**" patterns
            if let Some(prefix) = pattern.strip_suffix("/**") {
                if path.starts_with(prefix) && prefix.len() > best_prefix_len {
                    let rest = &path[prefix.len()..];
                    let rest = rest.strip_prefix('/').unwrap_or(rest);
                    if !rest.is_empty() {
                        best_relative = rest.to_string();
                        best_prefix_len = prefix.len();
                    }
                }
            }
        }

        best_relative
    }
}

// ---------------------------------------------------------------------------
// RolesConfig
// ---------------------------------------------------------------------------

/// Roles configuration loaded from roles.yml.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RolesConfig {
    /// Semantic path categories. Merged over built-in defaults.
    #[serde(default)]
    pub categories: HashMap<String, Vec<String>>,

    pub roles: HashMap<String, RoleDefinition>,
}

impl RolesConfig {
    /// Load roles from a YAML file. Expands `{{category}}` macros.
    pub fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self {
                categories: HashMap::new(),
                roles: HashMap::new(),
            });
        }
        let contents = std::fs::read_to_string(path)?;
        let mut config: Self =
            serde_yaml::from_str(&contents).map_err(|e| CaptainHookError::ConfigParse {
                path: path.to_path_buf(),
                reason: e.to_string(),
            })?;
        config.expand_categories()?;
        Ok(config)
    }

    /// Load roles from the project root. Checks `.captain-hook/roles.yml`.
    pub fn load_project(project_root: &Path) -> Result<Self> {
        let path = project_root.join(".captain-hook").join("roles.yml");
        Self::load_from(&path)
    }

    /// Look up a role by name.
    pub fn get_role(&self, name: &str) -> Option<&RoleDefinition> {
        self.roles.get(name)
    }

    /// Build a PathNormalizer from this config's categories.
    pub fn normalizer(&self) -> Result<PathNormalizer> {
        PathNormalizer::new(&self.categories)
    }

    /// Merge user categories over defaults, then expand macros in all roles.
    fn expand_categories(&mut self) -> Result<()> {
        let merged = self.merged_categories();

        for (role_name, role) in &mut self.roles {
            role.paths.allow_write = expand_macros(&role.paths.allow_write, &merged, role_name)?;
            role.paths.deny_write = expand_macros(&role.paths.deny_write, &merged, role_name)?;
            role.paths.allow_read = expand_macros(&role.paths.allow_read, &merged, role_name)?;
        }

        // Store the merged categories for normalizer use
        self.categories = merged;
        Ok(())
    }

    /// Merge user-specified categories over built-in defaults.
    fn merged_categories(&self) -> HashMap<String, Vec<String>> {
        let mut merged = default_categories();
        for (name, patterns) in &self.categories {
            merged.insert(name.clone(), patterns.clone());
        }
        merged
    }
}
