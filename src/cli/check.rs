use std::path::PathBuf;
use std::sync::Arc;

use crate::cascade::cache::ExactCache;
use crate::cascade::embed_sim::EmbeddingSimilarity;
use crate::cascade::human::{DecisionQueue, HumanTier};
use crate::cascade::path_policy::PathPolicyEngine;
use crate::cascade::supervisor::{SupervisorTier, UnixSocketSupervisor};
use crate::cascade::token_sim::TokenJaccard;
use crate::cascade::CascadeRunner;
use crate::config::{PolicyConfig, SupervisorConfig};
use crate::decision::Decision;
use crate::error::Result;
use crate::hook_io;
use crate::sanitize::SanitizePipeline;
use crate::session::SessionManager;
use crate::storage::jsonl::JsonlStorage;
use crate::storage::StorageBackend;

/// Run the `check` subcommand (hook mode).
/// Reads JSON from stdin, runs the cascade, writes JSON to stdout.
pub async fn run() -> Result<()> {
    // 1. Read hook input from stdin
    let input = hook_io::read_hook_input()?;

    let cwd = &input.cwd;
    let cwd_path = PathBuf::from(cwd);

    // 2. Load config
    let policy = PolicyConfig::load_project(&cwd_path)?;
    let team_id = std::env::var("CLAUDE_TEAM_ID").ok();

    // 3. Get session context
    let session_mgr = SessionManager::new(team_id.as_deref());

    // Check if session is disabled
    if session_mgr.is_disabled(&input.session_id) {
        // Disabled sessions always allow
        let output = hook_io::HookOutput::new(Decision::Allow);
        hook_io::write_hook_output(&output)?;
        return Ok(());
    }

    // Wait for registration if needed (5s timeout)
    if !session_mgr.is_registered(&input.session_id) {
        session_mgr
            .wait_for_registration(&input.session_id, policy.registration_timeout_secs)
            .await?;
    }

    let session = session_mgr.get_or_populate(&input.session_id, cwd)?;

    // If session has no role, deny (unregistered)
    if session.role.is_none() && !session.disabled {
        let output = hook_io::HookOutput::new(Decision::Deny);
        hook_io::write_hook_output(&output)?;
        return Ok(());
    }

    // 4. Build cascade runner
    let project_root = cwd_path.join(".captain-hook");
    let global_root = dirs_global();

    let storage = JsonlStorage::new(
        project_root.clone(),
        global_root.clone(),
        Some(session.org.clone()),
    );

    // Load existing decisions for caches
    let all_decisions = storage.load_decisions(crate::scope::ScopeLevel::Project)?;

    // Build tiers
    let path_policy = PathPolicyEngine::new()?;
    let exact_cache = Arc::new(ExactCache::new());
    exact_cache.load_from(all_decisions.clone());

    let token_jaccard = Arc::new(TokenJaccard::new(
        policy.similarity.jaccard_threshold,
        policy.similarity.jaccard_min_tokens,
    ));
    token_jaccard.load_from(&all_decisions);

    // Embedding similarity -- try to create, fall back to no-op if model loading fails
    let embedding_similarity =
        match EmbeddingSimilarity::new("default", policy.similarity.embedding_threshold) {
            Ok(es) => {
                let _ = es.build_index(&all_decisions);
                Arc::new(es)
            }
            Err(e) => {
                eprintln!("captain-hook: embedding tier unavailable, skipping ({})", e);
                Arc::new(EmbeddingSimilarity::new_noop())
            }
        };

    // Supervisor tier
    let supervisor: Box<dyn crate::cascade::CascadeTier> = match &policy.supervisor {
        SupervisorConfig::Socket { socket_path } => {
            let sock_path = socket_path.clone().unwrap_or_else(|| {
                let tid = team_id.as_deref().unwrap_or("solo");
                PathBuf::from(format!("/tmp/captain-hook-{tid}.sock"))
            });
            let backend = UnixSocketSupervisor::new(sock_path, 30);
            Box::new(SupervisorTier::new(Box::new(backend), policy.clone()))
        }
        SupervisorConfig::Api {
            api_base_url,
            model,
            max_tokens,
        } => {
            let api_key = std::env::var("ANTHROPIC_API_KEY").unwrap_or_default();
            let backend = crate::cascade::supervisor::ApiSupervisor::new(
                api_base_url
                    .clone()
                    .unwrap_or_else(|| "https://api.anthropic.com".into()),
                api_key,
                model
                    .clone()
                    .unwrap_or_else(|| "claude-sonnet-4-5-20250929".into()),
                max_tokens.unwrap_or(1024),
            );
            Box::new(SupervisorTier::new(Box::new(backend), policy.clone()))
        }
    };

    // Human tier
    let decision_queue = Arc::new(DecisionQueue::new());
    let human = HumanTier::new(decision_queue, policy.human_timeout_secs);

    let runner = CascadeRunner {
        sanitizer: SanitizePipeline::default_pipeline(),
        path_policy: Box::new(path_policy),
        exact_cache,
        token_jaccard,
        embedding_similarity,
        supervisor,
        human: Box::new(human),
        storage: Box::new(storage),
        policy: policy.clone(),
    };

    // 5. Run cascade
    let record = match runner
        .evaluate_with_cwd(&session, &input.tool_name, &input.tool_input, Some(cwd))
        .await
    {
        Ok(record) => record,
        Err(e) => {
            // On cascade error (e.g. human timeout), default to deny
            // but still write output so callers can parse it.
            eprintln!("captain-hook: cascade error, defaulting to deny ({})", e);
            let output = hook_io::HookOutput::new(Decision::Deny);
            hook_io::write_hook_output(&output)?;
            std::process::exit(1);
        }
    };

    // 6. Output result
    let output = hook_io::HookOutput::new(record.decision);
    hook_io::write_hook_output(&output)?;

    // Exit with appropriate code: 0 for allow, 1 for deny
    if record.decision == Decision::Deny {
        std::process::exit(1);
    }

    Ok(())
}

/// Get the global config directory.
fn dirs_global() -> PathBuf {
    crate::config::dirs_global()
}
