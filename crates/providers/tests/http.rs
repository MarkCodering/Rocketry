use anyhow::Result;
use axum::{Json, Router, extract::State, response::IntoResponse, routing::post};
use rocketry_core::*;
use rocketry_providers::{HttpProvider, Protocol, ProviderConfig};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;
#[derive(Clone)]
struct Fixture {
    frames: Vec<Value>,
    requests: Arc<Mutex<Vec<Value>>>,
}
async fn handler(State(f): State<Fixture>, Json(body): Json<Value>) -> impl IntoResponse {
    f.requests.lock().await.push(body);
    let data = f
        .frames
        .iter()
        .map(|v| format!("data: {v}\n\n"))
        .collect::<String>();
    ([("content-type", "text/event-stream")], data)
}
async fn roundtrip(protocol: Protocol, frames: Vec<Value>) -> Result<(Vec<ModelEvent>, Value)> {
    let state = Fixture {
        frames,
        requests: Default::default(),
    };
    let app = Router::new()
        .route("/{*path}", post(handler))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let p = HttpProvider::new(ProviderConfig {
        protocol,
        model: "fixture".into(),
        base_url: format!("http://{addr}/v1"),
        api_key_env: None,
        input_price_per_million: Some(1.0),
        output_price_per_million: Some(2.0),
    })?;
    let request = ModelRequest {
        model: Some("selected-model".into()),
        instructions: "test".into(),
        messages: vec![Message::text("user", "test")],
        tools: vec![],
        max_output_tokens: 100,
        output_schema: None,
    };
    let (tx, mut rx) = mpsc::channel(64);
    p.stream(request, tx, CancellationToken::new()).await?;
    let mut events = vec![];
    while let Some(e) = rx.recv().await {
        events.push(e);
    }
    let body = state.requests.lock().await[0].clone();
    if p.config.protocol != Protocol::Gemini {
        assert_eq!(body["model"], "selected-model");
    }
    server.abort();
    Ok((events, body))
}
#[tokio::test]
async fn openai_native_response() -> Result<()> {
    let(events,body)=roundtrip(Protocol::Openai,vec![json!({"type":"response.output_text.delta","delta":"hello"}),json!({"type":"response.completed","response":{"output":[{"type":"reasoning","id":"opaque","encrypted_content":"retain-me"},{"type":"function_call","call_id":"c1","name":"read_file","arguments":"{\"path\":\"README.md\"}"}],"usage":{"input_tokens":10,"output_tokens":2}}})]).await?;
    assert_eq!(body["store"], false);
    assert!(
        events
            .iter()
            .any(|e| matches!(e,ModelEvent::Call(c)if c.name=="read_file"))
    );
    assert!(events.iter().any(
        |e| matches!(e,ModelEvent::Continuation(_,v)if v[0]["encrypted_content"]=="retain-me")
    ));
    Ok(())
}
#[tokio::test]
async fn anthropic_tool_argument_fragments() -> Result<()> {
    let(events,_)=roundtrip(Protocol::Anthropic,vec![json!({"type":"message_start","message":{"usage":{"input_tokens":4}}}),json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"c1","name":"read_file","input":{}}}),json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}),json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"\"README.md\"}"}}),json!({"type":"content_block_stop","index":0}),json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":12}}),json!({"type":"message_stop"})]).await?;
    assert!(
        events
            .iter()
            .any(|e| matches!(e,ModelEvent::Call(c)if c.arguments["path"]=="README.md"))
    );
    Ok(())
}
#[tokio::test]
async fn gemini_preserves_signature() -> Result<()> {
    let(events,_)=roundtrip(Protocol::Gemini,vec![json!({"candidates":[{"content":{"parts":[{"functionCall":{"name":"read_file","args":{"path":"README.md"}},"thoughtSignature":"opaque-signature"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":2}})]).await?;
    assert!(events.iter().any(
        |e| matches!(e,ModelEvent::Continuation(_,v)if v[0]["thoughtSignature"]=="opaque-signature")
    ));
    assert!(matches!(events.last(), Some(ModelEvent::Finished)));
    Ok(())
}
#[tokio::test]
async fn compatible_stream_and_usage() -> Result<()> {
    let(events,body)=roundtrip(Protocol::Compatible,vec![json!({"choices":[{"delta":{"content":"hello"}}]}),json!({"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":2,"completion_tokens":1}})]).await?;
    assert_eq!(body["messages"][0]["role"], "system");
    assert!(
        events
            .iter()
            .any(|e| matches!(e,ModelEvent::Usage(u)if u.estimated_cost_usd==Some(0.000004)))
    );
    Ok(())
}
