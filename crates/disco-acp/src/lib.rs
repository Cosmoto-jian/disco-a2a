//! ACP → Disco bridge. Spawns a local coding agent (Claude Code, Codex, Hermes, pi,
//! OpenCode, dsh …) under a minimal sandbox and turns its ACP stream into
//! transport-neutral [`AgentEvent`]s. One child process per agent, one ACP session per
//! Disco context (DM or group). Prompts on different sessions run concurrently;
//! prompts on the same session are serialised, as ACP requires. The child is killed
//! (whole process group) when the handle drops.
#![forbid(unsafe_code)]

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    AuthCapabilities, AuthMethod, AuthenticateRequest, CancelNotification, ClientCapabilities, ContentBlock, EnvVariable, InitializeRequest, McpServer, NewSessionResponse, McpServerStdio, NewSessionRequest, PermissionOptionKind, PlanEntry, PlanEntryStatus, PromptRequest,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome, SessionConfigKind, SessionConfigOption, SessionConfigOptionCategory,
    SessionConfigSelectOptions, SessionId, SessionNotification, SessionUpdate,
    SetSessionConfigOptionRequest, TextContent,
};
use agent_client_protocol::{Agent, ByteStreams, ConnectionTo, ErrorCode, UntypedMessage};
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

/// How long a turn that timed out may take to wind down after it is cancelled.
const CANCEL_GRACE: Duration = Duration::from_secs(5);
/// Max wait for `initialize`, `session/new` and a setting change (capped by the prompt timeout).
const START_TIMEOUT: Duration = Duration::from_secs(60);
/// How long the agent's process group gets to exit after SIGTERM before SIGKILL.
#[cfg(unix)]
const KILL_GRACE: Duration = Duration::from_secs(1);

/// A stdio MCP server the agent should launch for its sessions.
#[derive(Debug, Clone)]
pub struct McpStdio {
    pub name: String,
    pub command: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    /// More env for the session with this key (e.g. a token bound to that session).
    pub session_env: Option<SessionEnv>,
}

type Env = Vec<(String, String)>;

/// Env for one session's MCP server, by session key.
#[derive(Clone)]
pub struct SessionEnv(pub Arc<dyn Fn(&str) -> Env + Send + Sync>);

impl std::fmt::Debug for SessionEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str("SessionEnv") }
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

/// A way the agent offers to sign in (ACP `authMethods`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Login {
    pub id: String,
    pub name: String,
    pub description: String,
    /// Sign in by running the agent's own command with these extra args (and env) in a
    /// terminal; `None` = the agent signs in by itself when asked (browser, saved key …).
    pub terminal: Option<TerminalLogin>,
}

/// Extra args and env for the agent's command when signing in from a terminal.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TerminalLogin { pub args: Vec<String>, pub env: Vec<(String, String)> }

/// Why the agent can't work right now, kept so the app can say it instead of failing silently.
#[derive(Debug, Clone, Default)]
struct Health {
    /// The last session that could not start (or the connection's end), with the agent's own words.
    error: Option<String>,
    /// That error was ACP `auth_required`.
    needs_login: bool,
    logins: Vec<Login>,
    /// Last lines the agent wrote to stderr.
    stderr: VecDeque<String>,
}
type Shared = Arc<Mutex<Health>>;

type Configs = Arc<Mutex<Vec<ConfigChoice>>>;
/// Slash commands the agent advertises: (name without `/`, description).
type Commands = Arc<Mutex<Vec<(String, String)>>>;

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

/// Older agents (Hermes, early adapters) still announce models and modes the pre-config
/// way: `models {availableModels, currentModelId}` and `modes {availableModes, currentModeId}`.
/// Turned into the same choices; `skip` holds the categories real config options already cover.
fn legacy_choices(v: &serde_json::Value, skip: &[ConfigChoice]) -> Vec<ConfigChoice> {
    let s = |x: &serde_json::Value| x.as_str().unwrap_or_default().to_owned();
    let mut out = Vec::new();
    for (key, list, cur, id_field, id, name) in [
        ("models", "availableModels", "currentModelId", "modelId", "model", "Model"),
        ("modes", "availableModes", "currentModeId", "id", "mode", "Mode"),
    ] {
        let m = &v[key];
        let Some(items) = m[list].as_array().filter(|a| !a.is_empty()) else { continue };
        if skip.iter().any(|c| c.category == id || c.id == id) { continue; }
        let options = items.iter().map(|x| { let v = s(&x[id_field]); let n = s(&x["name"]); (v.clone(), if n.is_empty() { v } else { n }) }).collect();
        out.push(ConfigChoice { id: id.into(), name: name.into(), category: id.into(), current: s(&m[cur]), options });
    }
    out
}

fn login_of(m: &AuthMethod) -> Login {
    let terminal = match m {
        AuthMethod::Terminal(t) => Some(TerminalLogin { args: t.args.clone(), env: t.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect() }),
        _ => None,
    };
    Login { id: m.id().to_string(), name: m.name().to_owned(), description: m.description().unwrap_or_default().to_owned(), terminal }
}

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
    /// ACP `authenticate` with one of the advertised methods.
    Login { id: String, done: oneshot::Sender<Result<(), String>> },
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// A running ACP agent with one session.
pub struct AcpAgent {
    cmd: mpsc::Sender<Cmd>,
    pending: Pending,
    configs: Configs,
    commands: Commands,
    health: Shared,
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
            // The agent gets the user's whole environment, as in a terminal:
            // API keys, base URLs and proxies are how CLIs find their provider.
            .envs(launch.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .current_dir(&launch.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // No kill_on_drop: `Drop` kills the whole group gracefully, and an instant SIGKILL
        // of the leader would cut that grace short.
        #[cfg(unix)]
        c.process_group(0);
        #[cfg(windows)]
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        let mut child = c.spawn().map_err(|e| AcpError::Spawn(launch.command.clone(), e))?;
        let stdin = child.stdin.take().ok_or(AcpError::Gone)?;
        let stdout = child.stdout.take().ok_or(AcpError::Gone)?;
        let health: Shared = Arc::default();
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(drain_stderr(stderr, health.clone()));
        }

        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>(32);
        let pending: Pending = Arc::default();
        let configs: Configs = Arc::default();
        let commands: Commands = Arc::default();
        let transport = ByteStreams::new(stdin.compat_write(), stdout.compat());
        tokio::spawn(run_connection(transport, launch, cmd_rx, pending.clone(), configs.clone(), commands.clone(), health.clone()));
        Ok(Self { cmd: cmd_tx, pending, configs, commands, health, child: Mutex::new(child) })
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

    /// The settings the agent offers, once the session for `key` exists. When the session
    /// can't start, the agent's reason (see [`Self::problem`]) instead of an empty list.
    pub async fn configs(&self, key: impl Into<String>) -> Result<Vec<ConfigChoice>, AcpError> {
        let (tx, rx) = oneshot::channel();
        self.cmd.send(Cmd::Warm { key: key.into(), ready: Some(tx) }).await.map_err(|_| self.gone())?;
        let _ = rx.await;
        let got = lock(&self.configs).clone();
        match self.problem() {
            Some((why, _)) if got.is_empty() => Err(AcpError::Protocol(why)),
            _ => Ok(got),
        }
    }

    /// Why the last session failed to start, and whether signing in would fix it.
    /// Includes the tail of the agent's stderr, which is usually where the real cause is.
    pub fn problem(&self) -> Option<(String, bool)> {
        let h = lock(&self.health);
        let why = h.error.clone()?;
        let tail: Vec<&str> = h.stderr.iter().map(String::as_str).filter(|l| !why.contains(*l)).collect();
        let why = if tail.is_empty() { why } else { format!("{why}\n{}", tail.join("\n")) };
        Some((why, h.needs_login))
    }

    /// The sign-in methods the agent advertised.
    pub fn logins(&self) -> Vec<Login> { lock(&self.health).logins.clone() }

    /// Sign in with one of [`Self::logins`] that the agent handles itself.
    pub async fn login(&self, id: impl Into<String>) -> Result<(), AcpError> {
        let (done, rx) = oneshot::channel();
        self.cmd.send(Cmd::Login { id: id.into(), done }).await.map_err(|_| self.gone())?;
        rx.await.map_err(|_| self.gone())?.map_err(AcpError::Protocol)
    }

    /// `Gone`, with the reason when the agent said why it stopped.
    fn gone(&self) -> AcpError {
        match self.problem() { Some((why, _)) => AcpError::Protocol(why), None => AcpError::Gone }
    }

    /// The slash commands the agent offers (known once a session has started).
    pub fn commands(&self) -> Vec<(String, String)> {
        lock(&self.commands).clone()
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

    /// False once the process has exited or its connection has ended.
    pub fn is_alive(&self) -> bool {
        let exited = lock(&self.child).try_wait();
        !self.cmd.is_closed() && matches!(exited, Ok(None))
    }

    /// Answer a `PermissionAsked` event.
    pub fn decide(&self, request_id: &str, decision: PermissionDecision) -> bool {
        lock(&self.pending).remove(request_id).is_some_and(|tx| tx.send(decision).is_ok())
    }

    /// Kill the agent and every process it started (it leads its own process group):
    /// SIGTERM, then SIGKILL after [`KILL_GRACE`]. Never blocks; the waiting runs on its own thread.
    pub fn kill(&self) {
        let mut child = lock(&self.child);
        let Some(pid) = child.id() else { return };
        #[cfg(unix)]
        {
            let signal = move |sig: &str| std::process::Command::new("kill").args([sig, &format!("-{pid}")])
                .stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success());
            let spawned = std::thread::Builder::new().name("acp-kill".into()).spawn(move || {
                // A failed SIGTERM means the group is already gone.
                if signal("-TERM") { std::thread::sleep(KILL_GRACE); signal("-KILL"); }
            });
            if spawned.is_err() { let _ = child.start_kill(); }
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            let _ = std::thread::Builder::new().name("acp-kill".into()).spawn(move || {
                let _ = std::process::Command::new("taskkill").args(["/T", "/F", "/PID", &pid.to_string()]).creation_flags(0x0800_0000)
                    .stdout(Stdio::null()).stderr(Stdio::null()).status();
            });
            let _ = child.start_kill();
        }
        #[cfg(not(any(unix, windows)))]
        { let _ = (pid, child.start_kill()); }
    }
}

async fn drain_stderr(stderr: tokio::process::ChildStderr, health: Shared) {
    use tokio::io::AsyncBufReadExt;
    let mut lines = tokio::io::BufReader::new(stderr).lines();
    while let Ok(Some(l)) = lines.next_line().await {
        tracing::debug!(target: "acp_child", "{l}");
        let l = l.trim();
        if l.is_empty() { continue; }
        let mut h = lock(&health);
        if h.stderr.len() == 6 { h.stderr.pop_front(); }
        h.stderr.push_back(l.chars().take(300).collect());
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
        SessionUpdate::Plan(p) => Some(AgentEvent::Plan { step: current_step(&p.entries) }),
        _ => None,
    }
}

/// What the agent is doing now: the first plan entry in progress.
fn current_step(entries: &[PlanEntry]) -> Option<String> {
    entries.iter().find(|e| e.status == PlanEntryStatus::InProgress)
        .map(|e| e.content.trim().to_owned()).filter(|c| !c.is_empty())
}

/// `session/new`, read raw so fields this protocol version no longer types
/// (the legacy `models` / `modes`) are not lost.
async fn open_session(conn: &ConnectionTo<Agent>, launch: &Launch, key: &str) -> agent_client_protocol::Result<(NewSessionResponse, serde_json::Value)> {
    let req = conn.send_request(UntypedMessage::new("session/new", new_session(launch, key))?).block_task();
    let raw = tokio::time::timeout(start_timeout(launch), req).await
        .map_err(|_| agent_client_protocol::Error::internal_error().data("session/new 超时"))??;
    let typed = serde_json::from_value(raw.clone()).map_err(|e| agent_client_protocol::Error::internal_error().data(e.to_string()))?;
    Ok((typed, raw))
}

fn start_timeout(launch: &Launch) -> Duration { START_TIMEOUT.min(launch.prompt_timeout) }

/// Config ids served through the legacy `session/set_model` / `set_mode` calls.
type Legacy = Arc<Mutex<HashSet<String>>>;

/// Apply one setting to one session; keeps `configs` in sync with the agent.
async fn apply(conn: &ConnectionTo<Agent>, legacy: &Legacy, configs: &Configs, wait: Duration, sid: &SessionId, id: &str, value: &str) -> Result<(), String> {
    let work = async {
        if lock(legacy).contains(id) {
            let (method, field) = if id == "model" { ("session/set_model", "modelId") } else { ("session/set_mode", "modeId") };
            let msg = UntypedMessage::new(method, serde_json::json!({ "sessionId": sid.to_string(), field: value })).map_err(|e| e.to_string())?;
            conn.send_request(msg).block_task().await.map_err(|e| e.to_string())?;
            for c in lock(configs).iter_mut().filter(|c| c.id == id) { c.current = value.to_owned(); }
            return Ok(());
        }
        let r = conn.send_request(SetSessionConfigOptionRequest::new(sid.clone(), id.to_owned(), value))
            .block_task().await.map_err(|e| e.to_string())?;
        *lock(configs) = choices(&r.config_options);
        Ok(())
    };
    tokio::time::timeout(wait, work).await.unwrap_or_else(|_| Err(format!("设置 {id} 超时")))
}

fn new_session(launch: &Launch, key: &str) -> NewSessionRequest {
    let req = NewSessionRequest::new(launch.cwd.clone());
    let Some(m) = &launch.mcp else { return req };
    let extra = m.session_env.as_ref().map(|f| (f.0)(key)).unwrap_or_default();
    let env = m.env.iter().chain(&extra).map(|(k, v)| EnvVariable::new(k, v))
        .chain(std::iter::once(EnvVariable::new("DISCO_CONTEXT", key))).collect();
    req.mcp_servers(vec![McpServer::Stdio(McpServerStdio::new(&m.name, &m.command).args(m.args.clone()).env(env))])
}

/// ACP `session/cancel`; open permission prompts of that session must be answered "cancelled".
fn cancel_turn(conn: &ConnectionTo<Agent>, pending: &Pending, sid: &SessionId) {
    let _ = conn.send_notification(CancelNotification::new(sid.clone()));
    let prefix = format!("{sid}/");
    let open: Vec<_> = lock(pending).extract_if(|k, _| k.starts_with(&prefix)).map(|(_, tx)| tx).collect();
    for tx in open { let _ = tx.send(PermissionDecision::Cancel); }
}

async fn run_connection<W, R>(
    transport: ByteStreams<W, R>,
    launch: Launch,
    mut cmd_rx: mpsc::Receiver<Cmd>,
    pending: Pending,
    configs: Configs,
    commands: Commands,
    health: Shared,
) where
    W: futures::AsyncWrite + Send + 'static,
    R: futures::AsyncRead + Send + 'static,
{
    let sinks: Sinks = Arc::default();
    let sinks_n = sinks.clone();
    let sinks_p = sinks.clone();
    let pending_p = pending.clone();
    let configs_n = configs.clone();
    let health_c = health.clone();
    let result = agent_client_protocol::Client
        .builder()
        .name("disco")
        .on_receive_notification(
            async move |n: SessionNotification, _cx| {
                if let SessionUpdate::ConfigOptionUpdate(u) = &n.update {
                    *lock(&configs_n) = choices(&u.config_options);
                }
                if let SessionUpdate::AvailableCommandsUpdate(u) = &n.update {
                    *lock(&commands) = u.available_commands.iter().map(|c| (c.name.trim_start_matches('/').to_owned(), c.description.clone())).collect();
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
                        always: matches!(o.kind, PermissionOptionKind::AllowAlways),
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
            // Advertise terminal sign-in so agents list their login commands (claude-agent-acp only does then).
            let caps = ClientCapabilities::new().auth(AuthCapabilities::new().terminal(true));
            let init = conn.send_request(InitializeRequest::new(ProtocolVersion::V1).client_capabilities(caps)).block_task();
            let wait = start_timeout(&launch);
            let init = match tokio::time::timeout(wait, init).await {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => { lock(&health_c).error = Some(format!("初始化失败：{e}")); return Err(e); }
                Err(_) => {
                    lock(&health_c).error = Some(format!("初始化超时（{} 秒没有响应）", wait.as_secs()));
                    return Err(agent_client_protocol::Error::internal_error().data("initialize timed out"));
                }
            };
            lock(&health_c).logins = init.auth_methods.iter().map(login_of).collect();
            let legacy: Legacy = Arc::default();
            // Disco context key -> (ACP session, per-session turn lock).
            let mut sessions: HashMap<String, (SessionId, Arc<tokio::sync::Mutex<()>>)> = HashMap::new();
            let mut prefs: Vec<(String, String)> = launch.config.clone();
            // Setting changes run off this loop (so Cancel never queues behind them), one at a time.
            let setting: Arc<tokio::sync::Mutex<()>> = Arc::default();
            while let Some(cmd) = cmd_rx.recv().await {
                let (key, job, ready) = match cmd {
                    Cmd::Prompt { key, text, events } => (key, Some((text, events)), None),
                    Cmd::Warm { key, ready } => (key, None, ready),
                    Cmd::Cancel { key } => {
                        if let Some((sid, _)) = sessions.get(&key) { cancel_turn(&conn, &pending, sid); }
                        continue;
                    }
                    Cmd::Login { id, done } => {
                        // Sign-in may wait on a browser; it runs off the loop.
                        let (conn, health) = (conn.clone(), health_c.clone());
                        tokio::spawn(async move {
                            let r = conn.send_request(AuthenticateRequest::new(id)).block_task().await.map(|_| ()).map_err(|e| e.to_string());
                            if r.is_ok() { let mut h = lock(&health); h.error = None; h.needs_login = false; }
                            let _ = done.send(r);
                        });
                        continue;
                    }
                    Cmd::Set { id, value, done } => {
                        prefs.retain(|(k, _)| *k != id);
                        prefs.push((id.clone(), value.clone()));
                        if sessions.is_empty() {
                            // No session yet: just remember it; the next session picks it up.
                            for c in lock(&configs).iter_mut().filter(|c| c.id == id) { c.current = value.clone(); }
                        }
                        let sids: Vec<SessionId> = sessions.values().map(|(sid, _)| sid.clone()).collect();
                        let (conn, legacy, configs, setting) = (conn.clone(), legacy.clone(), configs.clone(), setting.clone());
                        tokio::spawn(async move {
                            let _one = setting.lock().await;
                            let mut res = Ok(());
                            for sid in &sids {
                                if let Err(e) = apply(&conn, &legacy, &configs, wait, sid, &id, &value).await { res = Err(e); }
                            }
                            let _ = done.send(res);
                        });
                        continue;
                    }
                };
                let (session, turn) = match sessions.get(&key) {
                    Some(s) => s.clone(),
                    None => match open_session(&conn, &launch, &key).await {
                        Ok((r, raw)) => {
                            {
                                let mut h = lock(&health_c);
                                h.error = None;
                                h.needs_login = false;
                            }
                            let mut offered = choices(r.config_options.as_deref().unwrap_or_default());
                            let old = legacy_choices(&raw, &offered);
                            lock(&legacy).extend(old.iter().map(|c| c.id.clone()));
                            offered.extend(old);
                            for (id, value) in &prefs {
                                if offered.iter().any(|c| c.id == *id && c.current != *value && c.options.iter().any(|(v, _)| v == value))
                                    && let Err(e) = apply(&conn, &legacy, &configs, wait, &r.session_id, id, value).await
                                {
                                    tracing::warn!("set {id}={value}: {e}");
                                }
                            }
                            if !offered.is_empty() && lock(&configs).is_empty() { *lock(&configs) = offered; }
                            sessions.entry(key).or_insert((r.session_id, Arc::default())).clone()
                        }
                        Err(e) => {
                            {
                                let mut h = lock(&health_c);
                                h.needs_login = matches!(e.code, ErrorCode::AuthRequired);
                                h.error = Some(e.to_string());
                            }
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
                let health = health_c.clone();
                tokio::spawn(async move {
                    let _turn = turn.lock().await;
                    let sid = session.to_string();
                    lock(&sinks).insert(sid.clone(), events.clone());
                    let _ = events.send(AgentEvent::Phase { phase: TaskPhase::Working }).await;
                    let req = PromptRequest::new(session.clone(), vec![ContentBlock::Text(TextContent::new(text))]);
                    let reply = conn.send_request(req).block_task();
                    tokio::pin!(reply);
                    let done = match tokio::time::timeout(timeout, &mut reply).await {
                        Ok(Ok(r)) => AgentEvent::Done {
                            phase: TaskPhase::Completed,
                            stop_reason: Some(format!("{:?}", r.stop_reason).to_lowercase()),
                        },
                        Ok(Err(e)) => {
                            // Some agents (claude-agent-acp) only find out at the first prompt.
                            if matches!(e.code, ErrorCode::AuthRequired) {
                                let mut h = lock(&health);
                                h.needs_login = true;
                                h.error = Some(e.to_string());
                            }
                            AgentEvent::Done { phase: TaskPhase::Failed, stop_reason: Some(e.to_string()) }
                        }
                        Err(_) => {
                            // Stop the turn and give it a moment to end, so it can't bleed into the next one.
                            cancel_turn(&conn, &pending, &session);
                            let _ = tokio::time::timeout(CANCEL_GRACE, reply).await;
                            AgentEvent::Done { phase: TaskPhase::Failed, stop_reason: Some("timeout".into()) }
                        }
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
        let mut h = lock(&health);
        if h.error.is_none() { h.error = Some(format!("agent 已退出：{e}")); }
    } else {
        let mut h = lock(&health);
        if h.error.is_none() { h.error = Some("agent 已退出".into()); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::PlanEntryPriority;

    #[test]
    fn plan_step_is_the_first_entry_in_progress() {
        let e = |c: &str, s| PlanEntry::new(c, PlanEntryPriority::Medium, s);
        let plan = [e("读代码", PlanEntryStatus::Completed), e(" 写测试 ", PlanEntryStatus::InProgress), e("提交", PlanEntryStatus::InProgress)];
        assert_eq!(current_step(&plan).as_deref(), Some("写测试"));
        assert_eq!(current_step(&[e("提交", PlanEntryStatus::Pending)]), None);
        assert_eq!(current_step(&[e("  ", PlanEntryStatus::InProgress)]), None);
        assert_eq!(current_step(&[]), None);
    }
}
