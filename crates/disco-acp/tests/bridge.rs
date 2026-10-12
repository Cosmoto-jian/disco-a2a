//! M0-4 (mechanics): spawn the mock agent, stream, answer a permission ask, check env isolation.
use std::time::Duration;

use disco_acp::{AcpAgent, Launch};
use disco_types::{AgentEvent, PermissionDecision, TaskPhase};

async fn run(decision: &str) -> String {
    // SAFETY-free: set in parent only; the child must not see it.
    let launch = Launch {
        command: env!("CARGO_BIN_EXE_mock-acp-agent").into(),
        args: vec![],
        env: vec![],
        cwd: std::env::temp_dir(),
        prompt_timeout: Duration::from_secs(10),
    };
    let agent = AcpAgent::spawn(launch).await.unwrap();
    let mut rx = agent.prompt("go").await.unwrap();
    let mut text = String::new();
    while let Some(ev) = rx.recv().await {
        match ev {
            AgentEvent::TextChunk { text: t } => text.push_str(&t),
            AgentEvent::PermissionAsked { request_id, options, .. } => {
                assert_eq!(options.len(), 2);
                assert!(agent.decide(&request_id, PermissionDecision::Select { option_id: decision.into() }));
            }
            AgentEvent::Done { phase, stop_reason } => {
                assert_eq!(phase, TaskPhase::Completed, "{stop_reason:?}");
                break;
            }
            _ => {}
        }
    }
    text
}

#[tokio::test(flavor = "multi_thread")]
async fn streams_and_handles_permission() {
    assert_eq!(run("allow").await, "hello from mock env=clean ran");
    assert_eq!(run("deny").await, "hello from mock env=clean skipped");
}
