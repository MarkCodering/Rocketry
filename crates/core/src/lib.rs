//! Shared contracts. No network, database, or terminal implementation lives here.
use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub fn id() -> String {
    uuid::Uuid::new_v4().to_string()
}
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    pub name: String,
    pub instructions: String,
    pub provider: String,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub output_schema: Option<Value>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub text: String,
    #[serde(default)]
    pub calls: Vec<ToolCall>,
    #[serde(default)]
    pub call_id: Option<String>,
    #[serde(default)]
    pub provider_data: BTreeMap<String, Value>,
}
impl Message {
    pub fn text(role: &str, text: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            text: text.into(),
            calls: vec![],
            call_id: None,
            provider_data: BTreeMap::new(),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub schema: Value,
    pub effect: Effect,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    Read,
    Write,
    Process,
    External,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub estimated_cost_usd: Option<f64>,
}
#[derive(Debug, Clone)]
pub enum ModelEvent {
    Text(String),
    Call(ToolCall),
    Usage(Usage),
    Continuation(String, Value),
    Finished,
}
#[derive(Debug, Clone)]
pub struct ModelRequest {
    pub instructions: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub max_output_tokens: u32,
    pub output_schema: Option<Value>,
}
#[async_trait]
pub trait ModelProvider: Send + Sync {
    async fn stream(
        &self,
        request: ModelRequest,
        events: mpsc::Sender<ModelEvent>,
        cancel: CancellationToken,
    ) -> Result<()>;
}
#[derive(Clone)]
pub struct ToolContext {
    pub run_id: String,
    pub workspace: PathBuf,
    pub namespace: String,
    pub cancel: CancellationToken,
}
#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    async fn execute(&self, args: Value, context: ToolContext) -> Result<Value>;
}
#[async_trait]
pub trait ExecutionBackend: Send + Sync {
    async fn execute(&self, operation: &str, args: Value, context: ToolContext) -> Result<Value>;
    fn name(&self) -> &'static str;
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Allow,
    Deny,
    Ask,
}
pub trait ApprovalPolicy: Send + Sync {
    fn decide(&self, spec: &ToolSpec, call: &ToolCall) -> Decision;
}
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PermissionPolicy {
    #[serde(default)]
    pub allowed: Vec<String>,
    #[serde(default)]
    pub denied: Vec<String>,
    #[serde(default)]
    pub allow_reads: bool,
}
impl ApprovalPolicy for PermissionPolicy {
    fn decide(&self, spec: &ToolSpec, _: &ToolCall) -> Decision {
        if self.denied.contains(&spec.name) {
            Decision::Deny
        } else if self.allowed.contains(&spec.name)
            || (self.allow_reads && spec.effect == Effect::Read)
        {
            Decision::Allow
        } else {
            Decision::Ask
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Limits {
    pub active_runs: usize,
    pub parallel_tools: usize,
    pub children: usize,
    pub depth: usize,
    pub turns: usize,
    pub timeout_secs: u64,
    pub output_tokens: u32,
    pub context_bytes: usize,
    pub tool_output_bytes: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            active_runs: 100,
            parallel_tools: 4,
            children: 8,
            depth: 3,
            turns: 50,
            timeout_secs: 900,
            output_tokens: 4096,
            context_bytes: 128 * 1024,
            tool_output_bytes: 32 * 1024,
        }
    }
}
impl Limits {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.active_runs > 0
                && self.parallel_tools > 0
                && self.turns > 0
                && self.timeout_secs > 0
                && self.output_tokens > 0
                && self.context_bytes >= 4096
                && self.tool_output_bytes >= 1024,
            "limits must be positive; context >=4096 and tool output >=1024 bytes"
        );
        Ok(())
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Queued,
    Running,
    AwaitingApproval,
    NeedsReconciliation,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}
impl RunStatus {
    pub fn terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed
                | Self::Failed
                | Self::Cancelled
                | Self::Interrupted
                | Self::NeedsReconciliation
        )
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub title: String,
    pub created_at: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    pub id: String,
    pub session_id: String,
    pub parent_id: Option<String>,
    pub agent: Agent,
    pub status: RunStatus,
    pub created_at: u64,
    pub error: Option<String>,
    pub workspace: PathBuf,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum EventKind {
    ModelStarted,
    Status(RunStatus),
    Text(String),
    ToolStarted(ToolCall),
    ToolFinished {
        call: ToolCall,
        output: Value,
        error: bool,
    },
    Approval(Approval),
    Usage(Usage),
    Child {
        id: String,
        name: String,
    },
    Compacted {
        before: usize,
        after: usize,
    },
    Error(String),
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub version: u8,
    pub sequence: i64,
    pub run_id: String,
    pub timestamp: u64,
    pub kind: EventKind,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Approval {
    pub id: String,
    pub run_id: String,
    pub call: ToolCall,
    pub decision: Option<bool>,
}
#[async_trait]
pub trait SessionStore: Send + Sync {
    async fn create_session(&self, title: &str) -> Result<Session>;
    async fn sessions(&self) -> Result<Vec<Session>>;
    async fn messages(&self, session: &str) -> Result<Vec<Message>>;
    async fn add_message(&self, session: &str, message: &Message) -> Result<()>;
    async fn save_run(&self, run: &Run) -> Result<()>;
    async fn run(&self, id: &str) -> Result<Run>;
    async fn runs(&self) -> Result<Vec<Run>>;
    async fn append_event(&self, run: &str, kind: EventKind) -> Result<Event>;
    async fn events(&self, run: &str, after: i64, limit: usize) -> Result<Vec<Event>>;
}
pub type ProviderRegistry = BTreeMap<String, Arc<dyn ModelProvider>>;
pub type ToolRegistry = BTreeMap<String, Arc<dyn Tool>>;

/// An external request may have taken effect; automatic retries are forbidden.
#[derive(Debug)]
pub struct UncertainEffect(pub String);
impl std::fmt::Display for UncertainEffect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for UncertainEffect {}
