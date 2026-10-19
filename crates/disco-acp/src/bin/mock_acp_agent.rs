//! Deterministic ACP agent for tests and CI: streams two chunks, asks one
//! permission, reports whether the parent's environment reached it, then ends the turn.
//! A prompt containing `__hang__` waits until it is cancelled; `__die__` answers, then exits.
use agent_client_protocol::schema::v1::{
    AgentCapabilities, CancelNotification, ContentBlock, ContentChunk, InitializeRequest, InitializeResponse, NewSessionRequest,
    NewSessionResponse, PermissionOption, PermissionOptionKind, PromptRequest, PromptResponse,
    RequestPermissionOutcome, RequestPermissionRequest, SessionId, SessionNotification,
    SessionUpdate, StopReason, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind,
};
use agent_client_protocol::{Agent, Client, ConnectionTo, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};

static CANCELLED: AtomicBool = AtomicBool::new(false);

fn chunk(cx: &ConnectionTo<Client>, s: &SessionId, text: &str) -> agent_client_protocol::Result<()> {
    cx.send_notification(SessionNotification::new(
        s.clone(),
        SessionUpdate::AgentMessageChunk(ContentChunk::new(text.to_string().into())),
    ))
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> agent_client_protocol::Result<()> {
    Agent
        .builder()
        .name("mock")
        .on_receive_request(
            async move |r: InitializeRequest, responder, _cx| {
                responder.respond(InitializeResponse::new(r.protocol_version).agent_capabilities(AgentCapabilities::new()))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_r: NewSessionRequest, responder, _cx| {
                static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);
                let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                responder.respond(NewSessionResponse::new(format!("s{n}")))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |_n: CancelNotification, _cx| { CANCELLED.store(true, Ordering::Relaxed); Ok(()) },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |r: PromptRequest, responder, cx: ConnectionTo<Client>| {
                let cx2 = cx.clone();
                let has = |w: &str| r.prompt.iter().any(|b| matches!(b, ContentBlock::Text(t) if t.text.contains(w)));
                let (hang, die) = (has("__hang__"), has("__die__"));
                cx.spawn(async move {
                    let s = r.session_id.clone();
                    if hang {
                        CANCELLED.store(false, Ordering::Relaxed);
                        while !CANCELLED.load(Ordering::Relaxed) { tokio::time::sleep(std::time::Duration::from_millis(20)).await; }
                        chunk(&cx2, &s, "late")?;
                        return responder.respond(PromptResponse::new(StopReason::Cancelled));
                    }
                    if die {
                        std::thread::spawn(|| { std::thread::sleep(std::time::Duration::from_millis(100)); std::process::exit(3) });
                        chunk(&cx2, &s, "dying")?;
                        return responder.respond(PromptResponse::new(StopReason::EndTurn));
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                    chunk(&cx2, &s, "hello ")?;
                    chunk(&cx2, &s, "from mock")?;
                    let seen = std::env::var("DISCO_ENV_PROBE").is_ok();
                    chunk(&cx2, &s, if seen { " env=inherited" } else { " env=missing" })?;
                    let ask = RequestPermissionRequest::new(
                        s.clone(),
                        ToolCallUpdate::new("tc1", ToolCallUpdateFields::new().title("rm -rf build").kind(ToolKind::Execute).status(ToolCallStatus::Pending)),
                        vec![
                            PermissionOption::new("allow", "Allow once", PermissionOptionKind::AllowOnce),
                            PermissionOption::new("deny", "Reject", PermissionOptionKind::RejectOnce),
                        ],
                    );
                    let resp = cx2.send_request(ask).block_task().await?;
                    let verdict = match resp.outcome {
                        RequestPermissionOutcome::Selected(sel) if sel.option_id.to_string() == "allow" => " ran",
                        _ => " skipped",
                    };
                    chunk(&cx2, &s, verdict)?;
                    responder.respond(PromptResponse::new(StopReason::EndTurn))
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_to(Stdio::new())
        .await
}
