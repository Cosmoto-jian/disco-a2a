//! ACP → Disco bridge. Spawns a local coding agent (Claude Code, Codex, Hermes, pi,
//! OpenCode, dsh …) under a minimal sandbox and turns its ACP stream into
//! transport-neutral [`AgentEvent`]s. One child process per agent, one ACP session per
//! Disco context; the child is killed (whole process group) when the handle drops.
#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    ContentBlock, InitializeRequest, NewSessionRequest, PermissionOptionKind, PromptRequest,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome, SessionId, SessionNotification, SessionUpdate, TextContent,
};
use agent_client_protocol::{Agent, ByteStreams, ConnectionTo};
use disco_types::{AgentEvent, PermissionChoice, PermissionDecision, TaskPhase};
use tokio::sync::{mpsc, oneshot};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

#[derive(Debug, thiserror::Error)]
pub enum AcpError {
    #[error("spawn {0}: {1}")]
    Spawn(String, std::io::Error),
    #[error("agent is not running")]
    Gone,
    #[error("acp: {0}")]
    Protocol(String),
}

/// How to launch an agent. Comes from a `.disco.toml` template.
#[derive(Debug, Clone)]
pub struct Launch {
    pub command: String,
    pub args: Vec<String>,
    /// Only these variables reach the child (plus PATH/HOME/LANG). Secrets are
    /// read from the Keychain by the caller and passed here.
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
    /// Max wall time for one prompt.
    pub prompt_timeout: Duration,
}

const INHERITED: &[&str] = &["PATH", "HOME", "LANG", "TMPDIR", "USER", "SHELL"];

type Pending = Arc<Mutex<HashMap<String, oneshot::Sender<PermissionDecision>>>>;
type Sink = Arc<Mutex<Option<mpsc::Sender<AgentEvent>>>>;

enum Cmd {
    Prompt { text: String, events: mpsc::Sender<AgentEvent> },
    Cancel,
}

/// A running ACP agent with one session.
pub struct AcpAgent {
    cmd: mpsc::Sender<Cmd>,
    pending: Pending,
    child: Mutex<tokio::process::Child>,
}

impl Drop for AcpAgent {
    fn drop(&mut self) {
        self.kill();
    }
}

impl AcpAgent {
    pub async fn spawn(launch: Launch) -> Result<Self, AcpError> {
        let mut c = tokio::process::Command::new(&launch.command);
        c.args(&launch.args)
            .env_clear()
            .envs(INHERITED.iter().filter_map(|k| std::env::var(k).ok().map(|v| (*k, v))))
            .envs(launch.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .current_dir(&launch.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        c.process_group(0);
        let mut child = c.spawn().map_err(|e| AcpError::Spawn(launch.command.clone(), e))?;
        let stdin = child.stdin.take().ok_or(AcpError::Gone)?;
        let stdout = child.stdout.take().ok_or(AcpError::Gone)?;
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(drain_stderr(stderr));
        }

        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>(8);
        let pending: Pending = Arc::default();
        let sink: Sink = Arc::default();
        let transport = ByteStreams::new(stdin.compat_write(), stdout.compat());
        tokio::spawn(run_connection(transport, launch, cmd_rx, pending.clone(), sink));
        Ok(Self { cmd: cmd_tx, pending, child: Mutex::new(child) })
    }

    /// Send one prompt; events stream until a `Done`.
    pub async fn prompt(&self, text: impl Into<String>) -> Result<mpsc::Receiver<AgentEvent>, AcpError> {
        let (tx, rx) = mpsc::channel(64);
        self.cmd.send(Cmd::Prompt { text: text.into(), events: tx }).await.map_err(|_| AcpError::Gone)?;
        Ok(rx)
    }

    /// Answer a `PermissionAsked` event.
    pub fn decide(&self, request_id: &str, decision: PermissionDecision) -> bool {
        self.pending.lock().unwrap_or_else(|p| p.into_inner()).remove(request_id).is_some_and(|tx| tx.send(decision).is_ok())
    }

    /// Kill the agent and every process it started (it leads its own process group).
    pub fn kill(&self) {
        let mut child = self.child.lock().unwrap_or_else(|p| p.into_inner());
        #[cfg(unix)]
        if let Some(pid) = child.id() {
            let _ = std::process::Command::new("kill").args(["-KILL", &format!("-{pid}")])
                .stdout(Stdio::null()).stderr(Stdio::null()).status();
        }
        let _ = child.start_kill();
    }

    pub async fn cancel(&self) {
        let _ = self.cmd.send(Cmd::Cancel).await;
    }
}

async fn drain_stderr(stderr: tokio::process::ChildStderr) {
    use tokio::io::AsyncBufReadExt;
    let mut lines = tokio::io::BufReader::new(stderr).lines();
    while let Ok(Some(l)) = lines.next_line().await {
        tracing::debug!(target: "acp_child", "{l}");
    }
}

fn emit(sink: &Sink, ev: AgentEvent) {
    if let Some(tx) = sink.lock().unwrap_or_else(|p| p.into_inner()).as_ref() {
        // Progress may be dropped under backpressure; Done is sent separately and awaited.
        let _ = tx.try_send(ev);
    }
}

fn text_of(block: &ContentBlock) -> Option<String> {
    match block {
        ContentBlock::Text(t) => Some(t.text.clone()),
        _ => None,
    }
}

fn map_update(u: SessionUpdate) -> Option<AgentEvent> {
    match u {
        SessionUpdate::AgentMessageChunk(c) => text_of(&c.content).map(|text| AgentEvent::TextChunk { text }),
        SessionUpdate::AgentThoughtChunk(c) => text_of(&c.content).map(|text| AgentEvent::Thought { text }),
        SessionUpdate::ToolCall(t) => Some(AgentEvent::Tool {
            id: t.tool_call_id.to_string(),
            title: t.title.clone(),
            status: format!("{:?}", t.status).to_lowercase(),
        }),
        SessionUpdate::ToolCallUpdate(t) => Some(AgentEvent::Tool {
            id: t.tool_call_id.to_string(),
            title: t.fields.title.clone().unwrap_or_default(),
            status: t.fields.status.map(|s| format!("{s:?}").to_lowercase()).unwrap_or_default(),
        }),
        _ => None,
    }
}

async fn run_connection<W, R>(
    transport: ByteStreams<W, R>,
    launch: Launch,
    mut cmd_rx: mpsc::Receiver<Cmd>,
    pending: Pending,
    sink: Sink,
) where
    W: futures::AsyncWrite + Send + 'static,
    R: futures::AsyncRead + Send + 'static,
{
    let sink_n = sink.clone();
    let sink_p = sink.clone();
    let pending_p = pending.clone();
    let result = agent_client_protocol::Client
        .builder()
        .name("disco")
        .on_receive_notification(
            async move |n: SessionNotification, _cx| {
                if let Some(ev) = map_update(n.update) {
                    emit(&sink_n, ev);
                }
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |req: RequestPermissionRequest, responder, cx| {
                let id = req.tool_call.tool_call_id.to_string();
                let (tx, rx) = oneshot::channel();
                pending_p.lock().unwrap_or_else(|p| p.into_inner()).insert(id.clone(), tx);
                emit(&sink_p, AgentEvent::PermissionAsked {
                    request_id: id,
                    title: req.tool_call.fields.title.clone().unwrap_or_default(),
                    options: req.options.iter().map(|o| PermissionChoice {
                        id: o.option_id.to_string(),
                        label: o.name.clone(),
                        allow: matches!(o.kind, PermissionOptionKind::AllowOnce | PermissionOptionKind::AllowAlways),
                    }).collect(),
                });
                // Never block the dispatch loop while a human decides.
                cx.spawn(async move {
                    let outcome = match rx.await {
                        Ok(PermissionDecision::Select { option_id }) => {
                            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(option_id))
                        }
                        _ => RequestPermissionOutcome::Cancelled,
                    };
                    responder.respond(RequestPermissionResponse::new(outcome))
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(transport, async move |conn: ConnectionTo<Agent>| {
            conn.send_request(InitializeRequest::new(ProtocolVersion::V1)).block_task().await?;
            let session: SessionId = conn
                .send_request(NewSessionRequest::new(launch.cwd.clone()))
                .block_task()
                .await?
                .session_id;
            while let Some(cmd) = cmd_rx.recv().await {
                let Cmd::Prompt { text, events } = cmd else { continue };
                *sink.lock().unwrap_or_else(|p| p.into_inner()) = Some(events.clone());
                let _ = events.send(AgentEvent::Phase { phase: TaskPhase::Working }).await;
                let req = PromptRequest::new(session.clone(), vec![ContentBlock::Text(TextContent::new(text))]);
                let done = match tokio::time::timeout(launch.prompt_timeout, conn.send_request(req).block_task()).await {
                    Ok(Ok(r)) => AgentEvent::Done {
                        phase: TaskPhase::Completed,
                        stop_reason: Some(format!("{:?}", r.stop_reason).to_lowercase()),
                    },
                    Ok(Err(e)) => AgentEvent::Done { phase: TaskPhase::Failed, stop_reason: Some(e.to_string()) },
                    Err(_) => AgentEvent::Done { phase: TaskPhase::Failed, stop_reason: Some("timeout".into()) },
                };
                pending.lock().unwrap_or_else(|p| p.into_inner()).clear();
                *sink.lock().unwrap_or_else(|p| p.into_inner()) = None;
                let _ = events.send(done).await;
            }
            Ok(())
        })
        .await;
    if let Err(e) = result {
        tracing::warn!("acp connection ended: {e}");
    }
}
