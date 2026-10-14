//! M0-4 (mechanics): spawn the mock agent, stream, answer a permission ask, check the agent inherits the environment.
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
        mcp: None,
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
    assert_eq!(drive(&agent, "dm", "allow").await, "hello from mock env=inherited ran");
    assert_eq!(drive(&agent, "dm", "deny").await, "hello from mock env=inherited skipped");
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

/// Probe a real agent: `DISCO_REAL="npx -y @agentclientprotocol/claude-agent-acp" cargo test -p disco-acp --test bridge real_agent -- --ignored --nocapture`
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn real_agent() {
    let cmd = std::env::var("DISCO_REAL").expect("DISCO_REAL");
    let mut parts = cmd.split_whitespace().map(str::to_owned);
    let launch = Launch { command: parts.next().unwrap(), args: parts.collect(), env: vec![], cwd: std::env::temp_dir(),
        prompt_timeout: Duration::from_secs(60), config: vec![], mcp: None };
    let t = std::time::Instant::now();
    let agent = AcpAgent::spawn(launch).await;
    println!("spawn: {:?} after {:?}", agent.as_ref().err(), t.elapsed());
    let agent = agent.unwrap();
    println!("configs: {:?} after {:?}", agent.configs("dm").await, t.elapsed());
    println!("logins: {:?}", agent.logins());
    println!("problem: {:?}", agent.problem());
    let mut rx = agent.prompt("dm2", "say hi").await.unwrap();
    while let Some(ev) = rx.recv().await {
        if let AgentEvent::Done { phase, stop_reason } = ev { println!("turn: {phase:?} {stop_reason:?} after {:?}", t.elapsed()); break; }
    }
}
