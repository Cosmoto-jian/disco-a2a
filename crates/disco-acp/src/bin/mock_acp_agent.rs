//! Deterministic ACP agent for tests and CI: streams two chunks, asks one
//! permission, reports whether a forbidden env var leaked, then ends the turn.
use agent_client_protocol::schema::v1::{
    AgentCapabilities, ContentChunk, InitializeRequest, InitializeResponse, NewSessionRequest,
    NewSessionResponse, PermissionOption, PermissionOptionKind, PromptRequest, PromptResponse,
    RequestPermissionOutcome, RequestPermissionRequest, SessionId, SessionNotification,
    SessionUpdate, StopReason, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind,
};
use agent_client_protocol::{Agent, Client, ConnectionTo, Stdio};

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
            async move |_r: NewSessionRequest, responder, _cx| responder.respond(NewSessionResponse::new("s1")),
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |r: PromptRequest, responder, cx: ConnectionTo<Client>| {
                let cx2 = cx.clone();
                cx.spawn(async move {
                    let s = r.session_id.clone();
                    chunk(&cx2, &s, "hello ")?;
                    chunk(&cx2, &s, "from mock")?;
                    let leaked = std::env::var("DISCO_LEAK_PROBE").is_ok();
                    chunk(&cx2, &s, if leaked { " env=LEAKED" } else { " env=clean" })?;
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
