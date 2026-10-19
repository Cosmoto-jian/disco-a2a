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

#[tokio::test(flavor = "multi_thread")]
async fn timeout_cancels_the_turn() {
    let agent = AcpAgent::spawn(Launch { prompt_timeout: Duration::from_secs(1), ..launch() }).await.unwrap();
    let t = std::time::Instant::now();
    let mut rx = agent.prompt("dm", "__hang__").await.unwrap();
    let mut last = None;
    while let Some(ev) = rx.recv().await { if matches!(ev, AgentEvent::Done { .. }) { last = Some(ev); break; } }
    assert!(matches!(&last, Some(AgentEvent::Done { phase: TaskPhase::Failed, stop_reason: Some(r) }) if r == "timeout"), "{last:?}");
    // The mock only ends a hung turn when cancelled; without the cancel this waits out the 5 s grace.
    assert!(t.elapsed() < Duration::from_secs(3), "took {:?}", t.elapsed());
    // Nothing of the old turn ("late") leaks into the next one.
    assert_eq!(drive(&agent, "dm", "allow").await, "hello from mock env=inherited ran");
}

#[tokio::test(flavor = "multi_thread")]
async fn dead_agent_is_not_alive() {
    let agent = AcpAgent::spawn(launch()).await.unwrap();
    assert!(agent.is_alive());
    let mut rx = agent.prompt("dm", "__die__").await.unwrap();
    while rx.recv().await.is_some() {}
    let t = std::time::Instant::now();
    while agent.is_alive() && t.elapsed() < Duration::from_secs(5) { tokio::time::sleep(Duration::from_millis(50)).await; }
    assert!(!agent.is_alive());
}

/// An agent that never answers `initialize` fails fast with a reason instead of wedging the bridge.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn initialize_times_out() {
    let launch = Launch { command: "sleep".into(), args: vec!["30".into()], prompt_timeout: Duration::from_secs(1), ..launch() };
    let agent = AcpAgent::spawn(launch).await.unwrap();
    let t = std::time::Instant::now();
    let r = tokio::time::timeout(Duration::from_secs(5), agent.configs("dm")).await.expect("configs hung");
    assert!(matches!(&r, Err(disco_acp::AcpError::Protocol(why)) if why.contains("超时")), "{r:?}");
    assert!(t.elapsed() < Duration::from_secs(3), "took {:?}", t.elapsed());
    assert!(!agent.is_alive() || agent.prompt("dm", "go").await.is_err());
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
