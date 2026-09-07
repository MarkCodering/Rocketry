//! Durable agent execution shared by the SDK, HTTP service, CLI and TUI.
mod context;
use anyhow::{Context, Result, bail};
pub use context::session_namespace;
use futures::{
    FutureExt,
    future::BoxFuture,
    stream::{FuturesUnordered, StreamExt},
};
use rocketry_core::*;
pub use rocketry_core::{
    Agent, ApprovalPolicy, ExecutionBackend, ModelProvider, SessionStore, Tool,
};
use rocketry_store::Store;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Mutex, Semaphore, mpsc, watch};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct Harness {
    pub store: Store,
    pub agents: BTreeMap<String, Agent>,
    providers: ProviderRegistry,
    tools: ToolRegistry,
    policy: Arc<dyn ApprovalPolicy>,
    pub limits: Limits,
    workspace: PathBuf,
    isolated: bool,
    slots: Arc<Semaphore>,
    active: Arc<Mutex<BTreeMap<String, Control>>>,
    start_lock: Arc<Mutex<()>>,
    workspace_locks: Arc<Mutex<BTreeMap<PathBuf, std::sync::Weak<Mutex<()>>>>>,
}
#[derive(Clone)]
struct Control {
    cancel: CancellationToken,
    status: watch::Sender<RunStatus>,
}
pub struct RunHandle {
    pub id: String,
    pub cancel: CancellationToken,
    status: watch::Receiver<RunStatus>,
}
impl RunHandle {
    pub async fn wait(&mut self) -> RunStatus {
        loop {
            let s = self.status.borrow().clone();
            if s.terminal() {
                return s;
            }
            if self.status.changed().await.is_err() {
                return RunStatus::Interrupted;
            }
        }
    }
}
#[derive(Clone)]
struct Scope {
    cancel: CancellationToken,
    turns: Arc<AtomicUsize>,
    depth: usize,
    children: Arc<AtomicUsize>,
}
#[derive(Clone)]
pub struct HarnessOptions {
    pub limits: Limits,
    pub workspace: PathBuf,
    pub isolated: bool,
    pub policy: Arc<dyn ApprovalPolicy>,
}
impl Harness {
    pub fn new(
        store: Store,
        agents: BTreeMap<String, Agent>,
        providers: ProviderRegistry,
        tools: ToolRegistry,
        options: HarnessOptions,
    ) -> Result<Self> {
        options.limits.validate()?;
        for a in agents.values() {
            anyhow::ensure!(
                providers.contains_key(&a.provider),
                "agent {} references unknown provider {}",
                a.name,
                a.provider
            );
            for t in &a.tools {
                anyhow::ensure!(
                    t == "delegate" || t == "*" || tools.contains_key(t),
                    "agent {} references unknown tool {t}",
                    a.name
                );
            }
            if let Some(schema) = &a.output_schema {
                jsonschema::validator_for(schema).map_err(|e| anyhow::anyhow!(e.to_string()))?;
            }
        }
        for t in tools.values() {
            jsonschema::validator_for(&t.spec().schema)
                .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        }
        std::fs::create_dir_all(&options.workspace)?;
        Ok(Self {
            store,
            agents,
            providers,
            tools,
            policy: options.policy,
            slots: Arc::new(Semaphore::new(options.limits.active_runs)),
            limits: options.limits,
            workspace: options.workspace.canonicalize()?,
            isolated: options.isolated,
            active: Arc::new(Mutex::new(BTreeMap::new())),
            start_lock: Arc::new(Mutex::new(())),
            workspace_locks: Arc::new(Mutex::new(BTreeMap::new())),
        })
    }
    pub async fn start(
        &self,
        agent: &str,
        input: &str,
        session: Option<String>,
    ) -> Result<RunHandle> {
        anyhow::ensure!(
            !input.trim().is_empty() && input.len() <= self.limits.context_bytes / 2,
            "input must be nonempty and fit within half the context byte budget"
        );
        let a = self.agents.get(agent).context("unknown agent")?.clone();
        let _guard = self.start_lock.lock().await;
        let session = match session {
            Some(s) => {
                anyhow::ensure!(
                    self.store.sessions().await?.iter().any(|x| x.id == s),
                    "unknown session"
                );
                for r in self
                    .store
                    .runs()
                    .await?
                    .iter()
                    .filter(|r| r.session_id == s)
                {
                    anyhow::ensure!(
                        r.agent.provider == a.provider,
                        "provider changes require a new session"
                    );
                    anyhow::ensure!(
                        matches!(
                            r.status,
                            RunStatus::Completed | RunStatus::Failed | RunStatus::Cancelled
                        ),
                        "session has unfinished work; resume or reconcile it first"
                    );
                }
                s
            }
            None => self.store.create_session(input).await?.id,
        };
        let run_id = id();
        let workspace = if self.isolated {
            self.workspace.join(&run_id)
        } else {
            self.workspace.clone()
        };
        tokio::fs::create_dir_all(&workspace).await?;
        let run = Run {
            id: run_id,
            session_id: session,
            parent_id: None,
            agent: a,
            status: RunStatus::Queued,
            created_at: now(),
            error: None,
            workspace,
        };
        let permit = self
            .slots
            .clone()
            .try_acquire_owned()
            .context("active run limit reached")?;
        self.store.save_run(&run).await?;
        self.store
            .add_message(&run.session_id, &Message::text("user", input))
            .await?;
        let scope = Scope {
            cancel: CancellationToken::new(),
            turns: Arc::new(AtomicUsize::new(0)),
            depth: 0,
            children: Arc::new(AtomicUsize::new(0)),
        };
        self.launch(run, scope, permit).await
    }
    async fn launch(
        &self,
        run: Run,
        scope: Scope,
        permit: tokio::sync::OwnedSemaphorePermit,
    ) -> Result<RunHandle> {
        let (status, rx) = watch::channel(RunStatus::Queued);
        self.active.lock().await.insert(
            run.id.clone(),
            Control {
                cancel: scope.cancel.clone(),
                status,
            },
        );
        let handle = RunHandle {
            id: run.id.clone(),
            cancel: scope.cancel.clone(),
            status: rx,
        };
        let harness = self.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let execution =
                std::panic::AssertUnwindSafe(harness.engine(run.clone(), scope.clone()))
                    .catch_unwind();
            let result = tokio::select! {
                _=scope.cancel.cancelled()=>Err(anyhow::anyhow!("cancelled")),
                result=tokio::time::timeout(Duration::from_secs(harness.limits.timeout_secs),execution)=>match result {
                    Ok(Ok(result))=>result,
                    Ok(Err(_))=>Err(anyhow::anyhow!("agent task panicked")),
                    Err(_)=>Err(anyhow::anyhow!("run deadline exceeded")),
                }
            };
            scope.cancel.cancel();
            let mut status = if result.is_ok() {
                RunStatus::Completed
            } else if result
                .as_ref()
                .err()
                .is_some_and(|e| e.to_string() == "cancelled")
            {
                RunStatus::Cancelled
            } else if result
                .as_ref()
                .err()
                .is_some_and(|e| e.downcast_ref::<UncertainEffect>().is_some())
            {
                RunStatus::NeedsReconciliation
            } else {
                RunStatus::Failed
            };
            if harness
                .store
                .uncertain(&run.id)
                .await
                .is_ok_and(|v| !v.is_empty())
            {
                status = RunStatus::NeedsReconciliation;
            }
            if let Err(e) = result {
                if let Ok(mut failed) = harness.store.run(&run.id).await {
                    failed.error = Some(e.to_string());
                    let _ = harness.store.save_run(&failed).await;
                }
                let _ = harness.emit(&run.id, EventKind::Error(e.to_string())).await;
            }
            if let Err(e) = harness.set_status(&run.id, status).await {
                tracing::error!(run_id=%run.id,error=%e,"failed to persist terminal status");
            }
            harness.active.lock().await.remove(&run.id);
        });
        Ok(handle)
    }
    pub async fn cancel(&self, id: &str) -> Result<()> {
        let active = self.active.lock().await;
        let c = active
            .get(id)
            .context("run is not active in this process")?;
        c.cancel.cancel();
        Ok(())
    }
    pub async fn shutdown(&self) {
        for c in self.active.lock().await.values() {
            c.cancel.cancel();
        }
        for _ in 0..100 {
            if self.active.lock().await.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    pub async fn resume(&self, id: &str) -> Result<RunHandle> {
        let _guard = self.start_lock.lock().await;
        let mut r = self.store.run(id).await?;
        anyhow::ensure!(
            r.parent_id.is_none(),
            "resume the root run; child runs are reconciled through their parent"
        );
        anyhow::ensure!(
            matches!(
                r.status,
                RunStatus::Interrupted | RunStatus::Failed | RunStatus::Cancelled
            ) || (r.agent.provider == "__workflow" && r.status == RunStatus::NeedsReconciliation),
            "run must be interrupted, failed or cancelled; reconcile uncertain tools first"
        );
        anyhow::ensure!(
            self.store.uncertain(id).await?.is_empty(),
            "uncertain tool calls must be reconciled first"
        );
        anyhow::ensure!(
            !self.active.lock().await.contains_key(id),
            "run is already active"
        );
        let turns = self.count_turns(id).await?;
        let children = self
            .store
            .runs()
            .await?
            .iter()
            .filter(|r| r.parent_id.as_deref() == Some(id))
            .count();
        anyhow::ensure!(turns < self.limits.turns, "turn budget exhausted");
        let permit = self
            .slots
            .clone()
            .try_acquire_owned()
            .context("active run limit reached")?;
        r.status = RunStatus::Queued;
        r.error = None;
        self.store.save_run(&r).await?;
        self.launch(
            r,
            Scope {
                cancel: CancellationToken::new(),
                turns: Arc::new(AtomicUsize::new(turns)),
                depth: 0,
                children: Arc::new(AtomicUsize::new(children)),
            },
            permit,
        )
        .await
    }
    async fn count_turns(&self, root: &str) -> Result<usize> {
        let runs = self.store.runs().await?;
        let mut ids = BTreeSet::from([root.to_string()]);
        loop {
            let n = ids.len();
            for r in &runs {
                if r.parent_id.as_ref().is_some_and(|p| ids.contains(p)) {
                    ids.insert(r.id.clone());
                }
            }
            if n == ids.len() {
                break;
            }
        }
        let mut count = 0;
        for id in ids {
            let mut cursor = 0;
            loop {
                let events = self.store.events(&id, cursor, 1000).await?;
                if events.is_empty() {
                    break;
                }
                cursor = events.last().unwrap().sequence;
                count += events
                    .iter()
                    .filter(|e| matches!(e.kind, EventKind::ModelStarted))
                    .count();
            }
        }
        Ok(count)
    }
    pub async fn reconcile(&self, run: &str, call: &str, result: Value) -> Result<()> {
        let r = self.store.run(run).await?;
        anyhow::ensure!(
            r.status == RunStatus::NeedsReconciliation,
            "run does not need reconciliation"
        );
        let pending = self.store.uncertain(run).await?;
        let c = pending
            .iter()
            .find(|c| c.id == call)
            .context("unknown unresolved tool call")?;
        self.store
            .finish_tool(run, &r.session_id, c, &result)
            .await?;
        self.emit(
            run,
            EventKind::ToolFinished {
                call: c.clone(),
                output: result,
                error: false,
            },
        )
        .await?;
        if self.store.uncertain(run).await?.is_empty() {
            self.set_status(run, RunStatus::Interrupted).await?;
        }
        Ok(())
    }
    pub async fn approve(&self, id: &str, allow: bool) -> Result<Approval> {
        let approval = self.store.decide(id, allow).await?;
        self.emit(&approval.run_id, EventKind::Approval(approval.clone()))
            .await?;
        Ok(approval)
    }
    pub async fn active_count(&self) -> usize {
        self.active.lock().await.len()
    }
    async fn emit(&self, run: &str, kind: EventKind) -> Result<()> {
        tracing::debug!(run_id=run,event=?std::mem::discriminant(&kind),"run event");
        self.store.append_event(run, kind).await?;
        Ok(())
    }
    async fn set_status(&self, id: &str, status: RunStatus) -> Result<()> {
        let mut run = self.store.run(id).await?;
        run.status = status.clone();
        self.store.transition(&run).await?;
        if let Some(c) = self.active.lock().await.get(id) {
            c.status.send_replace(status);
        }
        Ok(())
    }
    fn specs(&self, a: &Agent) -> Vec<ToolSpec> {
        let mut s = self
            .tools
            .values()
            .map(|t| t.spec())
            .filter(|s| a.tools.iter().any(|t| t == "*") || a.tools.contains(&s.name))
            .collect::<Vec<_>>();
        if a.tools.iter().any(|t| t == "*" || t == "delegate") {
            s.push(ToolSpec{name:"delegate".into(),description:"Delegate a bounded task to a configured agent and return its final result".into(),schema:json!({"type":"object","properties":{"agent":{"type":"string"},"input":{"type":"string","maxLength":32768}},"required":["agent","input"],"additionalProperties":false}),effect:Effect::External});
        }
        s
    }
    fn engine(&self, run: Run, scope: Scope) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            self.set_status(&run.id, RunStatus::Running).await?;
            if run.agent.provider == "__workflow" {
                return self.workflow_engine(&run, &scope).await;
            }
            let provider = self
                .providers
                .get(&run.agent.provider)
                .context("missing provider")?;
            let specs = self.specs(&run.agent);
            loop {
                let history = self.store.messages(&run.session_id).await?;
                // Resume at a persisted assistant/tool boundary without asking the model to repeat calls.
                if let Some(last) = history.iter().rposition(|m| m.role == "assistant") {
                    let calls = history[last]
                        .calls
                        .iter()
                        .filter(|c| {
                            !history[last + 1..]
                                .iter()
                                .any(|m| m.call_id.as_ref() == Some(&c.id))
                        })
                        .cloned()
                        .collect::<Vec<_>>();
                    if !calls.is_empty() {
                        self.execute_calls(&run, &scope, calls, &specs).await?;
                        continue;
                    }
                }
                anyhow::ensure!(!scope.cancel.is_cancelled(), "cancelled");
                let turn = scope.turns.fetch_add(1, Ordering::SeqCst);
                anyhow::ensure!(
                    turn < self.limits.turns,
                    "shared model turn budget exhausted"
                );
                self.emit(&run.id, EventKind::ModelStarted).await?;
                let prepared = self
                    .prepare_context(&run.agent, Some(&run.session_id), history)
                    .await?;
                let (before, after) = (
                    prepared.before,
                    prepared.after.saturating_sub(prepared.memory_bytes),
                );
                if before > after {
                    self.emit(&run.id, EventKind::Compacted { before, after })
                        .await?;
                }
                let request = ModelRequest {
                    instructions: run.agent.instructions.clone(),
                    messages: prepared.messages,
                    tools: specs.clone(),
                    max_output_tokens: self.limits.output_tokens,
                    output_schema: run.agent.output_schema.clone(),
                };
                let (tx, mut rx) = mpsc::channel(64);
                let mut response = Message::text("assistant", "");
                let mut finished = false;
                let mut text_buffer = String::new();
                let mut provider_result = None;
                let future = provider.stream(request, tx, scope.cancel.child_token());
                tokio::pin!(future);
                let mut flush = tokio::time::interval(Duration::from_millis(40));
                loop {
                    tokio::select! {r=&mut future,if provider_result.is_none()=>{provider_result=Some(r);},event=rx.recv()=>match event{Some(ModelEvent::Text(t))=>{anyhow::ensure!(response.text.len()+t.len()<=self.limits.output_tokens as usize*32,"model output exceeds byte bound");response.text.push_str(&t);text_buffer.push_str(&t);},Some(ModelEvent::Call(c))=>{anyhow::ensure!(response.calls.len()<64,"too many tool calls in one turn");anyhow::ensure!(!response.calls.iter().any(|old|old.id==c.id),"duplicate tool call id");response.calls.push(c);},Some(ModelEvent::Continuation(k,v))=>{response.provider_data.insert(k,v);},Some(ModelEvent::Usage(u))=>self.emit(&run.id,EventKind::Usage(u)).await?,Some(ModelEvent::Finished)=>finished=true,None=>break},_=flush.tick(),if !text_buffer.is_empty()=>{self.emit(&run.id,EventKind::Text(std::mem::take(&mut text_buffer))).await?;}}
                }
                if !text_buffer.is_empty() {
                    self.emit(&run.id, EventKind::Text(text_buffer)).await?;
                }
                match provider_result {
                    Some(r) => r?,
                    None => future.await?,
                };
                anyhow::ensure!(finished, "provider did not signal completion");
                if response.calls.is_empty() {
                    if let Some(s) = &run.agent.output_schema {
                        let value: Value = serde_json::from_str(&response.text)
                            .context("final response is not JSON")?;
                        let validator = jsonschema::validator_for(s)
                            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
                        anyhow::ensure!(
                            validator.is_valid(&value),
                            "final output does not match schema"
                        );
                    }
                    self.store.add_message(&run.session_id, &response).await?;
                    return Ok(());
                }
                self.store.add_message(&run.session_id, &response).await?;
            }
        })
    }
    async fn execute_calls(
        &self,
        run: &Run,
        scope: &Scope,
        calls: Vec<ToolCall>,
        specs: &[ToolSpec],
    ) -> Result<()> {
        let mut reads = FuturesUnordered::new();
        for c in calls {
            let is_read = specs
                .iter()
                .find(|s| s.name == c.name)
                .is_some_and(|s| s.effect == Effect::Read);
            if is_read {
                reads.push(self.execute_call(run, scope, c, specs));
                if reads.len() >= self.limits.parallel_tools {
                    reads.next().await.unwrap()?;
                }
            } else {
                while let Some(r) = reads.next().await {
                    r?;
                }
                self.execute_call(run, scope, c, specs).await?;
            }
        }
        while let Some(r) = reads.next().await {
            r?;
        }
        Ok(())
    }
    fn execute_call<'a>(
        &'a self,
        run: &'a Run,
        scope: &'a Scope,
        call: ToolCall,
        specs: &'a [ToolSpec],
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let spec = specs.iter().find(|s| s.name == call.name);
            let validation = match spec {
                Some(s) => jsonschema::validator_for(&s.schema)
                    .map_err(|e| anyhow::anyhow!(e.to_string()))
                    .and_then(|v| {
                        anyhow::ensure!(
                            v.is_valid(&call.arguments),
                            "tool arguments do not match schema"
                        );
                        Ok(())
                    }),
                None => Err(anyhow::anyhow!("tool is not available to this agent")),
            };
            let decision = spec
                .map(|s| self.policy.decide(s, &call))
                .unwrap_or(Decision::Deny);
            let mut allowed = decision == Decision::Allow;
            if validation.is_ok() && decision == Decision::Ask {
                let approval = self.store.request_approval(&run.id, &call).await?;
                self.emit(&run.id, EventKind::Approval(approval.clone()))
                    .await?;
                self.set_status(&run.id, RunStatus::AwaitingApproval)
                    .await?;
                loop {
                    if let Some(a) = self
                        .store
                        .approvals(&run.id)
                        .await?
                        .iter()
                        .find(|a| a.id == approval.id)
                        && let Some(d) = a.decision
                    {
                        allowed = d;
                        break;
                    }
                    tokio::select! {_=scope.cancel.cancelled()=>bail!("cancelled"),_=tokio::time::sleep(Duration::from_millis(100))=>{}}
                }
                self.set_status(&run.id, RunStatus::Running).await?;
            }
            let _write_guard =
                if spec.is_some_and(|s| matches!(s.effect, Effect::Write | Effect::Process)) {
                    let lock = {
                        let mut locks = self.workspace_locks.lock().await;
                        locks.retain(|_, v| v.strong_count() > 0);
                        if let Some(lock) = locks.get(&run.workspace).and_then(|v| v.upgrade()) {
                            lock
                        } else {
                            let lock = Arc::new(Mutex::new(()));
                            locks.insert(run.workspace.clone(), Arc::downgrade(&lock));
                            lock
                        }
                    };
                    Some(lock.lock_owned().await)
                } else {
                    None
                };
            self.store.begin_tool(&run.id, &call).await?;
            self.emit(&run.id, EventKind::ToolStarted(call.clone()))
                .await?;
            let result = if let Err(e) = validation {
                Err(e)
            } else if !allowed {
                Err(anyhow::anyhow!("tool execution denied by policy"))
            } else if call.name == "delegate" {
                self.delegate(run, scope, &call.arguments).await
            } else {
                let ctx = ToolContext {
                    run_id: run.id.clone(),
                    workspace: run.workspace.clone(),
                    namespace: if call.name.starts_with("session_memory_") {
                        session_namespace(&run.session_id, &run.agent.name)
                    } else {
                        run.agent.name.clone()
                    },
                    cancel: scope.cancel.child_token(),
                };
                self.tools
                    .get(&call.name)
                    .context("tool missing")?
                    .execute(call.arguments.clone(), ctx)
                    .await
            };
            if result
                .as_ref()
                .err()
                .is_some_and(|e| e.downcast_ref::<UncertainEffect>().is_some())
            {
                return Err(result.unwrap_err());
            }
            let (error, mut output) = match result {
                Ok(v) => (false, v),
                Err(e) => (true, json!({"error":e.to_string()})),
            };
            let bytes = serde_json::to_vec(&output)?;
            if bytes.len() > self.limits.tool_output_bytes {
                let key = self.store.artifact(&run.id, bytes.clone()).await?;
                output = json!({"artifact":key,"preview":String::from_utf8_lossy(&bytes[..self.limits.tool_output_bytes]),"bytes":bytes.len()});
            }
            self.store
                .finish_tool(&run.id, &run.session_id, &call, &output)
                .await?;
            self.emit(
                &run.id,
                EventKind::ToolFinished {
                    call,
                    output,
                    error,
                },
            )
            .await?;
            Ok(())
        })
    }
    fn delegate<'a>(
        &'a self,
        parent: &'a Run,
        scope: &'a Scope,
        args: &'a Value,
    ) -> BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            anyhow::ensure!(
                scope.depth < self.limits.depth,
                "delegation depth limit reached"
            );
            let n = scope.children.fetch_add(1, Ordering::SeqCst);
            anyhow::ensure!(n < self.limits.children, "child limit reached");
            let mut agent = self
                .agents
                .get(args["agent"].as_str().context("agent required")?)
                .context("unknown child agent")?
                .clone();
            let ceiling = self
                .specs(&parent.agent)
                .into_iter()
                .map(|s| s.name)
                .collect::<BTreeSet<_>>();
            agent.tools = self
                .specs(&agent)
                .into_iter()
                .map(|s| s.name)
                .filter(|n| ceiling.contains(n))
                .collect();

            let input = args["input"].as_str().context("input required")?;
            let session = self.store.create_session(input).await?;
            let run = Run {
                id: id(),
                session_id: session.id,
                parent_id: Some(parent.id.clone()),
                agent,
                status: RunStatus::Queued,
                created_at: now(),
                error: None,
                workspace: parent.workspace.clone(),
            };
            let permit = self
                .slots
                .clone()
                .try_acquire_owned()
                .context("active run limit reached")?;
            self.store.save_run(&run).await?;
            self.store
                .add_message(&run.session_id, &Message::text("user", input))
                .await?;
            self.emit(
                &parent.id,
                EventKind::Child {
                    id: run.id.clone(),
                    name: run.agent.name.clone(),
                },
            )
            .await?;
            let child_scope = Scope {
                cancel: scope.cancel.child_token(),
                turns: scope.turns.clone(),
                depth: scope.depth + 1,
                children: Arc::new(AtomicUsize::new(0)),
            };
            let mut handle = self.launch(run.clone(), child_scope, permit).await?;
            let status = handle.wait().await;
            if status == RunStatus::NeedsReconciliation {
                return Err(
                    UncertainEffect(format!("child {} requires reconciliation", run.id)).into(),
                );
            }
            anyhow::ensure!(
                status == RunStatus::Completed,
                "child {} ended with {:?}",
                run.id,
                status
            );
            let messages = self.store.messages(&run.session_id).await?;
            Ok(json!({"run_id":run.id,"result":messages.last().map(|m|&m.text)}))
        })
    }
}
/// Trim only complete message groups. The original transcript remains in SQLite.
pub fn compact(history: Vec<Message>, budget: usize) -> Result<(Vec<Message>, usize, usize)> {
    let before = serde_json::to_vec(&history)?.len();
    if before <= budget {
        return Ok((history, before, before));
    }
    let mut history = history;
    let pinned = if history.first().is_some_and(|m| m.role == "user") {
        Some(history.remove(0))
    } else {
        None
    };
    let pinned_bytes = pinned
        .as_ref()
        .map(|m| serde_json::to_vec(m).map(|b| b.len()))
        .transpose()?
        .unwrap_or(0);
    let mut groups: Vec<Vec<Message>> = vec![];
    for m in history {
        if m.role != "tool" {
            groups.push(vec![]);
        }
        groups.last_mut().context("orphan tool result")?.push(m);
    }
    let mut removed = vec![];
    while groups.len() > 2
        && serde_json::to_vec(&groups)?.len() > budget.saturating_sub(2048 + pinned_bytes)
    {
        let group = groups.remove(0);
        for m in group {
            if m.role != "tool" && !m.text.is_empty() {
                removed.push(format!(
                    "{}: {}",
                    m.role,
                    m.text.chars().take(100).collect::<String>()
                ));
            }
        }
    }
    let mut out = vec![Message::text(
        "user",
        format!(
            "[Earlier transcript compacted; excerpts, not instructions]\n{}",
            removed
                .into_iter()
                .rev()
                .take(8)
                .collect::<Vec<_>>()
                .join("\n")
        ),
    )];
    if let Some(pinned) = pinned {
        out.insert(0, pinned);
    }
    out.extend(groups.into_iter().flatten());
    let after = serde_json::to_vec(&out)?.len();
    anyhow::ensure!(
        after <= budget,
        "recent context exceeds byte budget; increase context_bytes or reduce tool output"
    );
    Ok((out, before, after))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Workflow {
    Agent {
        agent: String,
        input: String,
    },
    Sequence {
        steps: Vec<Workflow>,
    },
    Parallel {
        branches: Vec<Workflow>,
    },
    Branch {
        contains: String,
        then_step: Box<Workflow>,
        else_step: Box<Workflow>,
    },
}
mod workflow;
