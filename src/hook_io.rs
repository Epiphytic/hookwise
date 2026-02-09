use serde::{Deserialize, Serialize};

use crate::error::Result;

/// The JSON payload Claude Code sends to hooks on stdin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookInput {
    pub session_id: String,
    pub tool_name: String,
    pub tool_input: serde_json::Value,
    pub cwd: String,
    #[serde(default)]
    pub permission_mode: Option<String>,
}

/// The JSON payload captain-hook outputs to stdout.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookOutput {
    #[serde(rename = "hookSpecificOutput")]
    pub hook_specific_output: HookSpecificOutput,
}

/// The permission decision output within HookOutput.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookSpecificOutput {
    #[serde(rename = "permissionDecision")]
    pub permission_decision: String,
}

impl HookOutput {
    /// Create a new HookOutput with the given decision.
    pub fn new(decision: crate::decision::Decision) -> Self {
        Self {
            hook_specific_output: HookSpecificOutput {
                permission_decision: match decision {
                    crate::decision::Decision::Allow => "allow".to_string(),
                    crate::decision::Decision::Deny => "deny".to_string(),
                    crate::decision::Decision::Ask => "ask".to_string(),
                },
            },
        }
    }
}

/// Read the hook input from stdin.
pub fn read_hook_input() -> Result<HookInput> {
    let stdin = std::io::stdin();
    let input: HookInput = serde_json::from_reader(stdin.lock())?;
    Ok(input)
}

/// Write the hook output to stdout.
/// Explicitly flushes stdout to ensure data is written before any
/// subsequent `std::process::exit()` call (which does not flush Rust buffers).
pub fn write_hook_output(output: &HookOutput) -> Result<()> {
    use std::io::Write;
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    serde_json::to_writer(&mut handle, output)?;
    handle.flush()?;
    Ok(())
}
