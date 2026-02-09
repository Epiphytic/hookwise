pub mod cache;
pub mod embed_sim;
pub mod human;
pub mod path_policy;
pub mod supervisor;
pub mod token_sim;

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;

use crate::decision::{
    CacheKey, Decision, DecisionMetadata, DecisionRecord, DecisionTier, ScopeLevel,
};
use crate::error::Result;
use crate::session::SessionContext;

/// Input to each cascade tier.
#[derive(Debug, Clone)]
pub struct CascadeInput {
    pub session: SessionContext,
    pub tool_name: String,
    pub tool_input: serde_json::Value,
    pub sanitized_input: String,
    pub file_path: Option<String>,
    /// The working directory of the tool call, used to relativize absolute paths.
    pub cwd: Option<String>,
}

/// A single tier in the decision cascade.
#[async_trait]
pub trait CascadeTier: Send + Sync {
    /// Evaluate this tier. Returns Some(record) if the tier can make a decision,
    /// None if it should fall through to the next tier.
    async fn evaluate(&self, input: &CascadeInput) -> Result<Option<DecisionRecord>>;

    /// The tier identifier.
    fn tier(&self) -> DecisionTier;

    /// Human-readable name for this tier.
    fn name(&self) -> &str;
}

/// The complete cascade runner. Evaluates tiers in order until one resolves.
pub struct CascadeRunner {
    pub sanitizer: crate::sanitize::SanitizePipeline,
    pub path_policy: Box<dyn CascadeTier>,
    pub exact_cache: Arc<cache::ExactCache>,
    pub token_jaccard: Arc<token_sim::TokenJaccard>,
    pub embedding_similarity: Arc<embed_sim::EmbeddingSimilarity>,
    pub supervisor: Box<dyn CascadeTier>,
    pub human: Box<dyn CascadeTier>,
    pub storage: Box<dyn crate::storage::StorageBackend>,
    pub policy: crate::config::PolicyConfig,
}

impl CascadeRunner {
    /// Run the full cascade for a tool call.
    pub async fn evaluate(
        &self,
        session: &SessionContext,
        tool_name: &str,
        tool_input: &serde_json::Value,
    ) -> Result<DecisionRecord> {
        self.evaluate_with_cwd(session, tool_name, tool_input, None)
            .await
    }

    /// Run the full cascade for a tool call, with an optional cwd for path relativization.
    pub async fn evaluate_with_cwd(
        &self,
        session: &SessionContext,
        tool_name: &str,
        tool_input: &serde_json::Value,
        cwd: Option<&str>,
    ) -> Result<DecisionRecord> {
        // Sanitize the tool input
        let raw_input = serde_json::to_string(tool_input).unwrap_or_default();
        let sanitized_input = self.sanitizer.sanitize(&raw_input);

        // Extract file path from tool input
        let file_path = Self::extract_file_path(tool_name, tool_input);

        let input = CascadeInput {
            session: session.clone(),
            tool_name: tool_name.to_string(),
            tool_input: tool_input.clone(),
            sanitized_input,
            file_path,
            cwd: cwd.map(String::from),
        };

        // Run tiers in order: path_policy -> exact_cache -> token_jaccard ->
        // embedding_similarity -> supervisor -> human
        let tiers: Vec<&dyn CascadeTier> = vec![
            self.path_policy.as_ref(),
            self.exact_cache.as_ref(),
            self.token_jaccard.as_ref(),
            self.embedding_similarity.as_ref(),
            self.supervisor.as_ref(),
            self.human.as_ref(),
        ];

        for tier in &tiers {
            if let Some(mut record) = tier.evaluate(&input).await? {
                // Fill in session_id on all records
                if record.session_id.is_empty() {
                    // Use a session identifier from the context
                    record.session_id = format!(
                        "{}/{}/{}",
                        input.session.org, input.session.project, input.session.user
                    );
                }

                // Persist decisions from tiers that produce new decisions
                match record.metadata.tier {
                    DecisionTier::ExactCache => {
                        // Already in exact cache -- no need to persist again
                    }
                    DecisionTier::TokenJaccard | DecisionTier::EmbeddingSimilarity => {
                        // Similarity tiers: insert into exact cache to prevent
                        // "ask drift" where repeated similar commands might match
                        // different entries on subsequent calls (HIGH-03).
                        self.exact_cache.insert(record.clone());
                    }
                    _ => {
                        // Path policy, supervisor, human -- full persist
                        self.persist_decision(&record).await?;
                    }
                }

                return Ok(record);
            }
        }

        // If no tier resolved, default to deny (timeout defaults to deny)
        let role_name = session
            .role
            .as_ref()
            .map(|r| r.name.clone())
            .unwrap_or_else(|| "*".to_string());

        let record = DecisionRecord {
            key: CacheKey {
                sanitized_input: input.sanitized_input,
                tool: tool_name.to_string(),
                role: role_name,
            },
            decision: Decision::Deny,
            metadata: DecisionMetadata {
                tier: DecisionTier::Default,
                confidence: 1.0,
                reason: "no cascade tier resolved; default deny".to_string(),
                matched_key: None,
                similarity_score: None,
            },
            timestamp: Utc::now(),
            scope: ScopeLevel::Project,
            file_path: input.file_path,
            session_id: format!("{}/{}/{}", session.org, session.project, session.user),
        };

        self.persist_decision(&record).await?;
        Ok(record)
    }

    /// Extract file path from tool input for file-related tools.
    fn extract_file_path(tool_name: &str, tool_input: &serde_json::Value) -> Option<String> {
        match tool_name {
            "Write" | "Edit" | "Read" => tool_input
                .get("file_path")
                .and_then(|v| v.as_str())
                .map(String::from),
            "Glob" | "Grep" => tool_input
                .get("path")
                .and_then(|v| v.as_str())
                .map(String::from),
            "Bash" => {
                // For Bash, return None here. The full path extraction with
                // regex patterns happens inside PathPolicyEngine::evaluate().
                // This field is mainly for audit/logging of the primary target.
                None
            }
            "NotebookEdit" => tool_input
                .get("notebook_path")
                .and_then(|v| v.as_str())
                .map(String::from),
            _ => None,
        }
    }

    /// Persist a decision to storage and update in-memory caches.
    async fn persist_decision(&self, record: &DecisionRecord) -> Result<()> {
        // 1. Save to JSONL storage
        self.storage.save_decision(record)?;

        // 2. Update exact cache
        self.exact_cache.insert(record.clone());

        // 3. Update token Jaccard index
        self.token_jaccard.insert(record);

        // 4. Update embedding similarity index (may fail if model not loaded)
        if let Err(e) = self.embedding_similarity.insert(record) {
            // Log but don't fail -- embedding index is optional
            eprintln!("captain-hook: embedding index update failed: {}", e);
        }

        Ok(())
    }
}
