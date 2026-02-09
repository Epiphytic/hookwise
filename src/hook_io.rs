use serde::{Deserialize, Serialize};

use crate::decision::Decision;
use crate::error::Result;

/// Hook format selector for multi-ecosystem support.
#[derive(Debug, Clone, Copy, Default, clap::ValueEnum)]
pub enum HookFormat {
    #[default]
    Claude,
    Gemini,
}

/// The JSON payload sent to hooks on stdin.
/// Works for both Claude Code (PreToolUse) and Gemini CLI (BeforeTool).
/// Extra Gemini fields are ignored via `#[serde(default)]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookInput {
    pub session_id: String,
    pub tool_name: String,
    pub tool_input: serde_json::Value,
    pub cwd: String,
    #[serde(default)]
    pub permission_mode: Option<String>,
    // Gemini-specific fields (ignored by Claude path)
    #[serde(default)]
    pub hook_event_name: Option<String>,
    #[serde(default)]
    pub timestamp: Option<String>,
    #[serde(default)]
    pub transcript_path: Option<String>,
    #[serde(default)]
    pub mcp_context: Option<serde_json::Value>,
}

/// Claude Code hook output: nested `hookSpecificOutput.permissionDecision`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookOutput {
    #[serde(rename = "hookSpecificOutput")]
    pub hook_specific_output: HookSpecificOutput,
}

/// The permission decision output within Claude's HookOutput.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookSpecificOutput {
    #[serde(rename = "permissionDecision")]
    pub permission_decision: String,
}

/// Gemini CLI hook output: flat `decision` field.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeminiHookOutput {
    pub decision: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl HookOutput {
    /// Create a new Claude HookOutput with the given decision.
    pub fn new(decision: Decision) -> Self {
        Self {
            hook_specific_output: HookSpecificOutput {
                permission_decision: decision_str(decision),
            },
        }
    }
}

impl GeminiHookOutput {
    /// Create a new Gemini hook output with the given decision.
    pub fn new(decision: Decision, reason: Option<String>) -> Self {
        Self {
            decision: decision_str(decision),
            reason,
        }
    }
}

fn decision_str(decision: Decision) -> String {
    match decision {
        Decision::Allow => "allow".to_string(),
        Decision::Deny => "deny".to_string(),
        Decision::Ask => "ask".to_string(),
    }
}

/// Read the hook input from stdin.
pub fn read_hook_input() -> Result<HookInput> {
    let stdin = std::io::stdin();
    let input: HookInput = serde_json::from_reader(stdin.lock())?;
    Ok(input)
}

/// Write the hook output to stdout in the appropriate format.
/// Explicitly flushes stdout to ensure data is written before any
/// subsequent `std::process::exit()` call (which does not flush Rust buffers).
pub fn write_hook_output(decision: Decision, format: HookFormat) -> Result<()> {
    use std::io::Write;
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    match format {
        HookFormat::Claude => {
            let output = HookOutput::new(decision);
            serde_json::to_writer(&mut handle, &output)?;
        }
        HookFormat::Gemini => {
            let output = GeminiHookOutput::new(decision, None);
            serde_json::to_writer(&mut handle, &output)?;
        }
    }
    handle.flush()?;
    Ok(())
}

/// Get the appropriate exit code for a deny decision.
/// Claude uses exit code 1, Gemini uses exit code 2 (emergency block).
pub fn deny_exit_code(format: HookFormat) -> i32 {
    match format {
        HookFormat::Claude => 1,
        HookFormat::Gemini => 2,
    }
}
