// SPDX-License-Identifier: MIT
// Copyright (c) 2024 Skillberry Contributors

//! MCP tools enricher filter: reads tools from filter metadata and injects them
//! into OpenAI-compatible chat completion request bodies.

mod config;

#[cfg(test)]
#[expect(clippy::allow_attributes, reason = "blanket test suppressions")]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "tests"
)]
mod tests;

use std::borrow::Cow;

use async_trait::async_trait;
use bytes::Bytes;
use tracing::{info, warn};

use self::config::{InvalidBodyBehavior, McpToolsEnricherConfig, validate_config};
use praxis_filter::{
    FilterAction, FilterError,
    BodyAccess, BodyMode,
    parse_filter_config,
    HttpFilter, HttpFilterContext,
};

/// Fetches MCP tools from filter metadata (set by `vmcp_manager`) and injects
/// them into the `tools` array of OpenAI-compatible chat completion request bodies.
pub struct McpToolsEnricherFilter {
    max_body_bytes: usize,
    on_invalid: InvalidBodyBehavior,
    tool_choice: String,
}

impl McpToolsEnricherFilter {
    /// Create from parsed YAML config.
    pub fn from_config(config: &serde_yaml::Value) -> Result<Box<dyn HttpFilter>, FilterError> {
        let cfg: McpToolsEnricherConfig = parse_filter_config("mcp_tools_enricher", config)?;
        validate_config(&cfg)?;

        Ok(Box::new(Self {
            max_body_bytes: cfg.max_body_bytes,
            on_invalid: cfg.on_invalid,
            tool_choice: cfg.tool_choice,
        }))
    }
}

#[async_trait]
impl HttpFilter for McpToolsEnricherFilter {
    fn name(&self) -> &'static str {
        "mcp_tools_enricher"
    }

    async fn on_request(&self, _ctx: &mut HttpFilterContext<'_>) -> Result<FilterAction, FilterError> {
        Ok(FilterAction::Continue)
    }

    fn request_body_access(&self) -> BodyAccess {
        BodyAccess::ReadWrite
    }

    fn request_body_mode(&self) -> BodyMode {
        BodyMode::StreamBuffer {
            max_bytes: Some(self.max_body_bytes),
        }
    }

    async fn on_request_body(
        &self,
        ctx: &mut HttpFilterContext<'_>,
        body: &mut Option<Bytes>,
        end_of_stream: bool,
    ) -> Result<FilterAction, FilterError> {
        if !end_of_stream {
            return Ok(FilterAction::Continue);
        }

        let Some(raw) = body.as_ref() else {
            return Ok(FilterAction::Continue);
        };

        // Get tools from filter_metadata (set by vmcp_manager)
        let tools_json_owned = match ctx.filter_metadata.get("mcp_tools") {
            Some(json) => json.clone(),
            None => return Ok(FilterAction::Continue),
        };

        let tools: Vec<serde_json::Value> = match serde_json::from_str(&tools_json_owned) {
            Ok(t) => t,
            Err(e) => {
                warn!("mcp_tools_enricher: failed to parse tools from metadata: {}", e);
                return Ok(FilterAction::Continue);
            }
        };

        info!("mcp_tools_enricher: enriching request body with {} tools", tools.len());

        // Parse the request body
        let mut value: serde_json::Value = match serde_json::from_slice(raw) {
            Ok(v) => v,
            Err(e) => {
                warn!("mcp_tools_enricher: failed to parse request body as JSON: {}", e);
                return Ok(invalid_body_action(self.on_invalid, "invalid JSON body"));
            }
        };

        enrich_request_with_tools(&mut value, tools, &self.tool_choice)?;

        let serialized = serde_json::to_vec(&value)
            .map_err(|e| -> FilterError { format!("mcp_tools_enricher: {e}").into() })?;

        let len = serialized.len();
        *body = Some(Bytes::from(serialized));

        ctx.extra_request_headers
            .push((Cow::Borrowed("content-length"), len.to_string()));

        info!("mcp_tools_enricher: request body enriched successfully");

        Ok(FilterAction::Continue)
    }
}

/// Enrich the request body with MCP tools.
fn enrich_request_with_tools(
    value: &mut serde_json::Value,
    tools: Vec<serde_json::Value>,
    tool_choice: &str,
) -> Result<(), FilterError> {
    let obj = value
        .as_object_mut()
        .ok_or_else(|| -> FilterError { "request body is not a JSON object".into() })?;

    let tools_count = tools.len();
    match obj.get_mut("tools") {
        Some(existing_tools) => {
            if let Some(existing_array) = existing_tools.as_array_mut() {
                existing_array.extend(tools);
                info!("Merged {} MCP tools with existing tools", tools_count);
            } else {
                warn!("Existing 'tools' field is not an array, replacing it");
                obj.insert("tools".to_owned(), serde_json::Value::Array(tools));
            }
        }
        None => {
            obj.insert("tools".to_owned(), serde_json::Value::Array(tools));
            info!("Added {} MCP tools to request", tools_count);
        }
    }

    if !obj.contains_key("tool_choice") {
        obj.insert(
            "tool_choice".to_owned(),
            serde_json::Value::String(tool_choice.to_owned()),
        );
    }

    Ok(())
}

/// Map [`InvalidBodyBehavior`] to the appropriate [`FilterAction`].
fn invalid_body_action(behavior: InvalidBodyBehavior, message: &'static str) -> FilterAction {
    use praxis_filter::Rejection;
    match behavior {
        InvalidBodyBehavior::Continue => FilterAction::Continue,
        InvalidBodyBehavior::Reject => FilterAction::Reject(
            Rejection::status(400)
                .with_header("content-type", "text/plain")
                .with_body(message),
        ),
    }
}
