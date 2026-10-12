//! M0-4 (mechanics): spawn the mock agent, stream, answer a permission ask, check env isolation.
use std::time::Duration;

use disco_acp::{AcpAgent, Launch};
use disco_types::{AgentEvent, PermissionDecision, TaskPhase};

fn launch() -> Launch {
    Launch {
        command: env!("CARGO_BIN_EXE_mock-acp-agent").into(),
        args: vec![],
        env: vec![],
        cwd: std::env::temp_dir(),
        prompt_timeout: Duration::from_secs(10),
        config: vec![],
    }
}

async fn drive(agent: &AcpAgent, key: &str, decision: &str) -> String {
    let mut rx = agent.prompt(key, "go").await.unwrap();
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
    let agent = AcpAgent::spawn(launch()).await.unwrap();
    assert_eq!(drive(&agent, "dm", "allow").await, "hello from mock env=clean ran");
    assert_eq!(drive(&agent, "dm", "deny").await, "hello from mock env=clean skipped");
}

#[tokio::test(flavor = "multi_thread")]
async fn sessions_run_concurrently() {
    let agent = AcpAgent::spawn(launch()).await.unwrap();
    let t = std::time::Instant::now();
    let (a, b, c) = tokio::join!(drive(&agent, "group:1", "allow"), drive(&agent, "group:2", "deny"), drive(&agent, "dm", "allow"));
    assert!(a.ends_with("ran") && b.ends_with("skipped") && c.ends_with("ran"));
    // Each turn sleeps 300 ms in the mock; serial would take >= 900 ms.
    assert!(t.elapsed() < Duration::from_millis(880), "took {:?}", t.elapsed());
}

#[tokio::test(flavor = "multi_thread")]
async fn kill_ends_the_turn() {
    let agent = AcpAgent::spawn(launch()).await.unwrap();
    let mut rx = agent.prompt("dm", "go").await.unwrap();
    agent.kill();
    let mut last = None;
    while let Some(ev) = rx.recv().await { last = Some(ev); }
    assert!(!matches!(last, Some(AgentEvent::Done { phase: TaskPhase::Completed, .. })));
}
