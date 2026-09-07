//! `cargo run -p rocketry-runtime --example sdk`
use anyhow::Result;
use rocketry_core::*;
use rocketry_providers::DemoProvider;
use rocketry_runtime::{Harness, HarnessOptions};
use rocketry_store::Store;
use rocketry_tools::{HostBackend, builtins};
use std::{collections::BTreeMap, sync::Arc};
#[tokio::main]
async fn main() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open(dir.path())?;
    let agent = Agent {
        name: "navigator".into(),
        instructions: "Inspect the workspace".into(),
        provider: "demo".into(),
        tools: vec!["list_dir".into()],
        output_schema: None,
    };
    let tools = builtins(Arc::new(HostBackend), store.clone()); // Explicitly trusted host access.
    let harness = Harness::new(
        store,
        BTreeMap::from([("navigator".into(), agent)]),
        ProviderRegistry::from([(
            "demo".into(),
            Arc::new(DemoProvider) as Arc<dyn ModelProvider>,
        )]),
        tools,
        HarnessOptions {
            limits: Limits::default(),
            workspace: std::env::current_dir()?,
            isolated: false,
            policy: Arc::new(PermissionPolicy {
                allow_reads: true,
                ..Default::default()
            }),
        },
    )?;
    let mut run = harness
        .start("navigator", "Inspect this workspace", None)
        .await?;
    let status = run.wait().await;
    println!("{}: {:?}", run.id, status);
    Ok(())
}
