pub mod build;
pub mod check;
pub mod init;
pub mod mcp_server;
pub mod monitor;
pub mod override_cmd;
pub mod queue;
pub mod register;
pub mod scan;
pub mod self_update;
pub mod session_check;

use std::path::PathBuf;

use crate::config::{GlobalConfig, PolicyConfig};
use crate::error::Result;

/// Dispatch a CLI command.
pub async fn dispatch(command: crate::Commands) -> Result<()> {
    match command {
        crate::Commands::Check { format } => check::run(format).await,
        crate::Commands::SessionCheck { format } => session_check::run(format).await,
        crate::Commands::Register {
            session_id,
            role,
            task,
            prompt_file,
        } => {
            register::run_register(&session_id, &role, task.as_deref(), prompt_file.as_deref())
                .await
        }
        crate::Commands::Disable { session_id } => register::run_disable(&session_id).await,
        crate::Commands::Enable { session_id } => register::run_enable(&session_id).await,
        crate::Commands::Queue => queue::run_queue().await,
        crate::Commands::Approve {
            id,
            always_ask,
            add_rule,
            scope,
        } => queue::run_approve(&id, always_ask, add_rule, &scope).await,
        crate::Commands::Deny {
            id,
            always_ask,
            add_rule,
            scope,
        } => queue::run_deny(&id, always_ask, add_rule, &scope).await,
        crate::Commands::Build => build::run_build().await,
        crate::Commands::Invalidate { role, scope, all } => {
            build::run_invalidate(role.as_deref(), scope.as_deref(), all).await
        }
        crate::Commands::Override {
            role,
            command,
            tool,
            file,
            allow,
            deny,
            ask,
            scope,
        } => {
            override_cmd::run(
                &role,
                command.as_deref(),
                tool.as_deref(),
                file.as_deref(),
                allow,
                deny,
                ask,
                &scope,
            )
            .await
        }
        crate::Commands::Monitor => monitor::run_monitor().await,
        crate::Commands::Stats => monitor::run_stats().await,
        crate::Commands::Scan { staged, path } => scan::run(staged, path.as_deref()).await,
        crate::Commands::Init => init::run().await,
        crate::Commands::Config => run_config().await,
        crate::Commands::Sync => run_sync().await,
        crate::Commands::McpServer => mcp_server::run().await,
        crate::Commands::SelfUpdate { check } => self_update::run(check).await,
    }
}

/// Display global and project configuration.
async fn run_config() -> Result<()> {
    // Show global config
    let global_dir = dirs_global();
    let global_config_path = global_dir.join("config.yml");

    println!("Global config: {}", global_config_path.display());
    match GlobalConfig::load()? {
        Some(config) => {
            println!("  Supervisor: {:?}", config.supervisor);
            if config.api_key.is_some() {
                println!("  API key: (set)");
            }
            if let Some(model) = &config.embedding_model {
                println!("  Embedding model: {}", model);
            }
        }
        None => {
            println!("  (not configured)");
        }
    }

    // Show project config
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let project_config_path = cwd.join(".captain-hook").join("policy.yml");

    println!("\nProject config: {}", project_config_path.display());
    if project_config_path.exists() {
        let policy = PolicyConfig::load_project(&cwd)?;
        println!(
            "  Sensitive paths (ask_write): {:?}",
            policy.sensitive_paths.ask_write
        );
        println!(
            "  Confidence thresholds: org={}, project={}, user={}",
            policy.confidence.org, policy.confidence.project, policy.confidence.user
        );
        println!(
            "  Similarity: jaccard={}, embedding={}, min_tokens={}",
            policy.similarity.jaccard_threshold,
            policy.similarity.embedding_threshold,
            policy.similarity.jaccard_min_tokens
        );
        println!("  Human timeout: {}s", policy.human_timeout_secs);
        println!(
            "  Registration timeout: {}s",
            policy.registration_timeout_secs
        );
    } else {
        println!("  (not initialized -- run `captain-hook init`)");
    }

    Ok(())
}

/// Pull latest org-level rules (placeholder).
async fn run_sync() -> Result<()> {
    eprintln!("captain-hook: sync is not yet implemented.");
    eprintln!("Org-level rule syncing will be available in a future release.");
    Ok(())
}

fn dirs_global() -> PathBuf {
    crate::config::dirs_global()
}
