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
    let events: Vec<AgentEvent> = agent.prompt("ctx-1", "hi").await.unwrap().collect().await;
    assert!(events.contains(&AgentEvent::Phase { phase: TaskPhase::Working }), "{events:?}");
    assert!(events.contains(&AgentEvent::TextChunk { text: "echo:hi".into() }), "{events:?}");
    assert!(matches!(events.last(), Some(AgentEvent::Done { phase: TaskPhase::Completed, .. })), "{events:?}");
}
