//! MCP calls are external effects, independent of workspace sandbox policy.
use super::*;
use rmcp::{
    RoleClient, ServiceExt,
    model::CallToolRequestParam,
    service::RunningService,
    transport::{
        StreamableHttpClientTransport, TokioChildProcess,
        streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpConfig {
    pub name: String,
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    pub url: Option<String>,
    pub token_env: Option<String>,
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
}
struct Connection {
    config: McpConfig,
    service: tokio::sync::Mutex<Option<RunningService<RoleClient, ()>>>,
}
impl Connection {
    async fn connect(&self) -> Result<RunningService<RoleClient, ()>> {
        match (&self.config.command, &self.config.url) {
            (Some(command), None) => {
                let mut cmd = Command::new(command);
                cmd.args(&self.config.args)
                    .env_clear()
                    .env("PATH", std::env::var("PATH").unwrap_or_default())
                    .stderr(Stdio::null());
                for (key, env_name) in &self.config.env {
                    cmd.env(
                        key,
                        std::env::var(env_name).with_context(|| {
                            format!("missing MCP environment variable {env_name}")
                        })?,
                    );
                }
                Ok(().serve(TokioChildProcess::new(cmd)?).await?)
            }
            (None, Some(url)) => {
                let mut config = StreamableHttpClientTransportConfig::with_uri(url.clone());
                if let Some(env) = &self.config.token_env {
                    config = config.auth_header(std::env::var(env).context("missing MCP token")?);
                }
                Ok(
                    ().serve(StreamableHttpClientTransport::from_config(config))
                        .await?,
                )
            }
            _ => bail!("MCP requires exactly one command or URL"),
        }
    }
}
struct McpTool {
    spec: ToolSpec,
    remote_name: String,
    connection: Arc<Connection>,
}
#[async_trait]
impl Tool for McpTool {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }
    async fn execute(&self, args: Value, ctx: ToolContext) -> Result<Value> {
        let mut service = self.connection.service.lock().await;
        if service
            .as_ref()
            .is_none_or(|s| s.peer().is_transport_closed())
        {
            *service = Some(tokio::select! {
                _ = ctx.cancel.cancelled() => bail!("MCP connection cancelled"),
                result = tokio::time::timeout(std::time::Duration::from_secs(20), self.connection.connect()) => result??,
            });
        }
        let call = service.as_ref().unwrap().call_tool(CallToolRequestParam {
            name: self.remote_name.clone().into(),
            arguments: Some(
                args.as_object()
                    .context("arguments must be object")?
                    .clone(),
            ),
        });
        let result = tokio::select! {_=ctx.cancel.cancelled()=>bail!("MCP call cancelled; external completion may be unknown"),r=call=>r};
        match result {
            Ok(r) => {
                anyhow::ensure!(
                    r.is_error != Some(true),
                    "MCP tool reported an error: {}",
                    serde_json::to_string(&r.content)?
                );
                Ok(serde_json::to_value(r)?)
            }
            Err(_) => {
                *service = None;
                Err(
                    UncertainEffect("MCP transport failed; external completion is unknown".into())
                        .into(),
                )
            }
        }
    }
}
pub async fn discover(config: McpConfig) -> Result<ToolRegistry> {
    anyhow::ensure!(
        config
            .name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
            && !config.name.is_empty(),
        "MCP name must be alphanumeric or underscore"
    );
    let connection = Arc::new(Connection {
        config,
        service: tokio::sync::Mutex::new(None),
    });
    let service =
        tokio::time::timeout(std::time::Duration::from_secs(20), connection.connect()).await??;
    let tools = tokio::time::timeout(std::time::Duration::from_secs(20), service.list_all_tools())
        .await??;
    let mut registry = ToolRegistry::new();
    for t in tools {
        let name = format!("mcp_{}_{}", connection.config.name, t.name);
        let spec = ToolSpec {
            name: name.clone(),
            description: t.description.as_deref().unwrap_or("MCP tool").into(),
            schema: json!(t.input_schema),
            effect: Effect::External,
        };
        registry.insert(
            name,
            Arc::new(McpTool {
                spec,
                remote_name: t.name.into_owned(),
                connection: connection.clone(),
            }) as Arc<dyn Tool>,
        );
    }
    *connection.service.lock().await = Some(service);
    Ok(registry)
}
