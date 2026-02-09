use std::path::PathBuf;
use std::sync::Arc;

use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, ServerInfo};
use rmcp::schemars::JsonSchema;
use rmcp::service::ServiceExt;
use rmcp::{tool, tool_router, ErrorData as McpError};
use serde::Deserialize;

use crate::cascade::cache::ExactCache;
use crate::cascade::human::{load_queue_file, DecisionQueue, HumanResponse};
use crate::decision::Decision;
use crate::error::Result;
use crate::scope::ScopeLevel;
use crate::session::SessionManager;
use crate::storage::jsonl::JsonlStorage;
use crate::storage::StorageBackend;

#[derive(Clone)]
pub struct CaptainHookMcp {
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

impl Default for CaptainHookMcp {
    fn default() -> Self {
        Self::new()
    }
}

// --- Parameter types ---

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RegisterParams {
    /// Session ID to register
    pub session_id: String,
    /// Role name (e.g. coder, tester, maintainer)
    pub role: String,
    /// Optional task description
    #[serde(default)]
    pub task: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SessionIdParams {
    /// Session ID
    pub session_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ApproveParams {
    /// Pending decision ID to approve
    pub id: String,
    /// Cache as 'ask' so it always prompts
    #[serde(default)]
    pub always_ask: bool,
    /// Add as a persistent rule
    #[serde(default)]
    pub add_rule: bool,
    /// Rule scope: project, user, or org
    #[serde(default = "default_scope")]
    pub scope: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DenyParams {
    /// Pending decision ID to deny
    pub id: String,
    /// Cache as 'ask' so it always prompts
    #[serde(default)]
    pub always_ask: bool,
    /// Add as a persistent rule
    #[serde(default)]
    pub add_rule: bool,
    /// Rule scope: project, user, or org
    #[serde(default = "default_scope")]
    pub scope: String,
}

fn default_scope() -> String {
    "project".to_string()
}

// --- Tool implementations ---

#[tool_router]
impl CaptainHookMcp {
    pub fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Register a session with a role for permission gating. Each session must be registered before tool calls are permitted."
    )]
    async fn captain_hook_register(
        &self,
        params: Parameters<RegisterParams>,
    ) -> std::result::Result<CallToolResult, McpError> {
        let p = params.0;
        let team_id = std::env::var("CLAUDE_TEAM_ID").ok();
        let session_mgr = SessionManager::new(team_id.as_deref());

        // Validate role
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let roles = crate::config::RolesConfig::load_project(&cwd).map_err(|e| {
            McpError::internal_error(format!("Failed to load roles config: {}", e), None)
        })?;

        if roles.get_role(&p.role).is_none() {
            let available: Vec<_> = roles.roles.keys().map(|k| k.as_str()).collect();
            return Ok(CallToolResult::success(vec![Content::text(format!(
                "Unknown role '{}'. Available roles: {}",
                p.role,
                available.join(", ")
            ))]));
        }

        session_mgr
            .register(&p.session_id, &p.role, p.task.as_deref(), None)
            .map_err(|e| McpError::internal_error(format!("Registration failed: {}", e), None))?;

        let role_def = roles.get_role(&p.role).unwrap();
        Ok(CallToolResult::success(vec![Content::text(format!(
            "Session {} registered as '{}'. {}",
            p.session_id, p.role, role_def.description
        ))]))
    }

    #[tool(
        description = "Disable captain-hook permission gating for a session. All tool calls will be permitted."
    )]
    async fn captain_hook_disable(
        &self,
        params: Parameters<SessionIdParams>,
    ) -> std::result::Result<CallToolResult, McpError> {
        let p = params.0;
        let team_id = std::env::var("CLAUDE_TEAM_ID").ok();
        let session_mgr = SessionManager::new(team_id.as_deref());

        session_mgr
            .disable(&p.session_id)
            .map_err(|e| McpError::internal_error(format!("Disable failed: {}", e), None))?;

        Ok(CallToolResult::success(vec![Content::text(format!(
            "Session {} disabled. All tool calls are now permitted.",
            p.session_id
        ))]))
    }

    #[tool(
        description = "Re-enable captain-hook permission gating for a previously disabled session."
    )]
    async fn captain_hook_enable(
        &self,
        params: Parameters<SessionIdParams>,
    ) -> std::result::Result<CallToolResult, McpError> {
        let p = params.0;
        let team_id = std::env::var("CLAUDE_TEAM_ID").ok();
        let session_mgr = SessionManager::new(team_id.as_deref());

        session_mgr
            .enable(&p.session_id)
            .map_err(|e| McpError::internal_error(format!("Enable failed: {}", e), None))?;

        Ok(CallToolResult::success(vec![Content::text(format!(
            "Session {} re-enabled.",
            p.session_id
        ))]))
    }

    #[tool(
        description = "Show captain-hook statistics: cached decisions, hit rates, and decision distribution by tier/role/tool."
    )]
    async fn captain_hook_status(&self) -> std::result::Result<CallToolResult, McpError> {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let project_root = cwd.join(".captain-hook");
        let global_root = crate::config::dirs_global();

        let storage = JsonlStorage::new(project_root, global_root, None);
        let decisions = storage.load_decisions(ScopeLevel::Project).map_err(|e| {
            McpError::internal_error(format!("Failed to load decisions: {}", e), None)
        })?;

        let cache = ExactCache::new();
        cache.load_from(decisions.clone());
        let stats = cache.stats();

        let mut output = String::new();
        output.push_str(&format!(
            "Total cached decisions: {}\n",
            stats.total_entries
        ));
        output.push_str(&format!("  Allow: {}\n", stats.allow_entries));
        output.push_str(&format!("  Deny:  {}\n", stats.deny_entries));
        output.push_str(&format!("  Ask:   {}\n", stats.ask_entries));

        // Count by role
        let mut role_counts = std::collections::HashMap::new();
        for record in &decisions {
            *role_counts.entry(record.key.role.clone()).or_insert(0) += 1;
        }
        if !role_counts.is_empty() {
            output.push_str("\nBy role:\n");
            for (role, count) in &role_counts {
                output.push_str(&format!("  {}: {}\n", role, count));
            }
        }

        // Pending decisions
        let queue_state = load_queue_file();
        output.push_str(&format!(
            "\nPending decisions: {}\n",
            queue_state.pending.len()
        ));

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    #[tool(description = "List pending permission decisions waiting for human approval.")]
    async fn captain_hook_queue(&self) -> std::result::Result<CallToolResult, McpError> {
        let state = load_queue_file();
        let pending: Vec<_> = state.pending.values().cloned().collect();

        if pending.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No pending decisions.",
            )]));
        }

        let mut output = String::new();
        for decision in &pending {
            output.push_str(&format!(
                "ID: {}\n  Role: {}\n  Tool: {}\n  Input: {}\n  File: {}\n  Queued: {}\n\n",
                decision.id,
                decision.role,
                decision.tool_name,
                truncate(&decision.sanitized_input, 80),
                decision.file_path.as_deref().unwrap_or("-"),
                decision.queued_at,
            ));
        }
        output.push_str(&format!("{} pending decision(s)", pending.len()));

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    #[tool(
        description = "Approve a pending permission decision. The tool call will be allowed to proceed."
    )]
    async fn captain_hook_approve(
        &self,
        params: Parameters<ApproveParams>,
    ) -> std::result::Result<CallToolResult, McpError> {
        let p = params.0;
        let queue = Arc::new(DecisionQueue::new());

        let rule_scope = if p.add_rule {
            Some(p.scope.parse::<ScopeLevel>().map_err(|e| {
                McpError::invalid_params(format!("Invalid scope '{}': {}", p.scope, e), None)
            })?)
        } else {
            None
        };

        let response = HumanResponse {
            decision: Decision::Allow,
            always_ask: p.always_ask,
            add_rule: p.add_rule,
            rule_scope,
        };

        queue
            .respond(&p.id, response)
            .map_err(|e| McpError::internal_error(format!("Approve failed: {}", e), None))?;

        let mut msg = format!("Approved decision {}", p.id);
        if p.always_ask {
            msg.push_str(" (cached as 'ask' -- will always prompt)");
        }
        if p.add_rule {
            msg.push_str(&format!(
                " (added as persistent rule at scope '{}')",
                p.scope
            ));
        }

        Ok(CallToolResult::success(vec![Content::text(msg)]))
    }

    #[tool(description = "Deny a pending permission decision. The tool call will be blocked.")]
    async fn captain_hook_deny(
        &self,
        params: Parameters<DenyParams>,
    ) -> std::result::Result<CallToolResult, McpError> {
        let p = params.0;
        let queue = Arc::new(DecisionQueue::new());

        let rule_scope = if p.add_rule {
            Some(p.scope.parse::<ScopeLevel>().map_err(|e| {
                McpError::invalid_params(format!("Invalid scope '{}': {}", p.scope, e), None)
            })?)
        } else {
            None
        };

        let response = HumanResponse {
            decision: Decision::Deny,
            always_ask: p.always_ask,
            add_rule: p.add_rule,
            rule_scope,
        };

        queue
            .respond(&p.id, response)
            .map_err(|e| McpError::internal_error(format!("Deny failed: {}", e), None))?;

        let mut msg = format!("Denied decision {}", p.id);
        if p.always_ask {
            msg.push_str(" (cached as 'ask' -- will always prompt)");
        }
        if p.add_rule {
            msg.push_str(&format!(
                " (added as persistent rule at scope '{}')",
                p.scope
            ));
        }

        Ok(CallToolResult::success(vec![Content::text(msg)]))
    }
}

impl rmcp::handler::server::ServerHandler for CaptainHookMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "captain-hook: intelligent permission gating for AI coding assistants".into(),
            ),
            ..Default::default()
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max).collect();
        format!("{}...", truncated)
    }
}

/// Run the MCP server over stdio.
pub async fn run() -> Result<()> {
    let server = CaptainHookMcp::new();
    let transport = rmcp::transport::io::stdio();

    let service = server.serve(transport).await.map_err(|e| {
        crate::error::CaptainHookError::Io(std::io::Error::other(format!(
            "MCP server initialization failed: {}",
            e
        )))
    })?;

    // Wait for the service to complete (client disconnect or shutdown)
    service.waiting().await.map_err(|e| {
        crate::error::CaptainHookError::Io(std::io::Error::other(format!(
            "MCP server error: {}",
            e
        )))
    })?;

    Ok(())
}
