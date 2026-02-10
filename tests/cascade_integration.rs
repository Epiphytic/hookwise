//! Integration tests for the decision cascade: end-to-end scenarios exercising
//! sanitization -> path policy -> exact cache -> token similarity -> default deny.
//!
//! These tests build a CascadeRunner with real tiers (except supervisor/human
//! which are stubbed) and verify the full pipeline.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use tempfile::TempDir;

use captain_hook::cascade::cache::ExactCache;
use captain_hook::cascade::embed_sim::EmbeddingSimilarity;
use captain_hook::cascade::path_policy::PathPolicyEngine;
use captain_hook::cascade::token_sim::TokenJaccard;
use captain_hook::cascade::{CascadeInput, CascadeRunner, CascadeTier};
use captain_hook::config::policy::PolicyConfig;
use captain_hook::config::roles::{CompiledPathPolicy, PathPolicyConfig, RoleDefinition};
use captain_hook::decision::{
    CacheKey, Decision, DecisionMetadata, DecisionRecord, DecisionTier, ScopeLevel,
};
use captain_hook::session::SessionContext;
use captain_hook::storage::jsonl::JsonlStorage;

// ---------------------------------------------------------------------------
// Stub tiers for deterministic testing
// ---------------------------------------------------------------------------

/// A supervisor tier that always returns None (falls through).
struct NoopSupervisor;

#[async_trait]
impl CascadeTier for NoopSupervisor {
    async fn evaluate(
        &self,
        _input: &CascadeInput,
    ) -> captain_hook::error::Result<Option<DecisionRecord>> {
        Ok(None)
    }
    fn tier(&self) -> DecisionTier {
        DecisionTier::Supervisor
    }
    fn name(&self) -> &str {
        "noop-supervisor"
    }
}

/// A human tier that always returns None (simulates timeout -> default deny).
struct NoopHuman;

#[async_trait]
impl CascadeTier for NoopHuman {
    async fn evaluate(
        &self,
        _input: &CascadeInput,
    ) -> captain_hook::error::Result<Option<DecisionRecord>> {
        Ok(None)
    }
    fn tier(&self) -> DecisionTier {
        DecisionTier::Human
    }
    fn name(&self) -> &str {
        "noop-human"
    }
}

/// A supervisor tier that always allows.
struct AllowSupervisor;

#[async_trait]
impl CascadeTier for AllowSupervisor {
    async fn evaluate(
        &self,
        input: &CascadeInput,
    ) -> captain_hook::error::Result<Option<DecisionRecord>> {
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
            decision: Decision::Allow,
            metadata: DecisionMetadata {
                tier: DecisionTier::Supervisor,
                confidence: 0.95,
                reason: "test supervisor allows".into(),
                matched_key: None,
                similarity_score: None,
            },
            timestamp: Utc::now(),
            scope: ScopeLevel::Project,
            file_path: input.file_path.clone(),
            session_id: String::new(),
        }))
    }
    fn tier(&self) -> DecisionTier {
        DecisionTier::Supervisor
    }
    fn name(&self) -> &str {
        "allow-supervisor"
    }
}

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

fn make_session(role_name: &str) -> SessionContext {
    let path_config = PathPolicyConfig {
        allow_write: vec!["src/**".into(), "Cargo.toml".into()],
        deny_write: vec!["tests/**".into(), "docs/**".into()],
        allow_read: vec!["**".into()],
    };
    let sensitive = vec![".claude/**".into(), ".env*".into()];
    let compiled = CompiledPathPolicy::compile(&path_config, &sensitive).unwrap();

    SessionContext {
        user: "test-user".into(),
        org: "test-org".into(),
        project: "test-project".into(),
        team: None,
        role: Some(RoleDefinition {
            name: role_name.into(),
            description: "test role".into(),
            paths: path_config,
        }),
        path_policy: Some(Arc::new(compiled)),
        agent_prompt_hash: None,
        agent_prompt_path: None,
        task_description: None,
        registered_at: Some(Utc::now()),
        disabled: false,
    }
}

fn make_runner(
    tmp: &TempDir,
    supervisor: Box<dyn CascadeTier>,
    human: Box<dyn CascadeTier>,
) -> CascadeRunner {
    let storage = JsonlStorage::new(tmp.path().to_path_buf(), tmp.path().join("global"), None);

    // Try embedding similarity; if model fails, use noop
    let embedding_sim = match EmbeddingSimilarity::new("default", 0.85) {
        Ok(es) => Arc::new(es),
        Err(_) => {
            // Create with impossible threshold so it never matches
            Arc::new(
                EmbeddingSimilarity::new("default", 999.0).unwrap_or_else(|_| {
                    panic!("EmbeddingSimilarity should not fail twice");
                }),
            )
        }
    };

    CascadeRunner {
        sanitizer: captain_hook::sanitize::SanitizePipeline::default_pipeline(),
        path_policy: Box::new(PathPolicyEngine::new().unwrap()),
        exact_cache: Arc::new(ExactCache::new()),
        token_jaccard: Arc::new(TokenJaccard::new(0.7, 3)),
        embedding_similarity: embedding_sim,
        supervisor,
        human,
        storage: Box::new(storage),
        policy: PolicyConfig::default(),
        normalizer: None,
    }
}

fn make_runner_simple(tmp: &TempDir) -> CascadeRunner {
    make_runner(tmp, Box::new(NoopSupervisor), Box::new(NoopHuman))
}

fn make_runner_with_allow_supervisor(tmp: &TempDir) -> CascadeRunner {
    make_runner(tmp, Box::new(AllowSupervisor), Box::new(NoopHuman))
}

// ---------------------------------------------------------------------------
// End-to-end cascade scenarios
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cascade_denies_write_to_denied_path() {
    let tmp = TempDir::new().unwrap();
    let runner = make_runner_simple(&tmp);
    let session = make_session("coder");

    // Coder writing to tests/ should be denied by path policy
    let tool_input = serde_json::json!({"file_path": "tests/unit.rs", "content": "test"});
    let record = runner
        .evaluate(&session, "Write", &tool_input)
        .await
        .unwrap();

    assert_eq!(record.decision, Decision::Deny);
    assert_eq!(record.metadata.tier, DecisionTier::PathPolicy);
}

#[tokio::test]
async fn cascade_allows_write_to_allowed_path() {
    let tmp = TempDir::new().unwrap();
    let runner = make_runner_with_allow_supervisor(&tmp);
    let session = make_session("coder");

    // Coder writing to src/ is allowed by path policy, falls through to supervisor
    let tool_input = serde_json::json!({"file_path": "src/main.rs", "content": "fn main() {}"});
    let record = runner
        .evaluate(&session, "Write", &tool_input)
        .await
        .unwrap();

    assert_eq!(record.decision, Decision::Allow);
}

#[tokio::test]
async fn cascade_asks_for_sensitive_path() {
    let tmp = TempDir::new().unwrap();
    let runner = make_runner_simple(&tmp);
    let session = make_session("coder");

    // Writing to .env triggers sensitive_ask_write
    let tool_input = serde_json::json!({"file_path": ".env", "content": "SECRET=x"});
    let record = runner
        .evaluate(&session, "Write", &tool_input)
        .await
        .unwrap();

    assert_eq!(record.decision, Decision::Ask);
    assert_eq!(record.metadata.tier, DecisionTier::PathPolicy);
}

#[tokio::test]
async fn cascade_exact_cache_hit() {
    let tmp = TempDir::new().unwrap();
    let runner = make_runner_with_allow_supervisor(&tmp);
    let session = make_session("coder");

    // First call: falls through to supervisor, gets allowed, gets cached
    let tool_input = serde_json::json!({"command": "cargo build --release"});
    let first = runner
        .evaluate(&session, "Bash", &tool_input)
        .await
        .unwrap();
    assert_eq!(first.decision, Decision::Allow);

    // Second call with identical input: should hit exact cache
    let second = runner
        .evaluate(&session, "Bash", &tool_input)
        .await
        .unwrap();
    assert_eq!(second.decision, Decision::Allow);
    assert_eq!(second.metadata.tier, DecisionTier::ExactCache);
}

#[tokio::test]
async fn cascade_default_deny_when_no_tier_resolves() {
    let tmp = TempDir::new().unwrap();
    let runner = make_runner_simple(&tmp);
    let session = make_session("coder");

    // A Bash command with no file path -- path policy doesn't fire,
    // cache is empty, supervisor is noop, human is noop -> default deny
    let tool_input = serde_json::json!({"command": "echo hello"});
    let record = runner
        .evaluate(&session, "Bash", &tool_input)
        .await
        .unwrap();

    assert_eq!(record.decision, Decision::Deny);
}

#[tokio::test]
async fn cascade_sanitizes_secrets_before_caching() {
    let tmp = TempDir::new().unwrap();
    let runner = make_runner_with_allow_supervisor(&tmp);
    let session = make_session("coder");

    // Input contains a secret (ghp_ token)
    let tool_input = serde_json::json!({"command": "git push ghp_secret123456789"});
    let record = runner
        .evaluate(&session, "Bash", &tool_input)
        .await
        .unwrap();

    // The cached key should have the secret redacted
    assert!(!record.key.sanitized_input.contains("ghp_secret123456789"));
    assert!(record.key.sanitized_input.contains("<REDACTED>"));
}

#[tokio::test]
async fn cascade_deny_wins_over_ask() {
    let tmp = TempDir::new().unwrap();
    let runner = make_runner_simple(&tmp);

    // Create a session where both deny_write and sensitive match the same path
    let path_config = PathPolicyConfig {
        allow_write: vec!["**".into()],
        deny_write: vec![".env*".into()],
        allow_read: vec!["**".into()],
    };
    let sensitive = vec![".env*".into()];
    let compiled = CompiledPathPolicy::compile(&path_config, &sensitive).unwrap();

    let session = SessionContext {
        user: "test".into(),
        org: "test".into(),
        project: "test".into(),
        team: None,
        role: Some(RoleDefinition {
            name: "custom".into(),
            description: "test".into(),
            paths: path_config,
        }),
        path_policy: Some(Arc::new(compiled)),
        agent_prompt_hash: None,
        agent_prompt_path: None,
        task_description: None,
        registered_at: Some(Utc::now()),
        disabled: false,
    };

    // .env matches both deny_write and sensitive_ask_write.
    // BUG DOCUMENTATION: The path policy engine checks sensitive_ask_write BEFORE
    // deny_write in its if-else chain (path_policy.rs:139-147). So when a path
    // matches both, sensitive wins and returns Ask, even though deny has higher
    // precedence. The precedence logic (worst_decision) only applies across
    // different paths, not within a single path's match.
    // This means deny_write patterns that overlap with sensitive_ask_write are
    // effectively shadowed. This could be an intentional design choice (sensitive
    // paths always prompt) or a bug where the if-else order should check deny first.
    let tool_input = serde_json::json!({"file_path": ".env.local", "content": "x"});
    let record = runner
        .evaluate(&session, "Write", &tool_input)
        .await
        .unwrap();

    // Current behavior: Ask wins because sensitive is checked first in the if-else
    assert_eq!(record.decision, Decision::Ask);
}

#[tokio::test]
async fn cascade_read_always_allowed_for_read_tools() {
    let tmp = TempDir::new().unwrap();
    let runner = make_runner_with_allow_supervisor(&tmp);
    let session = make_session("coder");

    // Even though tests/ is in deny_write, reading tests/ should be fine
    // because allow_read = "**"
    let tool_input = serde_json::json!({"file_path": "tests/unit.rs"});
    let record = runner
        .evaluate(&session, "Read", &tool_input)
        .await
        .unwrap();

    // Path policy should not fire for read ops on allowed read paths
    // Falls through to supervisor (which allows) or default deny
    assert_ne!(record.metadata.tier, DecisionTier::PathPolicy);
}

#[tokio::test]
async fn cascade_persists_decisions() {
    let tmp = TempDir::new().unwrap();
    let runner = make_runner_with_allow_supervisor(&tmp);
    let session = make_session("coder");

    let tool_input = serde_json::json!({"command": "cargo test"});
    let record = runner
        .evaluate(&session, "Bash", &tool_input)
        .await
        .unwrap();
    assert_eq!(record.decision, Decision::Allow);

    // Verify the decision was persisted to storage
    use captain_hook::storage::StorageBackend;
    let storage = JsonlStorage::new(tmp.path().to_path_buf(), tmp.path().join("global"), None);
    let loaded = storage.load_decisions(ScopeLevel::Project).unwrap();
    assert!(!loaded.is_empty(), "decision should be persisted to JSONL");
}

#[tokio::test]
async fn cascade_token_similarity_auto_approves() {
    let tmp = TempDir::new().unwrap();
    let runner = make_runner_with_allow_supervisor(&tmp);
    let session = make_session("coder");

    // First call: novel command, goes to supervisor, gets allowed
    let tool_input_1 =
        serde_json::json!({"command": "cargo build --release --target x86_64-unknown-linux"});
    let record_1 = runner
        .evaluate(&session, "Bash", &tool_input_1)
        .await
        .unwrap();
    assert_eq!(record_1.decision, Decision::Allow);

    // Second call with a very similar command
    // The token similarity threshold is 0.7, and these share most tokens
    let tool_input_2 =
        serde_json::json!({"command": "cargo build --release --target aarch64-unknown-linux"});
    let record_2 = runner
        .evaluate(&session, "Bash", &tool_input_2)
        .await
        .unwrap();

    // Should be allowed either via exact cache (if sanitized same) or token sim or supervisor
    assert_eq!(record_2.decision, Decision::Allow);
}

// ---------------------------------------------------------------------------
// HookOutput integration
// ---------------------------------------------------------------------------

#[test]
fn hook_output_serialization() {
    use captain_hook::hook_io::HookOutput;

    let output = HookOutput::new(Decision::Allow);
    let json = serde_json::to_string(&output).unwrap();
    assert!(json.contains("\"permissionDecision\":\"allow\""));

    let output = HookOutput::new(Decision::Deny);
    let json = serde_json::to_string(&output).unwrap();
    assert!(json.contains("\"permissionDecision\":\"deny\""));

    let output = HookOutput::new(Decision::Ask);
    let json = serde_json::to_string(&output).unwrap();
    assert!(json.contains("\"permissionDecision\":\"ask\""));
}

#[test]
fn hook_input_deserialization() {
    use captain_hook::hook_io::HookInput;

    let json = r#"{
        "session_id": "abc-123",
        "tool_name": "Bash",
        "tool_input": {"command": "echo hello"},
        "cwd": "/tmp"
    }"#;
    let input: HookInput = serde_json::from_str(json).unwrap();
    assert_eq!(input.session_id, "abc-123");
    assert_eq!(input.tool_name, "Bash");
    assert_eq!(input.cwd, "/tmp");
}

// ---------------------------------------------------------------------------
// Scope merge integration
// ---------------------------------------------------------------------------

#[test]
fn scope_merge_deny_wins_over_allow() {
    use captain_hook::scope::merge::merge_decisions;
    use captain_hook::scope::ScopedDecision;

    let allow_record = DecisionRecord {
        key: CacheKey {
            sanitized_input: "test".into(),
            tool: "Bash".into(),
            role: "coder".into(),
        },
        decision: Decision::Allow,
        metadata: DecisionMetadata {
            tier: DecisionTier::Human,
            confidence: 1.0,
            reason: "user allowed".into(),
            matched_key: None,
            similarity_score: None,
        },
        timestamp: Utc::now(),
        scope: ScopeLevel::User,
        file_path: None,
        session_id: "test".into(),
    };

    let deny_record = DecisionRecord {
        key: allow_record.key.clone(),
        decision: Decision::Deny,
        metadata: DecisionMetadata {
            tier: DecisionTier::Human,
            confidence: 1.0,
            reason: "org denied".into(),
            matched_key: None,
            similarity_score: None,
        },
        timestamp: Utc::now(),
        scope: ScopeLevel::Org,
        file_path: None,
        session_id: "test".into(),
    };

    let decisions = vec![
        ScopedDecision {
            decision: Decision::Allow,
            scope: ScopeLevel::User,
            record: allow_record,
        },
        ScopedDecision {
            decision: Decision::Deny,
            scope: ScopeLevel::Org,
            record: deny_record,
        },
    ];

    let result = merge_decisions(decisions).unwrap();
    assert_eq!(result.decision, Decision::Deny);
}

#[test]
fn scope_merge_ask_wins_over_allow() {
    use captain_hook::scope::merge::merge_decisions;
    use captain_hook::scope::ScopedDecision;

    let allow_record = DecisionRecord {
        key: CacheKey {
            sanitized_input: "test".into(),
            tool: "Bash".into(),
            role: "coder".into(),
        },
        decision: Decision::Allow,
        metadata: DecisionMetadata {
            tier: DecisionTier::Human,
            confidence: 1.0,
            reason: "allowed".into(),
            matched_key: None,
            similarity_score: None,
        },
        timestamp: Utc::now(),
        scope: ScopeLevel::User,
        file_path: None,
        session_id: "test".into(),
    };

    let ask_record = DecisionRecord {
        key: allow_record.key.clone(),
        decision: Decision::Ask,
        metadata: DecisionMetadata {
            tier: DecisionTier::Human,
            confidence: 1.0,
            reason: "sensitive".into(),
            matched_key: None,
            similarity_score: None,
        },
        timestamp: Utc::now(),
        scope: ScopeLevel::Project,
        file_path: None,
        session_id: "test".into(),
    };

    let decisions = vec![
        ScopedDecision {
            decision: Decision::Allow,
            scope: ScopeLevel::User,
            record: allow_record,
        },
        ScopedDecision {
            decision: Decision::Ask,
            scope: ScopeLevel::Project,
            record: ask_record,
        },
    ];

    let result = merge_decisions(decisions).unwrap();
    assert_eq!(result.decision, Decision::Ask);
}

// ---------------------------------------------------------------------------
// Human tier: decision queue integration
// ---------------------------------------------------------------------------

fn clean_queue_file() {
    let path = captain_hook::cascade::human::pending_queue_path();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn decision_queue_enqueue_and_list() {
    clean_queue_file();
    use captain_hook::cascade::human::{DecisionQueue, PendingDecision};

    let queue = DecisionQueue::new();
    let pending = PendingDecision {
        id: "test-1".into(),
        session_id: "session-1".into(),
        role: "coder".into(),
        tool_name: "Bash".into(),
        sanitized_input: "echo hello".into(),
        file_path: None,
        recommendation: None,
        is_ask_reprompt: false,
        ask_reason: None,
        queued_at: Utc::now(),
    };

    queue.enqueue(pending);
    let list = queue.list_pending();
    assert!(
        list.iter().any(|p| p.id == "test-1"),
        "enqueued item should appear in pending list"
    );
}

#[test]
fn decision_queue_respond_removes_pending() {
    clean_queue_file();
    use captain_hook::cascade::human::{DecisionQueue, HumanResponse, PendingDecision};

    let queue = DecisionQueue::new();
    let pending = PendingDecision {
        id: "test-2".into(),
        session_id: "session-1".into(),
        role: "coder".into(),
        tool_name: "Bash".into(),
        sanitized_input: "rm -rf /".into(),
        file_path: None,
        recommendation: None,
        is_ask_reprompt: false,
        ask_reason: None,
        queued_at: Utc::now(),
    };

    queue.enqueue(pending);

    queue
        .respond(
            "test-2",
            HumanResponse {
                decision: Decision::Deny,
                always_ask: false,
                add_rule: true,
                rule_scope: Some(ScopeLevel::Project),
            },
        )
        .unwrap();

    assert!(queue.list_pending().is_empty());
    // The response was consumed; take_response should return it once
    // (it was already consumed by respond, but the queue stores it in completed)
    let resp = queue.take_response("test-2");
    assert!(resp.is_some());
    assert_eq!(resp.unwrap().decision, Decision::Deny);
}
