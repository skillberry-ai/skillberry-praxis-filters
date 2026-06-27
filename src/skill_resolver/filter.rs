// SPDX-License-Identifier: MIT
// Copyright (c) 2024 Skillberry Contributors

//! Skill resolver filter implementation.

use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use reqwest::Client;
use serde::Deserialize;

use super::config::SkillResolverConfig;
use praxis_filter::{
    FilterAction, FilterError,
    BodyAccess, BodyMode,
    parse_filter_config,
    HttpFilter, HttpFilterContext,
};

/// Response from skillberry-store GET /skills/{uuid_or_name} endpoint.
#[derive(Debug, Deserialize)]
struct SkillResponse {
    uuid: String,
    #[allow(dead_code)]
    name: String,
    #[allow(dead_code)]
    description: Option<String>,
}

/// Resolves the skill to apply to this request.
///
/// Runs in `on_request_body` so that future iterations can inspect the full
/// incoming chat messages (e.g. the `messages` array in an OpenAI
/// `/v1/chat/completions` body) to pick the right skill dynamically.
///
/// **Current behaviour** (env-variable-only):
/// 1. `SKILL_UUID` env var → use directly.
/// 2. `SKILL_NAME` env var → look up UUID via `GET {store_base_url}/skills/{name}`.
/// 3. Neither set → continue without a skill.
///
/// **Planned extension**: once the body is available the filter will parse the
/// `messages` array and select the best-matching skill from the store based on
/// message content, making static env-var configuration optional.
///
/// Writes `skill_uuid`, `skill_name`, `skill_resolution_method` (and
/// `skill_resolution_error` on failure) into `ctx.filter_metadata` for use by
/// downstream filters (`vmcp_manager`, `mcp_tools_enricher`).
pub struct SkillResolverFilter {
    http_client: Client,
    store_base_url: String,
    skill_uuid_env: String,
    skill_name_env: String,
    #[allow(dead_code)]
    timeout: Duration,
}

impl SkillResolverFilter {
    /// Create from YAML config.
    pub fn from_config(config: &serde_yaml::Value) -> Result<Box<dyn HttpFilter>, FilterError> {
        let cfg: SkillResolverConfig = parse_filter_config("skill_resolver", config)?;

        if cfg.store_base_url.is_empty() {
            return Err("skill_resolver: 'store_base_url' must not be empty".into());
        }

        let http_client = Client::builder()
            .timeout(Duration::from_millis(cfg.timeout_ms))
            .build()
            .map_err(|e| -> FilterError {
                format!("skill_resolver: failed to create HTTP client: {e}").into()
            })?;

        Ok(Box::new(Self {
            http_client,
            store_base_url: cfg.store_base_url,
            skill_uuid_env: cfg.skill_uuid_env,
            skill_name_env: cfg.skill_name_env,
            timeout: Duration::from_millis(cfg.timeout_ms),
        }))
    }

    fn get_skill_uuid_from_env(&self) -> Option<String> {
        std::env::var(&self.skill_uuid_env).ok()
    }

    fn get_skill_name_from_env(&self) -> Option<String> {
        std::env::var(&self.skill_name_env).ok()
    }

    async fn lookup_skill_by_name(&self, skill_name: &str) -> Result<SkillResponse, FilterError> {
        let url = format!("{}/skills/{}", self.store_base_url, skill_name);

        tracing::debug!(
            skill_name = %skill_name,
            url = %url,
            "looking up skill via API"
        );

        let response = self.http_client
            .get(&url)
            .send()
            .await
            .map_err(|e| -> FilterError {
                if e.is_timeout() {
                    tracing::error!(skill_name = %skill_name, "skill lookup timed out");
                    FilterError::from("skill lookup timed out")
                } else if e.is_connect() {
                    tracing::error!(
                        skill_name = %skill_name,
                        error = %e,
                        "failed to connect to skillberry-store"
                    );
                    Box::new(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        "skillberry-store is unreachable",
                    ))
                } else {
                    tracing::error!(
                        skill_name = %skill_name,
                        error = %e,
                        "skill lookup request failed"
                    );
                    FilterError::from(format!("skill lookup failed: {e}"))
                }
            })?;

        let status = response.status();

        if status.is_success() {
            response.json::<SkillResponse>().await
                .map_err(|e| -> FilterError {
                    tracing::error!(
                        skill_name = %skill_name,
                        error = %e,
                        "failed to parse skill response"
                    );
                    FilterError::from(format!("invalid skill response: {e}"))
                })
        } else if status.as_u16() == 404 {
            tracing::warn!(skill_name = %skill_name, "skill not found in store");
            Err(FilterError::from(format!("skill '{}' not found", skill_name)))
        } else {
            tracing::error!(
                skill_name = %skill_name,
                status = %status,
                "skill lookup returned error status"
            );
            Err(FilterError::from(format!("skill lookup failed with status {}", status)))
        }
    }
}

#[async_trait]
impl HttpFilter for SkillResolverFilter {
    fn name(&self) -> &'static str {
        "skill_resolver"
    }

    fn request_body_access(&self) -> BodyAccess {
        // ReadOnly today; will be upgraded to ReadWrite once the filter parses
        // and potentially rewrites messages for skill matching.
        BodyAccess::ReadOnly
    }

    fn request_body_mode(&self) -> BodyMode {
        // Buffer the full body so future logic can inspect the `messages` array.
        BodyMode::StreamBuffer {
            max_bytes: Some(10_485_760),
        }
    }

    /// Runs once the complete request body has been received.
    ///
    /// Currently resolves the skill from environment variables only.
    /// The `_body` parameter is not yet inspected but will be used in a future
    /// iteration to read the incoming chat messages and select the best
    /// matching skill dynamically.
    async fn on_request_body(
        &self,
        ctx: &mut HttpFilterContext<'_>,
        _body: &mut Option<Bytes>,
        end_of_stream: bool,
    ) -> Result<FilterAction, FilterError> {
        if !end_of_stream {
            return Ok(FilterAction::Continue);
        }

        // Priority 1: Check for direct UUID in environment
        if let Some(skill_uuid) = self.get_skill_uuid_from_env() {
            tracing::info!(
                skill_uuid = %skill_uuid,
                "using skill UUID from environment variable"
            );
            ctx.filter_metadata.insert("skill_uuid".to_string(), skill_uuid);
            ctx.filter_metadata.insert("skill_resolution_method".to_string(), "env_uuid".to_string());
            return Ok(FilterAction::Continue);
        }

        // Priority 2: Check for skill name in environment
        if let Some(skill_name) = self.get_skill_name_from_env() {
            tracing::info!(
                skill_name = %skill_name,
                "resolving skill UUID from name via API"
            );

            match self.lookup_skill_by_name(&skill_name).await {
                Ok(skill) => {
                    tracing::info!(
                        skill_name = %skill_name,
                        skill_uuid = %skill.uuid,
                        "successfully resolved skill UUID"
                    );
                    ctx.filter_metadata.insert("skill_uuid".to_string(), skill.uuid);
                    ctx.filter_metadata.insert("skill_name".to_string(), skill_name);
                    ctx.filter_metadata.insert("skill_resolution_method".to_string(), "api_lookup".to_string());
                    return Ok(FilterAction::Continue);
                }
                Err(e) => {
                    tracing::warn!(
                        skill_name = %skill_name,
                        error = %e,
                        "failed to resolve skill, continuing without skill"
                    );
                    ctx.filter_metadata.insert("skill_resolution_error".to_string(), e.to_string());
                    return Ok(FilterAction::Continue);
                }
            }
        }

        // Priority 3: Neither UUID nor name set
        tracing::debug!("no skill UUID or name configured, continuing without skill");
        ctx.filter_metadata.insert("skill_resolution_method".to_string(), "none".to_string());
        Ok(FilterAction::Continue)
    }

    async fn on_request(&self, _ctx: &mut HttpFilterContext<'_>) -> Result<FilterAction, FilterError> {
        Ok(FilterAction::Continue)
    }
}
