// SPDX-License-Identifier: MIT
// Copyright (c) 2024 Skillberry Contributors

//! VMCP manager filter implementation.

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use mcp_client::{ClientCapabilities, ClientInfo, McpClient, McpClientTrait, McpService, Transport};
use mcp_client::transport::SseTransport;
use reqwest::Client;
use serde::Deserialize;

use super::config::VmcpManagerConfig;
use praxis_filter::{
    FilterAction, FilterError,
    BodyAccess, BodyMode,
    parse_filter_config,
    HttpFilter, HttpFilterContext,
};

/// Response from skillberry-store POST /vmcp_servers/ endpoint.
#[derive(Debug, Deserialize)]
struct VmcpResponse {
    uuid: String,
    name: String,
    port: u16,
    #[allow(dead_code)]
    skill_uuid: Option<String>,
    #[allow(dead_code)]
    runtime_tools: Option<serde_json::Value>,
}

/// Creates and manages Virtual MCP (VMCP) servers.
///
/// This filter creates VMCP servers via the skillberry-store API,
/// passing the environment context and optional skill UUID.
pub struct VmcpManagerFilter {
    http_client: Client,
    store_base_url: String,
    vmcp_name_template: String,
    always_create: bool,
    timeout: Duration,
    #[allow(dead_code)]
    cleanup_on_error: bool,
}

impl VmcpManagerFilter {
    /// Create from YAML config.
    pub fn from_config(config: &serde_yaml::Value) -> Result<Box<dyn HttpFilter>, FilterError> {
        let cfg: VmcpManagerConfig = parse_filter_config("vmcp_manager", config)?;

        if cfg.store_base_url.is_empty() {
            return Err("vmcp_manager: 'store_base_url' must not be empty".into());
        }

        let http_client = Client::builder()
            .timeout(Duration::from_millis(cfg.timeout_ms))
            .build()
            .map_err(|e| -> FilterError {
                format!("vmcp_manager: failed to create HTTP client: {e}").into()
            })?;

        Ok(Box::new(Self {
            http_client,
            store_base_url: cfg.store_base_url,
            vmcp_name_template: cfg.vmcp_name_template,
            always_create: cfg.always_create,
            timeout: Duration::from_millis(cfg.timeout_ms),
            cleanup_on_error: cfg.cleanup_on_error,
        }))
    }

    fn generate_vmcp_name(&self, env_id: &str) -> String {
        self.vmcp_name_template.replace("{env_id}", env_id)
    }

    async fn create_vmcp_server(
        &self,
        name: &str,
        skill_uuid: Option<&str>,
        env_id: &str,
    ) -> Result<VmcpResponse, FilterError> {
        let url = format!("{}/vmcp_servers/", self.store_base_url);

        tracing::debug!(
            vmcp_name = %name,
            skill_uuid = ?skill_uuid,
            env_id = %env_id,
            url = %url,
            "creating VMCP server"
        );

        let mut query_params = vec![
            ("name", name.to_string()),
            ("description", format!("VMCP server for environment {}", env_id)),
        ];

        if let Some(uuid) = skill_uuid {
            query_params.push(("skill_uuid", uuid.to_string()));
        }

        let mut request = self.http_client
            .post(&url)
            .header("skillberry-context-env-id", env_id)
            .query(&query_params);

        request = request.timeout(self.timeout);

        let response = request.send().await
            .map_err(|e| -> FilterError {
                if e.is_timeout() {
                    tracing::error!(vmcp_name = %name, "VMCP creation timed out");
                    FilterError::from("VMCP creation timed out")
                } else if e.is_connect() {
                    tracing::error!(vmcp_name = %name, error = %e, "failed to connect to skillberry-store");
                    Box::new(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        "skillberry-store is unreachable",
                    ))
                } else {
                    tracing::error!(vmcp_name = %name, error = %e, "VMCP creation request failed");
                    FilterError::from(format!("VMCP creation failed: {e}"))
                }
            })?;

        let status = response.status();

        if status.is_success() {
            response.json::<VmcpResponse>().await
                .map_err(|e| -> FilterError {
                    tracing::error!(vmcp_name = %name, error = %e, "failed to parse VMCP response");
                    FilterError::from(format!("invalid VMCP response: {e}"))
                })
        } else if status.as_u16() == 409 {
            if self.always_create {
                tracing::error!(vmcp_name = %name, "VMCP already exists but always_create is true");
                Err(FilterError::from(format!("VMCP '{}' already exists", name)))
            } else {
                tracing::info!(vmcp_name = %name, "VMCP already exists, reusing");
                Err(FilterError::from("VMCP reuse not yet implemented"))
            }
        } else {
            tracing::error!(vmcp_name = %name, status = %status, "VMCP creation returned error status");
            Err(FilterError::from(format!("VMCP creation failed with status {}", status)))
        }
    }

    async fn fetch_mcp_tools(
        &self,
        vmcp_port: u16,
        env_id: &str,
    ) -> Result<Vec<serde_json::Value>, FilterError> {
        let sse_url = format!("http://localhost:{}/sse", vmcp_port);

        tracing::debug!(
            vmcp_port = %vmcp_port,
            env_id = %env_id,
            sse_url = %sse_url,
            "fetching MCP tools via SSE"
        );

        let env = HashMap::new();
        let transport = SseTransport::new(&sse_url, env);

        let transport_handle = transport.start()
            .await
            .map_err(|e| -> FilterError {
                tracing::error!(vmcp_port = %vmcp_port, error = %e, "failed to start SSE transport");
                FilterError::from(format!("SSE transport start failed: {e}"))
            })?;

        let service = McpService::new(transport_handle);
        let mut client = McpClient::new(service);

        let client_info = ClientInfo {
            name: "praxis-vmcp-manager".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        };

        let capabilities = ClientCapabilities::default();

        let init_result = tokio::time::timeout(self.timeout, client.initialize(client_info, capabilities))
            .await
            .map_err(|_| -> FilterError {
                tracing::error!(vmcp_port = %vmcp_port, "MCP initialization timeout");
                FilterError::from("MCP initialization timeout")
            })?
            .map_err(|e| -> FilterError {
                tracing::error!(vmcp_port = %vmcp_port, error = %e, "MCP initialization failed");
                FilterError::from(format!("MCP initialization failed: {e}"))
            })?;

        tracing::debug!(
            vmcp_port = %vmcp_port,
            server_name = %init_result.server_info.name,
            server_version = %init_result.server_info.version,
            "MCP client initialized"
        );

        let tools_result = tokio::time::timeout(self.timeout, client.list_tools(None))
            .await
            .map_err(|_| -> FilterError {
                tracing::error!(vmcp_port = %vmcp_port, "MCP list_tools timeout");
                FilterError::from("MCP list_tools timeout")
            })?
            .map_err(|e| -> FilterError {
                tracing::error!(vmcp_port = %vmcp_port, error = %e, "failed to list MCP tools");
                FilterError::from(format!("MCP list_tools failed: {e}"))
            })?;

        let tools: Vec<serde_json::Value> = tools_result.tools
            .into_iter()
            .map(|tool| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.input_schema,
                    }
                })
            })
            .collect();

        tracing::info!(
            vmcp_port = %vmcp_port,
            tool_count = %tools.len(),
            "fetched MCP tools"
        );

        Ok(tools)
    }
}

#[async_trait]
impl HttpFilter for VmcpManagerFilter {
    fn name(&self) -> &'static str {
        "vmcp_manager"
    }

    fn request_body_access(&self) -> BodyAccess {
        BodyAccess::None
    }

    fn request_body_mode(&self) -> BodyMode {
        BodyMode::Stream
    }

    async fn on_request(&self, ctx: &mut HttpFilterContext<'_>) -> Result<FilterAction, FilterError> {
        let env_id = ctx.filter_metadata
            .get("env_id")
            .ok_or_else(|| {
                tracing::error!("env_id not found in filter_metadata — is context_extractor configured?");
                FilterError::from("env_id is required in filter_metadata (set by context_extractor)")
            })?
            .clone();

        let skill_uuid_owned = ctx.filter_metadata.get("skill_uuid").cloned();
        let skill_uuid = skill_uuid_owned.as_deref();
        let vmcp_name = self.generate_vmcp_name(&env_id);

        tracing::info!(
            env_id = %env_id,
            vmcp_name = %vmcp_name,
            skill_uuid = ?skill_uuid,
            "creating VMCP server"
        );

        let vmcp = match self.create_vmcp_server(&vmcp_name, skill_uuid, &env_id).await {
            Ok(vmcp) => {
                tracing::info!(
                    vmcp_name = %vmcp_name,
                    vmcp_uuid = %vmcp.uuid,
                    vmcp_port = %vmcp.port,
                    "VMCP server created successfully"
                );
                ctx.filter_metadata.insert("vmcp_uuid".to_string(), vmcp.uuid.clone());
                ctx.filter_metadata.insert("vmcp_name".to_string(), vmcp.name.clone());
                ctx.filter_metadata.insert("vmcp_port".to_string(), vmcp.port.to_string());

                if let Some(ref tools) = vmcp.runtime_tools {
                    if let Some(tools_array) = tools.as_array() {
                        ctx.filter_metadata.insert(
                            "vmcp_tools_count".to_string(),
                            tools_array.len().to_string(),
                        );
                    }
                }
                vmcp
            }
            Err(e) => {
                tracing::error!(vmcp_name = %vmcp_name, error = %e, "failed to create VMCP server");
                return Err(e);
            }
        };

        match self.fetch_mcp_tools(vmcp.port, &env_id).await {
            Ok(tools) => {
                tracing::info!(vmcp_port = %vmcp.port, tool_count = %tools.len(), "successfully fetched MCP tools");
                let tools_json = serde_json::to_string(&tools)
                    .map_err(|e| -> FilterError {
                        tracing::error!(error = %e, "failed to serialize MCP tools");
                        FilterError::from(format!("failed to serialize tools: {e}"))
                    })?;
                ctx.filter_metadata.insert("mcp_tools".to_string(), tools_json);
                Ok(FilterAction::Continue)
            }
            Err(e) => {
                tracing::error!(vmcp_port = %vmcp.port, error = %e, "failed to fetch MCP tools");
                Ok(FilterAction::Continue)
            }
        }
    }
}
