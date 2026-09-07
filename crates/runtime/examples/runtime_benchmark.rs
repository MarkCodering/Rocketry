//! Reproducible harness overhead measurement. No provider network or Docker startup.
use anyhow::Result;
use async_trait::async_trait;
use rocketry_core::*;
use rocketry_runtime::{Harness, HarnessOptions};
use rocketry_store::Store;
use serde_json::json;
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::{Semaphore, mpsc};
use tokio_util::sync::CancellationToken;
struct Mock {
    gate: Arc<Semaphore>,
    started: Arc<AtomicUsize>,
}
#[async_trait]
impl ModelProvider for Mock {
    async fn stream(
        &self,
        _: ModelRequest,
        events: mpsc::Sender<ModelEvent>,
        _: CancellationToken,
    ) -> Result<()> {
        self.started.fetch_add(1, Ordering::SeqCst);
        let _permit = self.gate.acquire().await?;
        events.send(ModelEvent::Text("done".into())).await?;
        events.send(ModelEvent::Finished).await?;
        Ok(())
    }
}
fn rss() -> u64 {
    let o = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&o.stdout)
        .trim()
        .parse::<u64>()
        .unwrap()
        * 1024
}
fn p95(v: &mut [f64]) -> f64 {
    v.sort_by(f64::total_cmp);
    v[(v.len() * 95 / 100).min(v.len() - 1)]
}
#[tokio::main]
async fn main() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open(dir.path())?;
    let gate = Arc::new(Semaphore::new(0));
    let started = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(Mock {
        gate: gate.clone(),
        started: started.clone(),
    });
    let agent = Agent {
        name: "bench".into(),
        instructions: String::new(),
        provider: "mock".into(),
        tools: vec![],
        output_schema: None,
    };
    let h = Harness::new(
        store,
        BTreeMap::from([("bench".into(), agent)]),
        ProviderRegistry::from([("mock".into(), provider as Arc<dyn ModelProvider>)]),
        ToolRegistry::new(),
        HarnessOptions {
            limits: Limits::default(),
            workspace: dir.path().into(),
            isolated: false,
            policy: Arc::new(PermissionPolicy::default()),
        },
    )?;
    let baseline = rss();
    let mut handles = vec![];
    let mut scheduling = vec![];
    let input = "x".repeat(32768);
    let start = Instant::now();
    for _ in 0..100 {
        let t = Instant::now();
        handles.push(h.start("bench", &input, None).await?);
        scheduling.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    tokio::time::timeout(Duration::from_secs(30), async {
        while started.load(Ordering::SeqCst) < 100 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    let delta = rss().saturating_sub(baseline);
    gate.add_permits(100);
    for mut h in handles {
        assert_eq!(h.wait().await, RunStatus::Completed);
    }
    let total = start.elapsed().as_secs_f64();
    let (tx, mut rx) = mpsc::channel(64);
    let mut dispatch = vec![];
    for _ in 0..10000 {
        let t = Instant::now();
        tx.send(ModelEvent::Finished).await?;
        rx.recv().await;
        dispatch.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    println!(
        "{}",
        serde_json::to_string_pretty(
            &json!({"concurrent_runs":100,"context_bytes":32768,"durable_start_p95_ms":p95(&mut scheduling),"in_memory_event_dispatch_p95_ms":p95(&mut dispatch),"incremental_rss_bytes":delta,"total_seconds":total,"runs_per_second":100.0/total,"scope":"local mock provider; excludes network and containers","os":std::env::consts::OS,"arch":std::env::consts::ARCH})
        )?
    );
    Ok(())
}
