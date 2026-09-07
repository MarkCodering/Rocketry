use anyhow::Result;
use rocketry_core::*;
use rocketry_tools::{
    DockerBackend,
    mcp::{McpConfig, discover},
};
use serde_json::json;
use tokio_util::sync::CancellationToken;
fn context(path: &std::path::Path) -> ToolContext {
    ToolContext {
        run_id: id(),
        workspace: path.into(),
        namespace: "test".into(),
        cancel: CancellationToken::new(),
    }
}
#[tokio::test]
async fn stdio_discovery_and_call() -> Result<()> {
    let registry = discover(McpConfig {
        name: "fixture".into(),
        command: Some("python3".into()),
        args: vec![format!("{}/tests/mcp_stdio.py", env!("CARGO_MANIFEST_DIR"))],
        url: None,
        token_env: None,
        env: Default::default(),
    })
    .await?;
    let tool = &registry["mcp_fixture_echo"];
    assert_eq!(tool.spec().effect, Effect::External);
    let dir = tempfile::tempdir()?;
    let result = tool
        .execute(json!({"text":"hello"}), context(dir.path()))
        .await?;
    assert_eq!(result["content"][0]["text"], "hello");
    Ok(())
}
#[tokio::test]
#[ignore = "requires a running Docker daemon and python:3.13-slim image"]
async fn docker_workspace_and_isolation() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let backend = DockerBackend {
        image: std::env::var("ROCKETRY_DOCKER_IMAGE").unwrap_or("python:3.13-slim".into()),
    };
    backend
        .execute(
            "write_file",
            json!({"path":"hello.txt","content":"inside container"}),
            context(dir.path()),
        )
        .await?;
    assert_eq!(
        std::fs::read_to_string(dir.path().join("hello.txt"))?,
        "inside container"
    );
    let result = backend
        .execute(
            "read_file",
            json!({"path":"hello.txt"}),
            context(dir.path()),
        )
        .await?;
    assert_eq!(result["text"], "inside container");
    assert!(
        backend
            .execute(
                "read_file",
                json!({"path":"../etc/passwd"}),
                context(dir.path())
            )
            .await
            .is_err()
    );
    let result=backend.execute("execute",json!({"command":"python3 -c 'import os,socket; assert not os.path.exists(\"/var/run/docker.sock\"); assert not os.environ.get(\"OPENAI_API_KEY\"); s=socket.socket(); s.settimeout(1); assert s.connect_ex((\"1.1.1.1\",443)) != 0; print(\"isolated\")'"}),context(dir.path())).await?;
    assert_eq!(result["exit_code"], 0);
    assert!(result["stdout"].as_str().unwrap().contains("isolated"));
    Ok(())
}
#[tokio::test]
async fn streamable_http_discovery_and_call() -> Result<()> {
    use axum::{Json, Router, http::StatusCode, response::IntoResponse, routing::post};
    async fn endpoint(Json(request): Json<serde_json::Value>) -> axum::response::Response {
        if request.get("id").is_none() {
            return StatusCode::ACCEPTED.into_response();
        }
        let result = match request["method"].as_str().unwrap_or("") {
            "initialize" => {
                json!({"protocolVersion":request["params"]["protocolVersion"],"capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"}})
            }
            "tools/list" => {
                json!({"tools":[{"name":"echo","description":"Echo input","inputSchema":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}}]})
            }
            "tools/call" => {
                json!({"content":[{"type":"text","text":request["params"]["arguments"]["text"]}]})
            }
            _ => json!({}),
        };
        Json(json!({"jsonrpc":"2.0","id":request["id"],"result":result})).into_response()
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let server = tokio::spawn(async move {
        axum::serve(listener, Router::new().route("/mcp", post(endpoint)))
            .await
            .unwrap();
    });
    let registry = discover(McpConfig {
        name: "http".into(),
        command: None,
        args: vec![],
        url: Some(format!("http://{addr}/mcp")),
        token_env: None,
        env: Default::default(),
    })
    .await?;
    let dir = tempfile::tempdir()?;
    let result = registry["mcp_http_echo"]
        .execute(json!({"text":"HTTP connected"}), context(dir.path()))
        .await?;
    assert_eq!(result["content"][0]["text"], "HTTP connected");
    server.abort();
    Ok(())
}

#[tokio::test]
async fn filesystem_lifecycle_and_memory_registry() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let backend = rocketry_tools::HostBackend;
    backend
        .execute(
            "create_dir",
            json!({"path":"notes/nested"}),
            context(dir.path()),
        )
        .await?;
    backend
        .execute(
            "write_file",
            json!({"path":"notes/nested/a.txt","content":"original"}),
            context(dir.path()),
        )
        .await?;
    backend
        .execute(
            "move_file",
            json!({"path":"notes/nested/a.txt","destination":"notes/b.txt"}),
            context(dir.path()),
        )
        .await?;
    assert!(!dir.path().join("notes/nested/a.txt").exists());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("notes/b.txt"))?,
        "original"
    );
    std::fs::write(dir.path().join("existing.txt"), "preserve")?;
    assert!(
        backend
            .execute(
                "move_file",
                json!({"path":"notes/b.txt","destination":"existing.txt"}),
                context(dir.path())
            )
            .await
            .is_err()
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("existing.txt"))?,
        "preserve"
    );
    assert!(
        backend
            .execute(
                "move_file",
                json!({"path":"notes/b.txt","destination":"../escape.txt"}),
                context(dir.path())
            )
            .await
            .is_err()
    );
    assert!(
        backend
            .execute("remove_file", json!({"path":"notes"}), context(dir.path()))
            .await
            .is_err()
    );
    backend
        .execute(
            "remove_file",
            json!({"path":"notes/b.txt"}),
            context(dir.path()),
        )
        .await?;
    assert!(!dir.path().join("notes/b.txt").exists());
    let store = rocketry_store::Store::open(&dir.path().join("store"))?;
    let tools = rocketry_tools::builtins(std::sync::Arc::new(backend), store);
    tools["memory_put"]
        .execute(json!({"key":"key","value":"fact"}), context(dir.path()))
        .await?;
    assert_eq!(
        tools["memory_list"]
            .execute(json!({}), context(dir.path()))
            .await?[0]["value"],
        "fact"
    );
    assert_eq!(
        tools["memory_delete"]
            .execute(json!({"key":"key"}), context(dir.path()))
            .await?["deleted"],
        true
    );
    assert!(
        tools["memory_list"]
            .execute(json!({}), context(dir.path()))
            .await?
            .as_array()
            .unwrap()
            .is_empty()
    );
    Ok(())
}
