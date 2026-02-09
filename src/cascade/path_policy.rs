use std::path::Path;

use async_trait::async_trait;
use chrono::Utc;

use crate::cascade::{CascadeInput, CascadeTier};
use crate::decision::{
    CacheKey, Decision, DecisionMetadata, DecisionRecord, DecisionTier, ScopeLevel,
};
use crate::error::Result;

/// Tier 0: Deterministic path policy check.
pub struct PathPolicyEngine {
    /// Regex patterns for extracting file paths from Bash commands.
    bash_path_extractors: Vec<regex::Regex>,
}

impl PathPolicyEngine {
    pub fn new() -> Result<Self> {
        let patterns = vec![
            // rm: extract first path after flags
            r#"(?:^|[;&|]\s*)rm\s+(?:-[rifvdIRP]+\s+)*(?:"([^"]+)"|'([^']+)'|((?:[/~.]|\w)[\w./_~*?\[\]{}-]*))"#,
            // mv: extract src and dst
            r#"(?:^|[;&|]\s*)mv\s+(?:-[fintuvTSZ]+\s+)*(?:"([^"]+)"|'([^']+)'|((?:[/~.]|\w)[\w./_~*?\[\]{}-]*))\s+(?:"([^"]+)"|'([^']+)'|((?:[/~.]|\w)[\w./_~*?\[\]{}-]*))"#,
            // cp: extract src and dst
            r#"(?:^|[;&|]\s*)cp\s+(?:-[raflinpuvRPdHLsxTZ]+\s+)*(?:"([^"]+)"|'([^']+)'|((?:[/~.]|\w)[\w./_~*?\[\]{}-]*))\s+(?:"([^"]+)"|'([^']+)'|((?:[/~.]|\w)[\w./_~*?\[\]{}-]*))"#,
            // mkdir: extract directory path
            r#"(?:^|[;&|]\s*)mkdir\s+(?:-[pmvZ]+\s+)*(?:"([^"]+)"|'([^']+)'|((?:[/~.]|\w)[\w./_~*?\[\]{}-]*))"#,
            // touch: extract file path
            r#"(?:^|[;&|]\s*)touch\s+(?:-[acmr]+\s+(?:\S+\s+)?)*(?:"([^"]+)"|'([^']+)'|((?:[/~.]|\w)[\w./_~*?\[\]{}-]*))"#,
            // Output redirects (> and >>)
            r#">{1,2}\s*(?:"([^"]+)"|'([^']+)'|((?:[/~.]|\w)[\w./_~*?\[\]{}-]*))"#,
            // tee
            r#"\|\s*tee\s+(?:-[ai]+\s+)*(?:"([^"]+)"|'([^']+)'|((?:[/~.]|\w)[\w./_~*?\[\]{}-]*))"#,
            // sed -i
            r#"(?:^|[;&|]\s*)sed\s+(?:-[nEerz]+\s+)*-i(?:\.\S+)?\s+(?:'[^']*'|"[^"]*"|\S+)\s+(?:"([^"]+)"|'([^']+)'|((?:[/~.]|\w)[\w./_~*?\[\]{}-]*))"#,
            // chmod
            r#"(?:^|[;&|]\s*)chmod\s+(?:-[RfvcH]+\s+)*(?:\+?[rwxXstugo0-7,]+)\s+(?:"([^"]+)"|'([^']+)'|((?:[/~.]|\w)[\w./_~*?\[\]{}-]*))"#,
            // chown
            r#"(?:^|[;&|]\s*)chown\s+(?:-[RfvcHhLP]+\s+)*(?:[\w.:-]+)\s+(?:"([^"]+)"|'([^']+)'|((?:[/~.]|\w)[\w./_~*?\[\]{}-]*))"#,
            // git checkout -- <path>
            r#"(?:^|[;&|]\s*)git\s+checkout\s+(?:-[bBfqm]+\s+)*--\s+(?:"([^"]+)"|'([^']+)'|((?:[/~.]|\w)[\w./_~*?\[\]{}-]*))"#,
            // curl -o
            r#"curl\s+.*?(?:-o|--output)\s+(?:"([^"]+)"|'([^']+)'|((?:[/~.]|\w)[\w./_~*?\[\]{}-]*))"#,
            // wget -O
            r#"wget\s+.*?(?:-O|--output-document)\s+(?:"([^"]+)"|'([^']+)'|((?:[/~.]|\w)[\w./_~*?\[\]{}-]*))"#,
            // dd of=
            r#"(?:^|[;&|]\s*)dd\s+.*?of=(?:"([^"]+)"|'([^']+)'|([^\s;&|]+))"#,
        ];

        let compiled: Vec<regex::Regex> = patterns
            .iter()
            .filter_map(|p| regex::Regex::new(p).ok())
            .collect();

        Ok(Self {
            bash_path_extractors: compiled,
        })
    }

    /// Extract write-target file paths from a Bash command string.
    fn extract_bash_paths(&self, command: &str) -> Vec<String> {
        let mut paths = Vec::new();

        for re in &self.bash_path_extractors {
            for caps in re.captures_iter(command) {
                // Each pattern has alternation groups for quoted/unquoted paths.
                // Walk all capture groups and collect non-empty matches.
                for i in 1..caps.len() {
                    if let Some(m) = caps.get(i) {
                        let path = m.as_str().trim();
                        if !path.is_empty() && path != "/dev/null" {
                            paths.push(path.to_string());
                        }
                    }
                }
            }
        }

        paths.sort();
        paths.dedup();
        paths
    }

    /// Make an absolute path relative to the cwd, for glob matching.
    /// If the path is already relative, or cwd is None, returns the path as-is.
    fn relativize(path: &str, cwd: Option<&str>) -> String {
        match cwd {
            Some(cwd) => {
                let p = Path::new(path);
                let c = Path::new(cwd);
                p.strip_prefix(c)
                    .map(|rel| rel.to_string_lossy().to_string())
                    .unwrap_or_else(|_| path.to_string())
            }
            None => path.to_string(),
        }
    }

    /// Extract file paths from tool input depending on tool type.
    fn extract_paths(&self, tool_name: &str, input: &CascadeInput) -> Vec<String> {
        match tool_name {
            "Write" | "Edit" | "Read" | "Glob" | "Grep" => {
                if let Some(fp) = &input.file_path {
                    vec![fp.clone()]
                } else {
                    Vec::new()
                }
            }
            "Bash" => {
                let command = input
                    .tool_input
                    .get("command")
                    .and_then(|v| v.as_str())
                    .unwrap_or(&input.sanitized_input);
                self.extract_bash_paths(command)
            }
            _ => Vec::new(),
        }
    }
}

#[async_trait]
impl CascadeTier for PathPolicyEngine {
    async fn evaluate(&self, input: &CascadeInput) -> Result<Option<DecisionRecord>> {
        let policy = match &input.session.path_policy {
            Some(p) => p,
            None => return Ok(None), // No role/policy = no path policy to evaluate
        };

        let raw_paths = self.extract_paths(&input.tool_name, input);
        if raw_paths.is_empty() {
            return Ok(None); // No file paths extracted = fall through
        }

        // Relativize absolute paths against cwd so globs like "src/**" can match.
        let paths: Vec<String> = raw_paths
            .iter()
            .map(|p| Self::relativize(p, input.cwd.as_deref()))
            .collect();

        let is_read_only =
            input.tool_name == "Read" || input.tool_name == "Glob" || input.tool_name == "Grep";

        // Evaluate each path against the policy. Most restrictive wins.
        let mut worst_decision: Option<Decision> = None;
        let mut worst_path = String::new();
        let mut worst_reason = String::new();

        for path in &paths {
            let decision = if is_read_only {
                // For read operations, check sensitive paths first, then allow_read
                if policy.sensitive_ask_write.is_match(path) {
                    Some(Decision::Ask) // Sensitive path read requires human approval
                } else if policy.allow_read.is_match(path) {
                    None // Allowed, no policy action needed
                } else {
                    Some(Decision::Deny)
                }
            } else {
                // For write operations, check in order:
                // 1. sensitive_ask_write -> Ask
                // 2. deny_write -> Deny
                // 3. allow_write -> Allow
                if policy.sensitive_ask_write.is_match(path) {
                    Some(Decision::Ask)
                } else if policy.deny_write.is_match(path) {
                    Some(Decision::Deny)
                } else if policy.allow_write.is_match(path) {
                    Some(Decision::Allow)
                } else {
                    None // No match = fall through
                }
            };

            if let Some(d) = decision {
                let dominated = match (&worst_decision, &d) {
                    (None, _) => true,
                    (Some(current), new) => new.precedence() > current.precedence(),
                };
                if dominated {
                    worst_decision = Some(d);
                    worst_path = path.clone();
                    worst_reason = match d {
                        Decision::Deny => format!("path '{}' denied by role path policy", path),
                        Decision::Ask => format!("path '{}' matches sensitive path pattern", path),
                        Decision::Allow => format!("path '{}' allowed by role path policy", path),
                    };
                }
            }
        }

        match worst_decision {
            Some(decision) => {
                let role_name = input
                    .session
                    .role
                    .as_ref()
                    .map(|r| r.name.clone())
                    .unwrap_or_else(|| "*".to_string());

                Ok(Some(DecisionRecord {
                    key: CacheKey {
                        sanitized_input: input.sanitized_input.clone(),
                        tool: input.tool_name.clone(),
                        role: role_name,
                    },
                    decision,
                    metadata: DecisionMetadata {
                        tier: DecisionTier::PathPolicy,
                        confidence: 1.0,
                        reason: worst_reason,
                        matched_key: None,
                        similarity_score: None,
                    },
                    timestamp: Utc::now(),
                    scope: ScopeLevel::Role,
                    file_path: Some(worst_path),
                    session_id: String::new(), // Filled by CascadeRunner
                }))
            }
            None => Ok(None), // No path policy match = fall through
        }
    }

    fn tier(&self) -> DecisionTier {
        DecisionTier::PathPolicy
    }

    fn name(&self) -> &str {
        "path-policy"
    }
}
