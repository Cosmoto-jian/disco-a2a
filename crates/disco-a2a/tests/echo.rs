//! M0-3: our client against an in-process a2a-rs server, JSON-RPC + SSE, with auth header.
use std::sync::Arc;

use a2a::event::StreamResponse;
use a2a::*;
use a2a_server::*;
use disco_a2a::{Credential, RemoteAgent};
use disco_types::{AgentEvent, TaskPhase};
use futures::StreamExt;
use futures::stream::{self, BoxStream};

struct Echo;

impl AgentExecutor for Echo {
    fn execute(&self, ctx: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let text = ctx.message.as_ref().and_then(|m| m.parts.first()).and_then(|p| match &p.content {
            PartContent::Text(t) => Some(t.clone()),
            _ => None,
        }).unwrap_or_default();
        let st = |state, message| TaskStatus { state, message, timestamp: Some(chrono::Utc::now()) };
        let working = StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
            task_id: ctx.task_id.clone(), context_id: ctx.context_id.clone(),
            status: st(TaskState::Working, None), metadata: None,
        });
        let done = StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
            task_id: ctx.task_id.clone(), context_id: ctx.context_id.clone(),
            status: st(TaskState::Completed, Some(Message::new(Role::Agent, vec![Part::text(format!("echo:{text}"))]))),
            metadata: None,
        });
        Box::pin(stream::iter([Ok(working), Ok(done)]))
    }
    fn cancel(&self, _ctx: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        Box::pin(stream::empty())
    }
}

#[tokio::test]
async fn streams_echo_over_jsonrpc() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let handler = Arc::new(DefaultRequestHandler::new(Echo, InMemoryTaskStore::new()));
    let card = AgentCard {
        name: "echo".into(), description: "echo".into(), version: a2a::VERSION.into(), provider: None,
        capabilities: AgentCapabilities { streaming: Some(true), push_notifications: Some(false), extensions: None, extended_agent_card: None },
        skills: vec![], default_input_modes: vec!["text/plain".into()], default_output_modes: vec!["text/plain".into()],
        supported_interfaces: vec![AgentInterface::new(format!("{base}/jsonrpc"), TRANSPORT_PROTOCOL_JSONRPC)],
        security_schemes: None, security_requirements: None, documentation_url: None, icon_url: None, signatures: None,
    };
    let app = axum::Router::new()
        .nest("/jsonrpc", a2a_server::jsonrpc::jsonrpc_router(handler))
        .merge(a2a_server::agent_card::agent_card_router(Arc::new(StaticAgentCard::new(card))));
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let agent = RemoteAgent::connect(&base, Credential::Header { name: "authorization".into(), value: "Bearer t".into() })
        .await
        .unwrap();
    assert_eq!(agent.card.name, "echo");
    let events: Vec<AgentEvent> = agent.prompt("ctx-1", "hi", &[]).await.unwrap().collect().await;
    assert!(events.contains(&AgentEvent::Phase { phase: TaskPhase::Working }), "{events:?}");
    assert!(events.contains(&AgentEvent::TextChunk { text: "echo:hi".into() }), "{events:?}");
    assert!(matches!(events.last(), Some(AgentEvent::Done { phase: TaskPhase::Completed, .. })), "{events:?}");
}

/// Same shape as Disco's share server: agent under a path prefix, bearer token
/// checked by middleware, answer streamed as appended artifact chunks.
struct Chunky;

impl AgentExecutor for Chunky {
    fn execute(&self, ctx: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let (t, c) = ctx.task_info();
        let chunk = |s: &str, append: bool| Ok(StreamResponse::ArtifactUpdate(a2a::event::TaskArtifactUpdateEvent {
            task_id: t.clone(), context_id: c.clone(),
            artifact: Artifact { artifact_id: "answer".into(), name: None, description: None, parts: vec![Part::text(s)], metadata: None, extensions: None },
            append: Some(append), last_chunk: Some(false), metadata: None,
        }));
        let done = Ok(StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
            task_id: t.clone(), context_id: c.clone(),
            status: TaskStatus { state: TaskState::Completed, message: None, timestamp: None }, metadata: None,
        }));
        let working = Ok(StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
            task_id: t.clone(), context_id: c.clone(),
            status: TaskStatus { state: TaskState::Working, message: None, timestamp: None }, metadata: None,
        }));
        Box::pin(stream::iter([working, chunk("he", false), chunk("llo", true), done]))
    }
    fn cancel(&self, _ctx: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        Box::pin(stream::empty())
    }
}

#[tokio::test]
async fn prefixed_share_with_bearer() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/a/abc", listener.local_addr().unwrap());
    let handler = Arc::new(DefaultRequestHandler::new(Chunky, InMemoryTaskStore::new()));
    let card = AgentCard {
        name: "shared".into(), description: "d".into(), version: a2a::VERSION.into(), provider: None,
        capabilities: AgentCapabilities { streaming: Some(true), push_notifications: Some(false), extensions: None, extended_agent_card: None },
        skills: vec![], default_input_modes: vec!["text/plain".into()], default_output_modes: vec!["text/plain".into()],
        supported_interfaces: vec![AgentInterface::new(format!("{base}/jsonrpc"), TRANSPORT_PROTOCOL_JSONRPC)],
        security_schemes: None, security_requirements: None, documentation_url: None, icon_url: None, signatures: None,
    };
    let card_json = serde_json::to_value(&card).unwrap();
    async fn guard(req: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response {
        use axum::response::IntoResponse;
        if req.uri().path().ends_with("/jsonrpc") && req.headers().get("authorization").and_then(|v| v.to_str().ok()) != Some("Bearer s3cret") {
            return axum::http::StatusCode::UNAUTHORIZED.into_response();
        }
        next.run(req).await
    }
    let app = axum::Router::new()
        .route("/a/{id}/.well-known/agent-card.json", axum::routing::get(move || { let c = card_json.clone(); async move { axum::Json(c) } }))
        .nest("/a/{id}/jsonrpc", a2a_server::jsonrpc::jsonrpc_router(handler))
        .layer(axum::middleware::from_fn(guard));
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let bad = RemoteAgent::connect(&base, Credential::Header { name: "authorization".into(), value: "Bearer nope".into() }).await.unwrap();
    let ev: Vec<AgentEvent> = match bad.prompt("c", "hi", &[]).await { Ok(s) => s.collect().await, Err(_) => vec![] };
    assert!(!ev.iter().any(|e| matches!(e, AgentEvent::TextChunk { .. })), "{ev:?}");

    let agent = RemoteAgent::connect(&base, Credential::Header { name: "authorization".into(), value: "Bearer s3cret".into() }).await.unwrap();
    let events: Vec<AgentEvent> = agent.prompt("ctx", "hi", &[]).await.unwrap().collect().await;
    let text: String = events.iter().filter_map(|e| match e { AgentEvent::TextChunk { text } => Some(text.as_str()), _ => None }).collect();
    assert_eq!(text, "hello", "{events:?}");
    assert!(matches!(events.last(), Some(AgentEvent::Done { phase: TaskPhase::Completed, .. })), "{events:?}");
}
