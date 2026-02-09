use crate::error::Result;
use crate::hook_io::HookFormat;
use crate::session::SessionManager;

/// Run the `session-check` subcommand.
/// Used by the `user_prompt_submit` hook (Claude) or `BeforeAgent` hook (Gemini)
/// to check if a session is registered.
/// If not registered, outputs a prompt asking the user to pick a role.
pub async fn run(format: HookFormat) -> Result<()> {
    // Read hook input from stdin to get session_id
    let input = crate::hook_io::read_hook_input()?;
    let team_id = std::env::var("CLAUDE_TEAM_ID").ok();
    let session_mgr = SessionManager::new(team_id.as_deref());

    if session_mgr.is_disabled(&input.session_id) {
        // Session is disabled, nothing to do
        return Ok(());
    }

    if session_mgr.is_registered(&input.session_id) {
        // Already registered, nothing to do
        return Ok(());
    }

    // Not registered -- output a registration prompt
    let cwd = std::path::PathBuf::from(&input.cwd);
    let roles = crate::config::RolesConfig::load_project(&cwd)?;
    let role_names: Vec<&String> = roles.roles.keys().collect();

    let _format = format; // format available for future use (e.g. Gemini-specific messaging)

    eprintln!(
        "captain-hook: session {} is not registered.",
        input.session_id
    );
    eprintln!(
        "Available roles: {}",
        role_names
            .iter()
            .map(|r| r.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    eprintln!(
        "Register with: captain-hook register --session-id {} --role <ROLE>",
        input.session_id
    );
    eprintln!(
        "Or disable: captain-hook disable --session-id {}",
        input.session_id
    );

    Ok(())
}
