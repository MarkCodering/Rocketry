//! Authenticated single-node REST and SSE service.
use anyhow::Result;
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, Query, Request, State},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{
        IntoResponse, Response, Sse,
        sse::{Event as SseEvent, KeepAlive},
    },
    routing::{get, post},
};
use rocketry_core::*;
use rocketry_runtime::Harness;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{convert::Infallible, sync::Arc, time::Duration};
use utoipa::{OpenApi, ToSchema};
#[derive(Clone)]
pub struct AppState {
    pub harness: Harness,
    token: Arc<String>,
}
struct ApiError(anyhow::Error);
impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(e: E) -> Self {
        Self(e.into())
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":self.0.to_string()})),
        )
            .into_response()
    }
}
type ApiResult<T> = std::result::Result<T, ApiError>;
#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct StartRequest {
    pub agent: String,
    pub input: String,
    pub session_id: Option<String>,
}
#[derive(Deserialize, ToSchema)]
pub struct SessionRequest {
    pub title: String,
}
#[derive(Deserialize, ToSchema)]
pub struct ApprovalRequest {
    pub allow: bool,
}
#[derive(Deserialize, ToSchema)]
pub struct ReconcileRequest {
    pub call_id: String,
    pub result: Value,
}
#[derive(Deserialize, Default)]
pub struct Cursor {
    pub after: Option<i64>,
    pub limit: Option<usize>,
}
async fn auth(State(s): State<AppState>, req: Request, next: Next) -> Response {
    let supplied = req
        .headers()
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    let expected = s.token.as_bytes();
    let bytes = supplied.as_bytes();
    let valid = bytes.len() == expected.len()
        && bytes
            .iter()
            .zip(expected)
            .fold(0u8, |a, (x, y)| a | (x ^ y))
            == 0;
    if !valid {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error":"authentication required"})),
        )
            .into_response();
    }
    next.run(req).await
}
#[utoipa::path(get,path="/v1/sessions",responses((status=200,body=Value)))]
async fn sessions(State(s): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(s.harness.store.sessions().await?)))
}
#[utoipa::path(post,path="/v1/sessions",request_body=SessionRequest,responses((status=200,body=Value)))]
async fn create_session(
    State(s): State<AppState>,
    Json(r): Json<SessionRequest>,
) -> ApiResult<Json<Value>> {
    Ok(Json(json!(s.harness.store.create_session(&r.title).await?)))
}
#[utoipa::path(get,path="/v1/runs",responses((status=200,body=Value)))]
async fn runs(State(s): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(s.harness.store.runs().await?)))
}
#[utoipa::path(post,path="/v1/runs",request_body=StartRequest,responses((status=202,body=Value)))]
async fn start(
    State(s): State<AppState>,
    Json(r): Json<StartRequest>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let h = s.harness.start(&r.agent, &r.input, r.session_id).await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(json!(s.harness.store.run(&h.id).await?)),
    ))
}
#[utoipa::path(get,path="/v1/runs/{id}",params(("id"=String,Path)),responses((status=200,body=Value)))]
async fn run(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(s.harness.store.run(&id).await?)))
}
#[utoipa::path(post,path="/v1/runs/{id}/cancel",params(("id"=String,Path)),responses((status=200,body=Value)))]
async fn cancel(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    s.harness.cancel(&id).await?;
    Ok(Json(json!({"cancel_requested":true})))
}
#[utoipa::path(post,path="/v1/runs/{id}/resume",params(("id"=String,Path)),responses((status=202,body=Value)))]
async fn resume(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    s.harness.resume(&id).await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(json!(s.harness.store.run(&id).await?)),
    ))
}
#[utoipa::path(get,path="/v1/runs/{id}/events",params(("id"=String,Path),("after"=Option<i64>,Query),("limit"=Option<usize>,Query)),responses((status=200,body=Value)))]
async fn events(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<Cursor>,
) -> ApiResult<Json<Value>> {
    s.harness.store.run(&id).await?;
    Ok(Json(json!(
        s.harness
            .store
            .events(&id, q.after.unwrap_or(0), q.limit.unwrap_or(500))
            .await?
    )))
}
#[utoipa::path(get,path="/v1/runs/{id}/stream",params(("id"=String,Path),("after"=Option<i64>,Query)),responses((status=200,description="SSE events; event id is a durable cursor",content_type="text/event-stream")))]
async fn stream(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<Cursor>,
    headers: HeaderMap,
) -> ApiResult<Sse<impl futures::Stream<Item = std::result::Result<SseEvent, Infallible>>>> {
    s.harness.store.run(&id).await?;
    let mut cursor = q
        .after
        .or_else(|| {
            headers
                .get("last-event-id")
                .and_then(|h| h.to_str().ok())
                .and_then(|v| v.parse().ok())
        })
        .unwrap_or(0);
    let stream = async_stream::stream! {loop{match s.harness.store.events(&id,cursor,100).await{Ok(events)=>{let empty=events.is_empty();for event in events{cursor=event.sequence;yield Ok(SseEvent::default().id(cursor.to_string()).event("run").data(serde_json::to_string(&event).unwrap_or_default()));}if empty&&s.harness.store.run(&id).await.is_ok_and(|r|r.status.terminal()){break;}},Err(_)=>break}tokio::time::sleep(Duration::from_millis(50)).await;}};
    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(10))))
}
#[utoipa::path(get,path="/v1/sessions/{id}/messages",params(("id"=String,Path)),responses((status=200,body=Value)))]
async fn messages(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(s.harness.store.messages(&id).await?)))
}
#[utoipa::path(post,path="/v1/sessions/{id}/messages",params(("id"=String,Path)),request_body=StartRequest,responses((status=202,body=Value)))]
async fn message(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(mut r): Json<StartRequest>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    r.session_id = Some(id);
    start(State(s), Json(r)).await
}
#[utoipa::path(get,path="/v1/runs/{id}/approvals",params(("id"=String,Path)),responses((status=200,body=Value)))]
async fn approvals(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(s.harness.store.approvals(&id).await?)))
}
#[utoipa::path(post,path="/v1/approvals/{id}",params(("id"=String,Path)),request_body=ApprovalRequest,responses((status=200,body=Value)))]
async fn approve(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(r): Json<ApprovalRequest>,
) -> ApiResult<Json<Value>> {
    Ok(Json(json!(s.harness.approve(&id, r.allow).await?)))
}
#[utoipa::path(post,path="/v1/runs/{id}/reconcile",params(("id"=String,Path)),request_body=ReconcileRequest,responses((status=200,body=Value)))]
async fn reconcile(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(r): Json<ReconcileRequest>,
) -> ApiResult<Json<Value>> {
    s.harness.reconcile(&id, &r.call_id, r.result).await?;
    Ok(Json(json!({"reconciled":true})))
}
#[utoipa::path(get,path="/v1/artifacts/{id}",params(("id"=String,Path)),responses((status=200,content_type="application/octet-stream")))]
async fn artifact(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<Response> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err(anyhow::anyhow!("invalid artifact id").into());
    }
    let bytes = tokio::fs::read(s.harness.store.artifacts.join(id)).await?;
    Ok((
        [
            ("content-type", "application/octet-stream"),
            ("content-disposition", "attachment"),
        ],
        bytes,
    )
        .into_response())
}
async fn agents(State(s): State<AppState>) -> Json<Value> {
    Json(json!(s.harness.agents))
}
async fn health() -> Json<Value> {
    Json(json!({"status":"ok","version":env!("CARGO_PKG_VERSION")}))
}
async fn ready(State(s): State<AppState>) -> ApiResult<Json<Value>> {
    s.harness.store.sessions().await?;
    Ok(Json(
        json!({"ready":true,"active_runs":s.harness.active_count().await}),
    ))
}
async fn metrics(State(s): State<AppState>) -> String {
    format!(
        "# TYPE rocketry_active_runs gauge\nrocketry_active_runs {}\n# TYPE rocketry_run_capacity gauge\nrocketry_run_capacity {}\n",
        s.harness.active_count().await,
        s.harness.limits.active_runs
    )
}
#[derive(OpenApi)]
#[openapi(
    paths(
        workflow,
        sessions,
        create_session,
        runs,
        start,
        run,
        cancel,
        resume,
        events,
        stream,
        messages,
        message,
        approvals,
        approve,
        reconcile,
        artifact
    ),
    components(schemas(
        StartRequest,
        SessionRequest,
        ApprovalRequest,
        ReconcileRequest,
        WorkflowRequest
    ))
)]
struct ApiDoc;
pub fn openapi() -> Value {
    let mut v = serde_json::to_value(ApiDoc::openapi()).unwrap();
    v["components"]["securitySchemes"] = json!({"bearerAuth":{"type":"http","scheme":"bearer"}});
    v["security"] = json!([{"bearerAuth":[]}]);
    v
}
pub fn router(harness: Harness, token: String) -> Result<Router> {
    anyhow::ensure!(
        token.len() >= 16,
        "server token must contain at least 16 characters"
    );
    let state = AppState {
        harness,
        token: Arc::new(token),
    };
    let protected = Router::new()
        .route("/v1/workflows", post(workflow))
        .route("/v1/sessions", get(sessions).post(create_session))
        .route("/v1/sessions/{id}/messages", get(messages).post(message))
        .route("/v1/runs", get(runs).post(start))
        .route("/v1/runs/{id}", get(run))
        .route("/v1/runs/{id}/cancel", post(cancel))
        .route("/v1/runs/{id}/resume", post(resume))
        .route("/v1/runs/{id}/events", get(events))
        .route("/v1/runs/{id}/stream", get(stream))
        .route("/v1/runs/{id}/approvals", get(approvals))
        .route("/v1/runs/{id}/reconcile", post(reconcile))
        .route("/v1/approvals/{id}", post(approve))
        .route("/v1/artifacts/{id}", get(artifact))
        .route("/v1/agents", get(agents))
        .route("/v1/openapi.json", get(|| async { Json(openapi()) }))
        .route("/ready", get(ready))
        .route("/metrics", get(metrics))
        .route_layer(middleware::from_fn_with_state(state.clone(), auth));
    Ok(protected
        .route("/health", get(health))
        .layer(DefaultBodyLimit::max(2 * 1024 * 1024))
        .with_state(state))
}
pub async fn serve(harness: Harness, addr: &str, token: String) -> Result<()> {
    let app = router(harness.clone(), token)?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(address=%listener.local_addr()?,"Rocketry server listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            harness.shutdown().await;
        })
        .await?;
    Ok(())
}
#[derive(Deserialize, ToSchema)]
pub struct WorkflowRequest {
    pub workflow: Value,
    pub input: Value,
}
#[utoipa::path(post,path="/v1/workflows",request_body=WorkflowRequest,responses((status=202,body=Value)))]
async fn workflow(
    State(s): State<AppState>,
    Json(r): Json<WorkflowRequest>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let w = serde_json::from_value(r.workflow)?;
    let handle = s.harness.start_workflow(w, r.input).await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(json!(s.harness.store.run(&handle.id).await?)),
    ))
}
#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use rocketry_providers::DemoProvider;
    use rocketry_runtime::HarnessOptions;
    use rocketry_store::Store;
    use std::collections::BTreeMap;
    use tower::ServiceExt;
    #[tokio::test]
    async fn authenticated_run_and_cursor_replay() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let store = Store::open(dir.path())?;
        let agent = Agent {
            name: "demo".into(),
            provider: "demo".into(),
            instructions: String::new(),
            tools: vec![],
            output_schema: None,
        };
        let h = Harness::new(
            store,
            BTreeMap::from([("demo".into(), agent)]),
            ProviderRegistry::from([(
                "demo".into(),
                Arc::new(DemoProvider) as Arc<dyn ModelProvider>,
            )]),
            ToolRegistry::new(),
            HarnessOptions {
                workspace: dir.path().into(),
                isolated: false,
                limits: Limits::default(),
                policy: Arc::new(PermissionPolicy::default()),
            },
        )?;
        let token = "local-test-token-123";
        let app = router(h.clone(), token.into())?;
        let response = app
            .clone()
            .oneshot(Request::builder().uri("/v1/sessions").body(Body::empty())?)
            .await?;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/runs")
                    .method("POST")
                    .header("authorization", format!("Bearer {token}"))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"agent":"demo","input":"test","session_id":null}).to_string(),
                    ))?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let run: Run = serde_json::from_slice(&to_bytes(response.into_body(), 100000).await?)?;
        tokio::time::timeout(Duration::from_secs(5), async {
            while !h.store.run(&run.id).await.unwrap().status.terminal() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await?;
        let events = h.store.events(&run.id, 0, 100).await?;
        assert!(!events.is_empty());
        let cursor = events[0].sequence;
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/v1/runs/{}/stream", run.id))
                    .header("authorization", format!("Bearer {token}"))
                    .header("last-event-id", cursor.to_string())
                    .body(Body::empty())?,
            )
            .await?;
        let body = String::from_utf8(to_bytes(response.into_body(), 100000).await?.to_vec())?;
        assert!(body.contains("event: run"));
        assert!(!body.contains(&format!("id: {cursor}\n")));
        let doc = openapi();
        assert!(doc["paths"]["/v1/runs"].is_object());
        assert!(doc["components"]["securitySchemes"]["bearerAuth"].is_object());
        Ok(())
    }
}
