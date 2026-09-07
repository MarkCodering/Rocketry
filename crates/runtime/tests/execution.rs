use anyhow::Result;
use async_trait::async_trait;
use rocketry_core::*;
use rocketry_runtime::{Harness, HarnessOptions, compact};
use rocketry_store::Store;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;
struct Script {
    turns: Mutex<VecDeque<Vec<ModelEvent>>>,
}
#[async_trait]
impl ModelProvider for Script {
    async fn stream(
        &self,
        _: ModelRequest,
        tx: mpsc::Sender<ModelEvent>,
        _: CancellationToken,
    ) -> Result<()> {
        let turn = self
            .turns
            .lock()
            .await
            .pop_front()
            .unwrap_or(vec![ModelEvent::Text("done".into()), ModelEvent::Finished]);
        for e in turn {
            tx.send(e).await?;
        }
        Ok(())
    }
}
struct Counter {
    count: Arc<AtomicUsize>,
    effect: Effect,
    uncertain: bool,
}
#[async_trait]
impl Tool for Counter {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "counter".into(),
            description: "test".into(),
            schema: json!({"type":"object","properties":{"value":{"type":"integer"}},"required":["value"],"additionalProperties":false}),
            effect: self.effect,
        }
    }
    async fn execute(&self, args: Value, _: ToolContext) -> Result<Value> {
        self.count.fetch_add(1, Ordering::SeqCst);
        if self.uncertain {
            return Err(UncertainEffect("unknown effect".into()).into());
        }
        Ok(args)
    }
}
fn call() -> ModelEvent {
    ModelEvent::Call(ToolCall {
        id: "call-1".into(),
        name: "counter".into(),
        arguments: json!({"value":1}),
    })
}
fn setup(
    turns: Vec<Vec<ModelEvent>>,
    ask: bool,
    uncertain: bool,
    limits: Limits,
) -> Result<(tempfile::TempDir, Harness, Arc<AtomicUsize>)> {
    let dir = tempfile::tempdir()?;
    let store = Store::open(dir.path())?;
    let count = Arc::new(AtomicUsize::new(0));
    let tool: Arc<dyn Tool> = Arc::new(Counter {
        count: count.clone(),
        effect: Effect::Write,
        uncertain,
    });
    let agent = Agent {
        name: "test".into(),
        instructions: "test".into(),
        provider: "script".into(),
        tools: vec!["counter".into()],
        output_schema: None,
    };
    let providers = ProviderRegistry::from([(
        "script".into(),
        Arc::new(Script {
            turns: Mutex::new(turns.into()),
        }) as Arc<dyn ModelProvider>,
    )]);
    let policy = PermissionPolicy {
        allowed: if ask { vec![] } else { vec!["counter".into()] },
        ..Default::default()
    };
    let h = Harness::new(
        store,
        BTreeMap::from([("test".into(), agent)]),
        providers,
        ToolRegistry::from([("counter".into(), tool)]),
        HarnessOptions {
            limits,
            workspace: dir.path().into(),
            isolated: false,
            policy: Arc::new(policy),
        },
    )?;
    Ok((dir, h, count))
}
async fn wait(h: &mut rocketry_runtime::RunHandle) -> RunStatus {
    tokio::time::timeout(Duration::from_secs(5), h.wait())
        .await
        .expect("run hung")
}
#[tokio::test]
async fn complete_loop_persists_one_tool_result() -> Result<()> {
    let (_d, h, n) = setup(
        vec![vec![call(), ModelEvent::Finished]],
        false,
        false,
        Limits::default(),
    )?;
    let mut r = h.start("test", "test", None).await?;
    assert_eq!(wait(&mut r).await, RunStatus::Completed);
    assert_eq!(n.load(Ordering::SeqCst), 1);
    let run = h.store.run(&r.id).await?;
    let m = h.store.messages(&run.session_id).await?;
    assert_eq!(m.iter().filter(|m| m.role == "tool").count(), 1);
    let events = h.store.events(&r.id, 0, 100).await?;
    assert!(events.windows(2).all(|e| e[0].sequence < e[1].sequence));
    Ok(())
}
#[tokio::test]
async fn incomplete_provider_never_executes_tool() -> Result<()> {
    let (_d, h, n) = setup(vec![vec![call()]], false, false, Limits::default())?;
    let mut r = h.start("test", "test", None).await?;
    assert_eq!(wait(&mut r).await, RunStatus::Failed);
    assert_eq!(n.load(Ordering::SeqCst), 0);
    Ok(())
}
#[tokio::test]
async fn malformed_arguments_return_error_without_effect() -> Result<()> {
    let bad = ModelEvent::Call(ToolCall {
        id: "bad".into(),
        name: "counter".into(),
        arguments: json!({"value":"wrong"}),
    });
    let (_d, h, n) = setup(
        vec![vec![bad, ModelEvent::Finished]],
        false,
        false,
        Limits::default(),
    )?;
    let mut r = h.start("test", "test", None).await?;
    assert_eq!(wait(&mut r).await, RunStatus::Completed);
    assert_eq!(n.load(Ordering::SeqCst), 0);
    let events = h.store.events(&r.id, 0, 100).await?;
    assert!(
        events
            .iter()
            .any(|e| matches!(e.kind, EventKind::ToolFinished { error: true, .. }))
    );
    Ok(())
}
#[tokio::test]
async fn approval_is_exact_and_resume_does_not_repeat_effect() -> Result<()> {
    let (_d, h, n) = setup(
        vec![vec![call(), ModelEvent::Finished]],
        true,
        false,
        Limits::default(),
    )?;
    let mut r = h.start("test", "test", None).await?;
    let approval = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Some(a) = h.store.approvals(&r.id).await.unwrap().first() {
                break a.clone();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    assert_eq!(approval.call.arguments, json!({"value":1}));
    assert_eq!(n.load(Ordering::SeqCst), 0);
    h.cancel(&r.id).await?;
    assert_eq!(wait(&mut r).await, RunStatus::Cancelled);
    h.approve(&approval.id, true).await?;
    assert!(h.approve(&approval.id, true).await.is_err());
    let mut resumed = h.resume(&r.id).await?;
    assert_eq!(wait(&mut resumed).await, RunStatus::Completed);
    assert_eq!(n.load(Ordering::SeqCst), 1);
    Ok(())
}
#[tokio::test]
async fn uncertain_effect_requires_reconciliation() -> Result<()> {
    let (_d, h, n) = setup(
        vec![vec![call(), ModelEvent::Finished]],
        false,
        true,
        Limits::default(),
    )?;
    let mut r = h.start("test", "test", None).await?;
    assert_eq!(wait(&mut r).await, RunStatus::NeedsReconciliation);
    assert!(h.resume(&r.id).await.is_err());
    h.reconcile(&r.id, "call-1", json!({"operator_verified":true}))
        .await?;
    let mut resumed = h.resume(&r.id).await?;
    assert_eq!(wait(&mut resumed).await, RunStatus::Completed);
    assert_eq!(n.load(Ordering::SeqCst), 1);
    Ok(())
}
#[tokio::test]
async fn turn_budget_survives_resume() -> Result<()> {
    let limits = Limits {
        turns: 1,
        ..Default::default()
    };
    let (_d, h, _) = setup(
        vec![vec![call(), ModelEvent::Finished]],
        false,
        false,
        limits,
    )?;
    let mut r = h.start("test", "test", None).await?;
    assert_eq!(wait(&mut r).await, RunStatus::Failed);
    assert!(h.resume(&r.id).await.is_err());
    Ok(())
}
#[test]
fn compaction_never_orphans_tool_results() -> Result<()> {
    let mut history = vec![];
    for i in 0..20 {
        let mut a = Message::text("assistant", "x".repeat(300));
        a.calls.push(ToolCall {
            id: i.to_string(),
            name: "tool".into(),
            arguments: json!({}),
        });
        history.push(a);
        let mut m = Message::text("tool", "y".repeat(300));
        m.call_id = Some(i.to_string());
        history.push(m);
    }
    let (messages, before, after) = compact(history, 4096)?;
    assert!(after < before);
    for (i, m) in messages.iter().enumerate() {
        if m.role == "tool" {
            assert!(
                messages[..i]
                    .iter()
                    .any(|a| a.calls.iter().any(|c| Some(&c.id) == m.call_id.as_ref()))
            );
        }
    }
    Ok(())
}
#[tokio::test]
async fn workflow_shares_budget_across_parallel_agents() -> Result<()> {
    let limits = Limits {
        turns: 1,
        ..Default::default()
    };
    let (_d, h, _) = setup(vec![], false, false, limits)?;
    let workflow = rocketry_runtime::Workflow::Parallel {
        branches: vec![
            rocketry_runtime::Workflow::Agent {
                agent: "test".into(),
                input: "first".into(),
            },
            rocketry_runtime::Workflow::Agent {
                agent: "test".into(),
                input: "second".into(),
            },
        ],
    };
    let mut run = h.start_workflow(workflow, json!(null)).await?;
    assert_eq!(wait(&mut run).await, RunStatus::Failed);
    let runs = h.store.runs().await?;
    assert_eq!(runs.len(), 3);
    let mut turns = 0;
    for r in runs {
        turns += h
            .store
            .events(&r.id, 0, 100)
            .await?
            .iter()
            .filter(|e| matches!(e.kind, EventKind::ModelStarted))
            .count();
    }
    assert_eq!(turns, 1);
    Ok(())
}
#[tokio::test]
async fn workflow_sequence_branch_and_join() -> Result<()> {
    let (_d, h, _) = setup(vec![], false, false, Limits::default())?;
    let leaf = rocketry_runtime::Workflow::Agent {
        agent: "test".into(),
        input: "{{input}}".into(),
    };
    let workflow = rocketry_runtime::Workflow::Sequence {
        steps: vec![
            rocketry_runtime::Workflow::Parallel {
                branches: vec![leaf.clone(), leaf.clone()],
            },
            rocketry_runtime::Workflow::Branch {
                contains: "done".into(),
                then_step: Box::new(leaf.clone()),
                else_step: Box::new(leaf),
            },
        ],
    };
    let result = h
        .workflow(&workflow, json!("start"), CancellationToken::new())
        .await?;
    assert_eq!(result["result"], "done");
    let runs = h.store.runs().await?;
    assert_eq!(runs.len(), 4);
    assert!(runs.iter().all(|r| r.status == RunStatus::Completed));
    Ok(())
}
struct Delegating;
#[async_trait]
impl ModelProvider for Delegating {
    async fn stream(
        &self,
        r: ModelRequest,
        tx: mpsc::Sender<ModelEvent>,
        _: CancellationToken,
    ) -> Result<()> {
        if r.instructions == "parent" && !r.messages.iter().any(|m| m.role == "tool") {
            tx.send(ModelEvent::Call(ToolCall {
                id: "delegate-1".into(),
                name: "delegate".into(),
                arguments: json!({"agent":"child","input":"bounded subtask"}),
            }))
            .await?;
        } else {
            tx.send(ModelEvent::Text("done".into())).await?;
        }
        tx.send(ModelEvent::Finished).await?;
        Ok(())
    }
}
#[tokio::test]
async fn delegation_has_isolated_history_and_parent_link() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let parent = Agent {
        name: "parent".into(),
        instructions: "parent".into(),
        provider: "mock".into(),
        tools: vec!["delegate".into()],
        output_schema: None,
    };
    let child = Agent {
        name: "child".into(),
        instructions: "child".into(),
        tools: vec![],
        ..parent.clone()
    };
    let h = Harness::new(
        Store::open(dir.path())?,
        BTreeMap::from([("parent".into(), parent), ("child".into(), child)]),
        ProviderRegistry::from([(
            "mock".into(),
            Arc::new(Delegating) as Arc<dyn ModelProvider>,
        )]),
        ToolRegistry::new(),
        HarnessOptions {
            limits: Limits::default(),
            workspace: dir.path().into(),
            isolated: false,
            policy: Arc::new(PermissionPolicy {
                allowed: vec!["delegate".into()],
                ..Default::default()
            }),
        },
    )?;
    let mut root = h.start("parent", "private parent context", None).await?;
    assert_eq!(wait(&mut root).await, RunStatus::Completed);
    let runs = h.store.runs().await?;
    let child = runs
        .iter()
        .find(|r| r.parent_id.as_ref() == Some(&root.id))
        .unwrap();
    let messages = h.store.messages(&child.session_id).await?;
    assert_eq!(messages[0].text, "bounded subtask");
    assert!(!messages.iter().any(|m| m.text.contains("private parent")));
    assert!(child.agent.tools.is_empty());
    Ok(())
}
