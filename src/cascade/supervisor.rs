use async_trait::async_trait;
use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::config::PolicyConfig;
use crate::decision::{
    CacheKey, Decision, DecisionMetadata, DecisionRecord, DecisionTier, ScopeLevel,
};
use crate::error::{CaptainHookError, Result};

/// Request sent to the supervisor for evaluation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SupervisorRequest {
    pub session_id: String,
    pub role: String,
    pub role_description: String,
    pub tool_name: String,
    pub sanitized_input: String,
    pub file_path: Option<String>,
    pub task_description: Option<String>,
    pub agent_prompt_path: Option<String>,
    pub cwd: String,
}

/// Response from the supervisor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SupervisorResponse {
    pub decision: Decision,
    pub confidence: f64,
    pub reason: String,
}

/// Pluggable supervisor backend trait.
#[async_trait]
pub trait SupervisorBackend: Send + Sync {
    async fn evaluate(
        &self,
        request: &SupervisorRequest,
        policy: &PolicyConfig,
    ) -> Result<DecisionRecord>;
}

/// Unix socket supervisor -- communicates with a Claude Code subagent.
pub struct UnixSocketSupervisor {
    socket_path: std::path::PathBuf,
    timeout_secs: u64,
}

impl UnixSocketSupervisor {
    pub fn new(socket_path: std::path::PathBuf, timeout_secs: u64) -> Self {
        Self {
            socket_path,
            timeout_secs,
        }
    }
}

#[async_trait]
impl SupervisorBackend for UnixSocketSupervisor {
    async fn evaluate(
        &self,
        request: &SupervisorRequest,
        _policy: &PolicyConfig,
    ) -> Result<DecisionRecord> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::UnixStream;

        if !self.socket_path.exists() {
            return Err(CaptainHookError::SocketNotFound {
                path: self.socket_path.clone(),
            });
        }

        let timeout = std::time::Duration::from_secs(self.timeout_secs);

        let result = tokio::time::timeout(timeout, async {
            let mut stream = UnixStream::connect(&self.socket_path).await.map_err(|e| {
                CaptainHookError::Ipc {
                    reason: format!("connect failed: {}", e),
                }
            })?;

            // Send request as JSON line
            let request_json = serde_json::to_string(request)?;
            stream
                .write_all(request_json.as_bytes())
                .await
                .map_err(|e| CaptainHookError::Ipc {
                    reason: format!("write failed: {}", e),
                })?;
            stream
                .write_all(b"\n")
                .await
                .map_err(|e| CaptainHookError::Ipc {
                    reason: format!("write newline failed: {}", e),
                })?;
            stream.shutdown().await.map_err(|e| CaptainHookError::Ipc {
                reason: format!("shutdown write failed: {}", e),
            })?;

            // Read response (bounded to 1MB to prevent OOM)
            let mut response_buf = Vec::new();
            stream
                .take(1_048_576)
                .read_to_end(&mut response_buf)
                .await
                .map_err(|e| CaptainHookError::Ipc {
                    reason: format!("read failed: {}", e),
                })?;

            let response: SupervisorResponse =
                serde_json::from_slice(&response_buf).map_err(|e| {
                    CaptainHookError::Supervisor {
                        reason: format!("invalid response: {}", e),
                    }
                })?;

            Ok::<SupervisorResponse, CaptainHookError>(response)
        })
        .await;

        let response = match result {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => return Err(e),
            Err(_) => {
                return Err(CaptainHookError::SupervisorTimeout {
                    timeout_secs: self.timeout_secs,
                })
            }
        };

        Ok(DecisionRecord {
            key: CacheKey {
                sanitized_input: request.sanitized_input.clone(),
                tool: request.tool_name.clone(),
                role: request.role.clone(),
            },
            decision: response.decision,
            metadata: DecisionMetadata {
                tier: DecisionTier::Supervisor,
                confidence: response.confidence,
                reason: response.reason,
                matched_key: None,
                similarity_score: None,
            },
            timestamp: Utc::now(),
            scope: ScopeLevel::Project,
            file_path: request.file_path.clone(),
            session_id: request.session_id.clone(),
        })
    }
}

/// API supervisor -- calls the Anthropic API directly.
pub struct ApiSupervisor {
    client: reqwest::Client,
    api_base_url: String,
    api_key: String,
    model: String,
    max_tokens: u32,
}

impl ApiSupervisor {
    pub fn new(api_base_url: String, api_key: String, model: String, max_tokens: u32) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_base_url,
            api_key,
            model,
            max_tokens,
        }
    }

    fn build_system_prompt(&self, policy: &PolicyConfig) -> String {
        format!(
            "You are a permission supervisor for captain-hook. \
            Evaluate whether a tool call should be allowed, denied, or escalated to a human.\n\n\
            Policy:\n\
            - Sensitive paths: {:?}\n\
            - Confidence thresholds: org={}, project={}, user={}\n\n\
            Respond with JSON: {{\"decision\": \"allow\"|\"deny\"|\"ask\", \"confidence\": 0.0-1.0, \"reason\": \"...\"}}",
            policy.sensitive_paths.ask_write,
            policy.confidence.org,
            policy.confidence.project,
            policy.confidence.user,
        )
    }

    fn build_user_message(&self, request: &SupervisorRequest) -> String {
        let mut msg = format!(
            "Role: {} ({})\nTool: {}\nInput: {}\nCWD: {}",
            request.role,
            request.role_description,
            request.tool_name,
            request.sanitized_input,
            request.cwd,
        );
        if let Some(fp) = &request.file_path {
            msg.push_str(&format!("\nFile path: {}", fp));
        }
        if let Some(task) = &request.task_description {
            msg.push_str(&format!("\nTask: {}", task));
        }
        msg
    }

    fn parse_response(&self, response_text: &str) -> Result<SupervisorResponse> {
        // Try to extract JSON from the response (it might have surrounding text)
        let json_start = response_text.find('{');
        let json_end = response_text.rfind('}');

        match (json_start, json_end) {
            (Some(start), Some(end)) if start < end => {
                let json_str = &response_text[start..=end];
                serde_json::from_str(json_str).map_err(|e| CaptainHookError::Supervisor {
                    reason: format!("failed to parse supervisor JSON: {}", e),
                })
            }
            _ => Err(CaptainHookError::Supervisor {
                reason: format!("no JSON found in supervisor response: {}", response_text),
            }),
        }
    }
}

#[async_trait]
impl SupervisorBackend for ApiSupervisor {
    async fn evaluate(
        &self,
        request: &SupervisorRequest,
        policy: &PolicyConfig,
    ) -> Result<DecisionRecord> {
        let system_prompt = self.build_system_prompt(policy);
        let user_message = self.build_user_message(request);

        let body = serde_json::json!({
            "model": self.model,
            "max_tokens": self.max_tokens,
            "system": system_prompt,
            "messages": [{"role": "user", "content": user_message}]
        });

        let resp = self
            .client
            .post(format!("{}/v1/messages", self.api_base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| CaptainHookError::Supervisor {
                reason: format!("API request failed: {}", e),
            })?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body_text = resp.text().await.unwrap_or_default();
            return Err(CaptainHookError::Api {
                status,
                body: body_text,
            });
        }

        let resp_json: serde_json::Value =
            resp.json()
                .await
                .map_err(|e| CaptainHookError::Supervisor {
                    reason: format!("failed to parse API response: {}", e),
                })?;

        // Extract text from Anthropic Messages API response
        let text = resp_json["content"]
            .as_array()
            .and_then(|arr| arr.first())
            .and_then(|block| block["text"].as_str())
            .unwrap_or("");

        let supervisor_response = self.parse_response(text)?;

        Ok(DecisionRecord {
            key: CacheKey {
                sanitized_input: request.sanitized_input.clone(),
                tool: request.tool_name.clone(),
                role: request.role.clone(),
            },
            decision: supervisor_response.decision,
            metadata: DecisionMetadata {
                tier: DecisionTier::Supervisor,
                confidence: supervisor_response.confidence,
                reason: supervisor_response.reason,
                matched_key: None,
                similarity_score: None,
            },
            timestamp: Utc::now(),
            scope: ScopeLevel::Project,
            file_path: request.file_path.clone(),
            session_id: request.session_id.clone(),
        })
    }
}

/// Wraps a SupervisorBackend as a CascadeTier.
pub struct SupervisorTier {
    backend: Box<dyn SupervisorBackend>,
    policy: PolicyConfig,
}

impl SupervisorTier {
    pub fn new(backend: Box<dyn SupervisorBackend>, policy: PolicyConfig) -> Self {
        Self { backend, policy }
    }
}

#[async_trait]
impl crate::cascade::CascadeTier for SupervisorTier {
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

        let role_description = input
            .session
            .role
            .as_ref()
            .map(|r| r.description.clone())
            .unwrap_or_default();

        let request = SupervisorRequest {
            session_id: String::new(), // Filled by CascadeRunner
            role: role_name,
            role_description,
            tool_name: input.tool_name.clone(),
            sanitized_input: input.sanitized_input.clone(),
            file_path: input.file_path.clone(),
            task_description: input.session.task_description.clone(),
            agent_prompt_path: input
                .session
                .agent_prompt_path
                .as_ref()
                .map(|p| p.display().to_string()),
            cwd: String::new(), // Filled by CascadeRunner
        };

        let record = match self.backend.evaluate(&request, &self.policy).await {
            Ok(r) => r,
            Err(e) => {
                eprintln!(
                    "captain-hook: supervisor unavailable, falling through ({})",
                    e
                );
                return Ok(None);
            }
        };

        // If supervisor has low confidence, return None to escalate to human
        if record.metadata.confidence < self.policy.confidence.project {
            return Ok(None);
        }

        Ok(Some(record))
    }

    fn tier(&self) -> crate::decision::DecisionTier {
        crate::decision::DecisionTier::Supervisor
    }

    fn name(&self) -> &str {
        "supervisor"
    }
}
