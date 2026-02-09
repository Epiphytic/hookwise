use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::RwLock;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::decision::{
    CacheKey, Decision, DecisionMetadata, DecisionRecord, DecisionTier, ScopeLevel,
};
use crate::error::{CaptainHookError, Result};
use crate::scope::ScopeLevel as ScopeLevelType;

/// A pending decision waiting for human response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingDecision {
    pub id: String,
    pub session_id: String,
    pub role: String,
    pub tool_name: String,
    pub sanitized_input: String,
    pub file_path: Option<String>,
    pub recommendation: Option<SupervisorRecommendation>,
    pub is_ask_reprompt: bool,
    pub ask_reason: Option<String>,
    pub queued_at: DateTime<Utc>,
}

/// The supervisor's recommendation accompanying a human prompt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SupervisorRecommendation {
    pub decision: Decision,
    pub confidence: f64,
    pub reason: String,
}

/// A human's response to a pending decision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HumanResponse {
    pub decision: Decision,
    pub always_ask: bool,
    pub add_rule: bool,
    pub rule_scope: Option<ScopeLevelType>,
}

/// File-backed queue state persisted to disk so separate CLI processes can interact.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QueueFileState {
    pub pending: HashMap<String, PendingDecision>,
    pub responses: HashMap<String, HumanResponse>,
}

/// Returns the path for the file-backed pending queue.
/// Includes CLAUDE_TEAM_ID in the filename to isolate per-team state
/// and prevent cross-process interference when multiple teams run concurrently.
pub fn pending_queue_path() -> PathBuf {
    let team_suffix = std::env::var("CLAUDE_TEAM_ID")
        .map(|id| format!("-{}", id))
        .unwrap_or_default();
    let filename = format!("captain-hook-pending{}.json", team_suffix);

    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        PathBuf::from(runtime_dir).join(filename)
    } else {
        PathBuf::from("/tmp").join(filename)
    }
}

/// Load the file-backed queue state from disk.
pub fn load_queue_file() -> QueueFileState {
    let path = pending_queue_path();
    match std::fs::read_to_string(&path) {
        Ok(contents) => serde_json::from_str(&contents).unwrap_or_default(),
        Err(_) => QueueFileState::default(),
    }
}

/// Save the file-backed queue state to disk.
fn save_queue_file(state: &QueueFileState) -> Result<()> {
    let path = pending_queue_path();
    let json = serde_json::to_string_pretty(state)?;
    std::fs::write(&path, json)?;
    Ok(())
}

/// The decision queue for human-in-the-loop interactions.
/// Uses both in-memory state (for the running process) and file-backed state
/// (for cross-process communication with the queue/approve/deny CLI).
pub struct DecisionQueue {
    pending: RwLock<HashMap<String, PendingDecision>>,
    completed: RwLock<HashMap<String, HumanResponse>>,
}

impl Default for DecisionQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl DecisionQueue {
    pub fn new() -> Self {
        Self {
            pending: RwLock::new(HashMap::new()),
            completed: RwLock::new(HashMap::new()),
        }
    }

    pub fn enqueue(&self, decision: PendingDecision) -> String {
        let id = decision.id.clone();
        {
            let mut pending = self.pending.write().unwrap_or_else(|e| e.into_inner());
            pending.insert(id.clone(), decision.clone());
        }
        // Also write to file for cross-process visibility
        let mut state = load_queue_file();
        state.pending.insert(id.clone(), decision);
        let _ = save_queue_file(&state);
        id
    }

    pub fn list_pending(&self) -> Vec<PendingDecision> {
        // Read from file to get cross-process state
        let state = load_queue_file();
        state.pending.values().cloned().collect()
    }

    pub fn get_pending(&self, id: &str) -> Option<PendingDecision> {
        let state = load_queue_file();
        state.pending.get(id).cloned()
    }

    pub fn respond(&self, id: &str, response: HumanResponse) -> Result<()> {
        {
            let mut pending = self.pending.write().unwrap_or_else(|e| e.into_inner());
            pending.remove(id);
        }
        {
            let mut completed = self.completed.write().unwrap_or_else(|e| e.into_inner());
            completed.insert(id.to_string(), response.clone());
        }
        // Also write to file for cross-process visibility
        let mut state = load_queue_file();
        state.pending.remove(id);
        state.responses.insert(id.to_string(), response);
        save_queue_file(&state)?;
        Ok(())
    }

    pub async fn wait_for_response(&self, id: &str, timeout_secs: u64) -> Result<HumanResponse> {
        let start = std::time::Instant::now();
        let timeout = std::time::Duration::from_secs(timeout_secs);

        loop {
            // Check in-memory first
            if let Some(response) = self.take_response(id) {
                return Ok(response);
            }

            // Then check file-backed state (response from another process)
            let mut state = load_queue_file();
            if let Some(response) = state.responses.remove(id) {
                state.pending.remove(id);
                let _ = save_queue_file(&state);
                // Also update in-memory state
                let mut pending = self.pending.write().unwrap_or_else(|e| e.into_inner());
                pending.remove(id);
                return Ok(response);
            }

            if start.elapsed() >= timeout {
                // Remove the pending decision on timeout
                {
                    let mut pending = self.pending.write().unwrap_or_else(|e| e.into_inner());
                    pending.remove(id);
                }
                // Also clean up file
                let mut state = load_queue_file();
                state.pending.remove(id);
                let _ = save_queue_file(&state);

                return Err(CaptainHookError::HumanTimeout { timeout_secs });
            }

            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    }

    pub fn take_response(&self, id: &str) -> Option<HumanResponse> {
        let mut completed = self.completed.write().unwrap_or_else(|e| e.into_inner());
        completed.remove(id)
    }
}

/// Tier 4: Human-in-the-loop.
pub struct HumanTier {
    queue: std::sync::Arc<DecisionQueue>,
    timeout_secs: u64,
}

impl HumanTier {
    pub fn new(queue: std::sync::Arc<DecisionQueue>, timeout_secs: u64) -> Self {
        Self {
            queue,
            timeout_secs,
        }
    }
}

#[async_trait]
impl crate::cascade::CascadeTier for HumanTier {
    async fn evaluate(
        &self,
        input: &crate::cascade::CascadeInput,
    ) -> Result<Option<DecisionRecord>> {
        let role_name = input
            .session
            .role
            .as_ref()
            .map(|r| r.name.clone())
            .unwrap_or_else(|| "*".to_string());

        // Generate a unique ID for this pending decision
        let id = format!(
            "{}-{}-{}",
            role_name,
            input.tool_name,
            Utc::now().timestamp_millis()
        );

        let pending = PendingDecision {
            id: id.clone(),
            session_id: String::new(), // Filled by CascadeRunner
            role: role_name.clone(),
            tool_name: input.tool_name.clone(),
            sanitized_input: input.sanitized_input.clone(),
            file_path: input.file_path.clone(),
            recommendation: None,
            is_ask_reprompt: false,
            ask_reason: None,
            queued_at: Utc::now(),
        };

        self.queue.enqueue(pending);

        // Wait for human response
        let response = self.queue.wait_for_response(&id, self.timeout_secs).await?;

        // The decision from the human. If always_ask, store as Ask.
        let effective_decision = if response.always_ask {
            Decision::Ask
        } else {
            response.decision
        };

        Ok(Some(DecisionRecord {
            key: CacheKey {
                sanitized_input: input.sanitized_input.clone(),
                tool: input.tool_name.clone(),
                role: role_name,
            },
            decision: effective_decision,
            metadata: DecisionMetadata {
                tier: DecisionTier::Human,
                confidence: 1.0,
                reason: format!("human decision: {}", response.decision),
                matched_key: None,
                similarity_score: None,
            },
            timestamp: Utc::now(),
            scope: response.rule_scope.unwrap_or(ScopeLevel::Project),
            file_path: input.file_path.clone(),
            session_id: String::new(), // Filled by CascadeRunner
        }))
    }

    fn tier(&self) -> crate::decision::DecisionTier {
        crate::decision::DecisionTier::Human
    }

    fn name(&self) -> &str {
        "human"
    }
}
