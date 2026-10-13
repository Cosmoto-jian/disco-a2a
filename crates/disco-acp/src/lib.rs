//! ACP → Disco bridge. Spawns a local coding agent (Claude Code, Codex, Hermes, pi,
//! OpenCode, dsh …) under a minimal sandbox and turns its ACP stream into
//! transport-neutral [`AgentEvent`]s. One child process per agent, one ACP session per
//! Disco context (DM or group). Prompts on different sessions run concurrently;
//! prompts on the same session are serialised, as ACP requires. The child is killed
//! (whole process group) when the handle drops.
#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    CancelNotification, ContentBlock, EnvVariable, InitializeRequest, McpServer, McpServerStdio, NewSessionRequest, PermissionOptionKind, PromptRequest,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome, SessionConfigKind, SessionConfigOption, SessionConfigOptionCategory,
    SessionConfigSelectOptions, SessionId, SessionNotification, SessionUpdate,
    SetSessionConfigOptionRequest, TextContent,
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
    /// Session settings to apply to every new session (config id -> value id),
    /// e.g. `model = "sonnet"`. Ids the agent doesn't offer are ignored.
    pub config: Vec<(String, String)>,
    /// A stdio MCP server handed to every new session. Each session gets
    /// `DISCO_CONTEXT=<session key>` added to its env so the server knows where it is.
    pub mcp: Option<McpStdio>,
}

/// A stdio MCP server the agent should launch for its sessions.
#[derive(Debug, Clone)]
pub struct McpStdio {
    pub name: String,
    pub command: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// One selectable session setting the agent offers: model, reasoning effort, mode …
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigChoice {
    pub id: String,
    pub name: String,
    /// `model`, `thought_level`, `mode`, `model_config` or the agent's own word.
    pub category: String,
    pub current: String,
    /// (value id, label)
    pub options: Vec<(String, String)>,
}

type Configs = Arc<Mutex<Vec<ConfigChoice>>>;

fn choices(opts: &[SessionConfigOption]) -> Vec<ConfigChoice> {
    opts.iter()
        .filter_map(|o| {
            let SessionConfigKind::Select(s) = &o.kind else { return None };
            let options = match &s.options {
                SessionConfigSelectOptions::Ungrouped(v) => v.iter().map(|x| (x.value.0.to_string(), x.name.clone())).collect(),
                SessionConfigSelectOptions::Grouped(g) => {
                    g.iter().flat_map(|g| g.options.iter().map(|x| (x.value.0.to_string(), x.name.clone()))).collect()
                }
                _ => Vec::new(),
            };
            let category = match &o.category {
                Some(SessionConfigOptionCategory::Model) => "model".to_owned(),
                Some(SessionConfigOptionCategory::ThoughtLevel) => "thought_level".into(),
                Some(SessionConfigOptionCategory::Mode) => "mode".into(),
                Some(SessionConfigOptionCategory::ModelConfig) => "model_config".into(),
                Some(SessionConfigOptionCategory::Other(c)) => c.clone(),
                _ => String::new(),
            };
            Some(ConfigChoice { id: o.id.0.to_string(), name: o.name.clone(), category, current: s.current_value.0.to_string(), options })
        })
        .collect()
}

const INHERITED: &[&str] = &["PATH", "HOME", "LANG", "TMPDIR", "USER", "SHELL",
    // Windows: Node, npm and most CLIs fail without these.
    "SystemRoot", "SystemDrive", "windir", "ComSpec", "PATHEXT", "TEMP", "TMP", "USERPROFILE", "USERNAME", "APPDATA", "LOCALAPPDATA",
    "ProgramData", "ProgramFiles", "ProgramFiles(x86)", "CommonProgramFiles", "HOMEDRIVE", "HOMEPATH", "NUMBER_OF_PROCESSORS", "PROCESSOR_ARCHITECTURE", "OS"];

type Pending = Arc<Mutex<HashMap<String, oneshot::Sender<PermissionDecision>>>>;
/// ACP session id -> event channel of the prompt currently running in it.
type Sinks = Arc<Mutex<HashMap<String, mpsc::Sender<AgentEvent>>>>;

enum Cmd {
    Prompt { key: String, text: String, events: mpsc::Sender<AgentEvent> },
    /// Create the session for `key` ahead of the first prompt.
    Warm { key: String, ready: Option<oneshot::Sender<()>> },
    /// Stop the turn running in the session for `key`.
    Cancel { key: String },
    /// Change a session setting in every open session (and every later one).
    Set { id: String, value: String, done: oneshot::Sender<Result<(), String>> },
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// A running ACP agent with one session.
pub struct AcpAgent {
    cmd: mpsc::Sender<Cmd>,
    pending: Pending,
    configs: Configs,
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
        #[cfg(windows)]
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        let mut child = c.spawn().map_err(|e| AcpError::Spawn(launch.command.clone(), e))?;
        let stdin = child.stdin.take().ok_or(AcpError::Gone)?;
        let stdout = child.stdout.take().ok_or(AcpError::Gone)?;
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(drain_stderr(stderr));
        }

        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>(32);
        let pending: Pending = Arc::default();
        let configs: Configs = Arc::default();
        let transport = ByteStreams::new(stdin.compat_write(), stdout.compat());
        tokio::spawn(run_connection(transport, launch, cmd_rx, pending.clone(), configs.clone()));
        Ok(Self { cmd: cmd_tx, pending, configs, child: Mutex::new(child) })
    }

    /// Send one prompt in the session named `key` (created on first use);
    /// events stream until a `Done`.
    pub async fn prompt(&self, key: impl Into<String>, text: impl Into<String>) -> Result<mpsc::Receiver<AgentEvent>, AcpError> {
        let (tx, rx) = mpsc::channel(512);
        self.cmd.send(Cmd::Prompt { key: key.into(), text: text.into(), events: tx }).await.map_err(|_| AcpError::Gone)?;
        Ok(rx)
    }

    /// Start the session for `key` now so the first real prompt doesn't pay for it.
    pub async fn warm(&self, key: impl Into<String>) -> Result<(), AcpError> {
        self.cmd.send(Cmd::Warm { key: key.into(), ready: None }).await.map_err(|_| AcpError::Gone)
    }

    /// The settings the agent offers, once the session for `key` exists.
    pub async fn configs(&self, key: impl Into<String>) -> Result<Vec<ConfigChoice>, AcpError> {
        let (tx, rx) = oneshot::channel();
        self.cmd.send(Cmd::Warm { key: key.into(), ready: Some(tx) }).await.map_err(|_| AcpError::Gone)?;
        let _ = rx.await;
        Ok(lock(&self.configs).clone())
    }

    /// Change a setting (e.g. the model) for every session, now and later.
    pub async fn set_config(&self, id: impl Into<String>, value: impl Into<String>) -> Result<(), AcpError> {
        let (done, rx) = oneshot::channel();
        self.cmd.send(Cmd::Set { id: id.into(), value: value.into(), done }).await.map_err(|_| AcpError::Gone)?;
        rx.await.map_err(|_| AcpError::Gone)?.map_err(AcpError::Protocol)
    }

    /// Stop the reply currently running in the session for `key` (the agent ends
    /// the turn with `cancelled`; the process and session stay alive).
    pub async fn cancel(&self, key: impl Into<String>) -> Result<(), AcpError> {
        self.cmd.send(Cmd::Cancel { key: key.into() }).await.map_err(|_| AcpError::Gone)
    }

    /// Answer a `PermissionAsked` event.
    pub fn decide(&self, request_id: &str, decision: PermissionDecision) -> bool {
        lock(&self.pending).remove(request_id).is_some_and(|tx| tx.send(decision).is_ok())
    }

    /// Kill the agent and every process it started (it leads its own process group).
    pub fn kill(&self) {
        let mut child = lock(&self.child);
        #[cfg(unix)]
        if let Some(pid) = child.id() {
            let _ = std::process::Command::new("kill").args(["-KILL", &format!("-{pid}")])
                .stdout(Stdio::null()).stderr(Stdio::null()).status();
        }
        #[cfg(windows)]
        if let Some(pid) = child.id() {
            use std::os::windows::process::CommandExt;
            let _ = std::process::Command::new("taskkill").args(["/T", "/F", "/PID", &pid.to_string()]).creation_flags(0x0800_0000)
                .stdout(Stdio::null()).stderr(Stdio::null()).status();
        }
        let _ = child.start_kill();
    }
}

async fn drain_stderr(stderr: tokio::process::ChildStderr) {
    use tokio::io::AsyncBufReadExt;
    let mut lines = tokio::io::BufReader::new(stderr).lines();
    while let Ok(Some(l)) = lines.next_line().await {
        tracing::debug!(target: "acp_child", "{l}");
    }
}

fn emit(sinks: &Sinks, session: &str, ev: AgentEvent) {
    let Some(tx) = lock(sinks).get(session).cloned() else { return };
    if matches!(ev, AgentEvent::PermissionAsked { .. }) {
        // Never lose a permission prompt: the turn would hang waiting for an answer.
        tokio::spawn(async move { let _ = tx.send(ev).await; });
    } else {
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

fn new_session(launch: &Launch, key: &str) -> NewSessionRequest {
    let req = NewSessionRequest::new(launch.cwd.clone());
    let Some(m) = &launch.mcp else { return req };
    let env = m.env.iter().map(|(k, v)| EnvVariable::new(k, v))
        .chain(std::iter::once(EnvVariable::new("DISCO_CONTEXT", key))).collect();
    req.mcp_servers(vec![McpServer::Stdio(McpServerStdio::new(&m.name, &m.command).args(m.args.clone()).env(env))])
}

async fn run_connection<W, R>(
    transport: ByteStreams<W, R>,
    launch: Launch,
    mut cmd_rx: mpsc::Receiver<Cmd>,
    pending: Pending,
    configs: Configs,
) where
    W: futures::AsyncWrite + Send + 'static,
    R: futures::AsyncRead + Send + 'static,
{
    let sinks: Sinks = Arc::default();
    let sinks_n = sinks.clone();
    let sinks_p = sinks.clone();
    let pending_p = pending.clone();
    let configs_n = configs.clone();
    let result = agent_client_protocol::Client
        .builder()
        .name("disco")
        .on_receive_notification(
            async move |n: SessionNotification, _cx| {
                if let SessionUpdate::ConfigOptionUpdate(u) = &n.update {
                    *lock(&configs_n) = choices(&u.config_options);
                }
                if let Some(ev) = map_update(n.update) {
                    emit(&sinks_n, &n.session_id.to_string(), ev);
                }
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |req: RequestPermissionRequest, responder, cx| {
                let session = req.session_id.to_string();
                // Tool-call ids are only unique within a session.
                let id = format!("{session}/{}", req.tool_call.tool_call_id);
                let (tx, rx) = oneshot::channel();
                lock(&pending_p).insert(id.clone(), tx);
                emit(&sinks_p, &session, AgentEvent::PermissionAsked {
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
            // Disco context key -> (ACP session, per-session turn lock).
            let mut sessions: HashMap<String, (SessionId, Arc<tokio::sync::Mutex<()>>)> = HashMap::new();
            let mut prefs: Vec<(String, String)> = launch.config.clone();
            // Apply one setting to one session; keeps `configs` in sync with the agent.
            let set = async |sid: &SessionId, id: &str, value: &str| -> Result<(), String> {
                let r = conn.send_request(SetSessionConfigOptionRequest::new(sid.clone(), id.to_owned(), value))
                    .block_task().await.map_err(|e| e.to_string())?;
                *lock(&configs) = choices(&r.config_options);
                Ok(())
            };
            while let Some(cmd) = cmd_rx.recv().await {
                let (key, job, ready) = match cmd {
                    Cmd::Prompt { key, text, events } => (key, Some((text, events)), None),
                    Cmd::Warm { key, ready } => (key, None, ready),
                    Cmd::Cancel { key } => {
                        if let Some((sid, _)) = sessions.get(&key) {
                            let _ = conn.send_notification(CancelNotification::new(sid.clone()));
                            // ACP: open permission prompts of that session must be answered "cancelled".
                            let prefix = format!("{sid}/");
                            let open: Vec<_> = lock(&pending).extract_if(|k, _| k.starts_with(&prefix)).map(|(_, tx)| tx).collect();
                            for tx in open { let _ = tx.send(PermissionDecision::Cancel); }
                        }
                        continue;
                    }
                    Cmd::Set { id, value, done } => {
                        prefs.retain(|(k, _)| *k != id);
                        prefs.push((id.clone(), value.clone()));
                        let mut res = Ok(());
                        for (sid, _) in sessions.values() {
                            if let Err(e) = set(sid, &id, &value).await { res = Err(e); }
                        }
                        if sessions.is_empty() {
                            // No session yet: just remember it; the next session picks it up.
                            for c in lock(&configs).iter_mut().filter(|c| c.id == id) { c.current = value.clone(); }
                        }
                        let _ = done.send(res);
                        continue;
                    }
                };
                let (session, turn) = match sessions.get(&key) {
                    Some(s) => s.clone(),
                    None => match conn.send_request(new_session(&launch, &key)).block_task().await {
                        Ok(r) => {
                            let offered = choices(r.config_options.as_deref().unwrap_or_default());
                            for (id, value) in &prefs {
                                if offered.iter().any(|c| c.id == *id && c.current != *value && c.options.iter().any(|(v, _)| v == value))
                                    && let Err(e) = set(&r.session_id, id, value).await
                                {
                                    tracing::warn!("set {id}={value}: {e}");
                                }
                            }
                            if !offered.is_empty() && lock(&configs).is_empty() { *lock(&configs) = offered; }
                            sessions.entry(key).or_insert((r.session_id, Arc::default())).clone()
                        }
                        Err(e) => {
                            if let Some((_, events)) = job {
                                let _ = events.send(AgentEvent::Done { phase: TaskPhase::Failed, stop_reason: Some(e.to_string()) }).await;
                            }
                            if let Some(r) = ready { let _ = r.send(()); }
                            continue;
                        }
                    },
                };
                if let Some(r) = ready { let _ = r.send(()); }
                let Some((text, events)) = job else { continue };
                let (conn, sinks, pending, timeout) = (conn.clone(), sinks.clone(), pending.clone(), launch.prompt_timeout);
                tokio::spawn(async move {
                    let _turn = turn.lock().await;
                    let sid = session.to_string();
                    lock(&sinks).insert(sid.clone(), events.clone());
                    let _ = events.send(AgentEvent::Phase { phase: TaskPhase::Working }).await;
                    let req = PromptRequest::new(session, vec![ContentBlock::Text(TextContent::new(text))]);
                    let done = match tokio::time::timeout(timeout, conn.send_request(req).block_task()).await {
                        Ok(Ok(r)) => AgentEvent::Done {
                            phase: TaskPhase::Completed,
                            stop_reason: Some(format!("{:?}", r.stop_reason).to_lowercase()),
                        },
                        Ok(Err(e)) => AgentEvent::Done { phase: TaskPhase::Failed, stop_reason: Some(e.to_string()) },
                        Err(_) => AgentEvent::Done { phase: TaskPhase::Failed, stop_reason: Some("timeout".into()) },
                    };
                    let prefix = format!("{sid}/");
                    lock(&pending).retain(|k, _| !k.starts_with(&prefix));
                    lock(&sinks).remove(&sid);
                    let _ = events.send(done).await;
                });
            }
            Ok(())
        })
        .await;
    if let Err(e) = result {
        tracing::warn!("acp connection ended: {e}");
    }
}
