use anyhow::Result;
use async_trait::async_trait;
use rocketry_core::*;
use rocketry_runtime::{Harness, HarnessOptions};
use rocketry_store::Store;
use rocketry_tools::{DisabledBackend, builtins};
use serde_json::json;
use std::{collections::BTreeMap, sync::Arc};
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;

struct Recorder {
    requests: Arc<Mutex<Vec<ModelRequest>>>,
    write_first: bool,
}
#[async_trait]
impl ModelProvider for Recorder {
    async fn stream(
        &self,
        request: ModelRequest,
        tx: mpsc::Sender<ModelEvent>,
        _: CancellationToken,
    ) -> Result<()> {
        let mut requests = self.requests.lock().await;
        let first = requests.is_empty();
        requests.push(request);
        if first && self.write_first {
            for (id, name, value) in [
                ("long", "memory_put", "durable preference: concise"),
                ("short", "session_memory_put", "working note: inspect cache"),
            ] {
                tx.send(ModelEvent::Call(ToolCall {
                    id: id.into(),
                    name: name.into(),
                    arguments: json!({"key":id,"value":value}),
                }))
                .await?;
            }
        } else {
            tx.send(ModelEvent::Text("done".into())).await?;
        }
        tx.send(ModelEvent::Finished).await?;
        Ok(())
    }
}
fn harness(
    path: &std::path::Path,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
    write_first: bool,
    deny_reads: bool,
) -> Result<Harness> {
    let store = Store::open(path)?;
    let agent = Agent {
        name: "navigator".into(),
        provider: "record".into(),
        instructions: "test".into(),
        tools: vec![
            "memory_put".into(),
            "memory_search".into(),
            "session_memory_put".into(),
            "session_memory_search".into(),
        ],
        output_schema: None,
    };
    Harness::new(
        store.clone(),
        BTreeMap::from([
            ("navigator".into(), agent.clone()),
            (
                "other".into(),
                Agent {
                    name: "other".into(),
                    ..agent
                },
            ),
        ]),
        BTreeMap::from([(
            "record".into(),
            Arc::new(Recorder {
                requests,
                write_first,
            }) as Arc<dyn ModelProvider>,
        )]),
        builtins(Arc::new(DisabledBackend), store),
        HarnessOptions {
            workspace: path.into(),
            isolated: false,
            limits: Limits::default(),
            policy: Arc::new(PermissionPolicy {
                allow_reads: !deny_reads,
                allowed: vec!["memory_put".into(), "session_memory_put".into()],
                ..Default::default()
            }),
        },
    )
}
#[tokio::test]
async fn memory_tools_recall_across_restart_and_isolate_session_and_agent() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let requests = Arc::new(Mutex::new(vec![]));
    let h = harness(dir.path(), requests.clone(), true, false)?;
    let mut run = h.start("navigator", "remember these notes", None).await?;
    assert_eq!(run.wait().await, RunStatus::Completed);
    let session = h.store.run(&run.id).await?.session_id;
    let recorded = requests.lock().await;
    let second = serde_json::to_string(&recorded[1].messages)?;
    assert!(second.contains("durable preference"));
    assert!(second.contains("working note"));
    drop(recorded);
    h.shutdown().await;
    drop(h);
    let next = Arc::new(Mutex::new(vec![]));
    let h = harness(dir.path(), next.clone(), false, false)?;
    let mut run = h.start("navigator", "new session", None).await?;
    assert_eq!(run.wait().await, RunStatus::Completed);
    let content = serde_json::to_string(&next.lock().await[0].messages)?;
    assert!(content.contains("durable preference"));
    assert!(!content.contains("working note"));
    let mut run = h.start("navigator", "continue", Some(session)).await?;
    assert_eq!(run.wait().await, RunStatus::Completed);
    assert!(serde_json::to_string(&next.lock().await[1].messages)?.contains("working note"));
    let mut run = h.start("other", "isolated agent", None).await?;
    assert_eq!(run.wait().await, RunStatus::Completed);
    assert!(!serde_json::to_string(&next.lock().await[2].messages)?.contains("durable preference"));
    h.shutdown().await;
    Ok(())
}
#[tokio::test]
async fn memory_recall_obeys_policy_and_full_request_budget() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let requests = Arc::new(Mutex::new(vec![]));
    let mut h = harness(dir.path(), requests.clone(), false, true)?;
    h.store
        .memory_put("navigator", "private", "must not be recalled")
        .await?;
    let mut run = h.start("navigator", "test", None).await?;
    assert_eq!(run.wait().await, RunStatus::Completed);
    assert!(
        !serde_json::to_string(&requests.lock().await[0].messages)?
            .contains("must not be recalled")
    );
    h.limits.context_bytes = 4096;
    h.agents.get_mut("navigator").unwrap().instructions = "x".repeat(4096);
    let report = h.inspect_context("navigator", None).await?;
    assert!(
        report["context_error"]
            .as_str()
            .unwrap()
            .contains("exhaust")
    );
    h.shutdown().await;
    Ok(())
}
#[tokio::test]
async fn large_memory_is_bounded_and_forget_removes_search_index() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let requests = Arc::new(Mutex::new(vec![]));
    let h = harness(dir.path(), requests.clone(), false, false)?;
    for n in 0..30 {
        h.store
            .memory_put(
                "navigator",
                &format!("entry-{n}"),
                &"火箭 launch ".repeat(500),
            )
            .await?;
    }
    let report = h.inspect_context("navigator", None).await?;
    assert!(report["context"]["memory_bytes"].as_u64().unwrap() <= 8192);
    assert!(
        report["context"]["total_bytes"].as_u64().unwrap()
            <= report["limit_bytes"].as_u64().unwrap()
    );
    h.store
        .memory_put("navigator", "unique", "sentineluniquefact")
        .await?;
    assert_eq!(
        h.store
            .memory_search("navigator", "sentineluniquefact")
            .await?
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(h.store.memory_delete("navigator", "unique").await?);
    assert!(
        h.store
            .memory_search("navigator", "sentineluniquefact")
            .await?
            .as_array()
            .unwrap()
            .is_empty()
    );
    h.shutdown().await;
    Ok(())
}
