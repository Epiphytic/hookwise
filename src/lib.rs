pub mod cascade;
pub mod cli;
pub mod config;
pub mod decision;
pub mod error;
pub mod hook_io;
pub mod ipc;
pub mod sanitize;
pub mod scope;
pub mod session;
pub mod storage;

use clap::Subcommand;

pub use cascade::CascadeRunner;
pub use config::{CompiledPathPolicy, PolicyConfig, RoleDefinition};
pub use decision::{CacheKey, Decision, DecisionMetadata, DecisionRecord, DecisionTier};
pub use error::{CaptainHookError, Result};
pub use hook_io::{HookFormat, HookInput, HookOutput};
pub use session::{SessionContext, SessionManager};

#[derive(Subcommand)]
pub enum Commands {
    /// Evaluate a tool call (hook mode). Reads JSON from stdin, writes JSON to stdout.
    Check {
        /// Output format: claude (default) or gemini
        #[arg(long, default_value = "claude")]
        format: HookFormat,
    },

    /// Check if session is registered (user_prompt_submit / BeforeAgent hook).
    SessionCheck {
        /// Output format: claude (default) or gemini
        #[arg(long, default_value = "claude")]
        format: HookFormat,
    },

    /// Register a session with a role.
    Register {
        #[arg(long)]
        session_id: String,
        #[arg(long)]
        role: String,
        #[arg(long)]
        task: Option<String>,
        #[arg(long)]
        prompt_file: Option<String>,
    },

    /// Disable captain-hook for a session.
    Disable {
        #[arg(long)]
        session_id: String,
    },

    /// Re-enable captain-hook for a disabled session.
    Enable {
        #[arg(long)]
        session_id: String,
    },

    /// List pending permission decisions.
    Queue,

    /// Approve a pending decision.
    Approve {
        id: String,
        #[arg(long)]
        always_ask: bool,
        #[arg(long)]
        add_rule: bool,
        #[arg(long, default_value = "project")]
        scope: String,
    },

    /// Deny a pending decision.
    Deny {
        id: String,
        #[arg(long)]
        always_ask: bool,
        #[arg(long)]
        add_rule: bool,
        #[arg(long, default_value = "project")]
        scope: String,
    },

    /// Rebuild vector indexes from rules.
    Build,

    /// Clear cached decisions.
    Invalidate {
        #[arg(long)]
        role: Option<String>,
        #[arg(long)]
        scope: Option<String>,
        #[arg(long)]
        all: bool,
    },

    /// Set an explicit permission override.
    Override {
        #[arg(long)]
        role: String,
        #[arg(long)]
        command: Option<String>,
        #[arg(long)]
        tool: Option<String>,
        #[arg(long)]
        file: Option<String>,
        #[arg(long, group = "decision")]
        allow: bool,
        #[arg(long, group = "decision")]
        deny: bool,
        #[arg(long, group = "decision")]
        ask: bool,
        #[arg(long, default_value = "project")]
        scope: String,
    },

    /// Stream decisions in real time.
    Monitor,

    /// Show cache hit rates and decision distribution.
    Stats,

    /// Pre-commit secret scan on staged files.
    Scan {
        #[arg(long)]
        staged: bool,
        path: Option<String>,
    },

    /// Initialize .captain-hook/ in the current repo.
    Init,

    /// View/edit global configuration.
    Config,

    /// Pull latest org-level rules.
    Sync,

    /// Start MCP server over stdio (for Gemini CLI extension).
    McpServer,

    /// Check for and install binary updates from GitHub releases.
    SelfUpdate {
        /// Only check for updates, don't install.
        #[arg(long)]
        check: bool,
    },
}
