//! Agent runtimes. A2A clients and ACP child processes are started lazily and
//! cached; `ask` drives one prompt to completion and returns the full answer.
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use disco_a2a::{Credential, RemoteAgent};
use disco_acp::{AcpAgent, Launch, Login, McpStdio};
use disco_store::AgentRow;
use disco_types::{AgentEvent, PermissionDecision, TaskPhase};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

/// What `card_json` holds for local agents.
#[derive(Serialize, Deserialize, Clone, Default)]
pub struct LocalSpec {
    pub template: String,
    pub command: String,
    pub args: Vec<String>,
    pub cwd: String,
    pub description: String,
    /// Give this agent its own git worktree (branch `disco/<name>`) so several
    /// coding agents can work on one repo without overwriting each other.
    #[serde(default)]
    pub worktree: bool,
    /// Conda environment prefix to run in (its bin/ goes first on PATH).
    #[serde(default)]
    pub conda: String,
    /// Worktree directory/branch name; set when the agent is renamed so it keeps its
    /// existing worktree. Empty means "the agent's name".
    #[serde(default)]
    pub worktree_name: String,
    /// Chosen session settings (config id -> value id), e.g. the model.
    #[serde(default)]
    pub config: std::collections::BTreeMap<String, String>,
}

/// A setting the agent offers (model, reasoning effort, mode …), for the UI.
#[derive(Serialize)]
pub struct ConfigView {
    pub id: String,
    pub name: String,
    pub category: String,
    pub current: String,
    pub options: Vec<(String, String)>,
}

#[derive(Clone)]
enum Runtime {
    A2a(Arc<RemoteAgent>),
    Acp(Arc<AcpAgent>),
}

type Cell = Arc<tokio::sync::OnceCell<Runtime>>;

pub struct Hub {
    /// One cell per agent so concurrent first uses (warm-up + a prompt) start it once.
    live: Mutex<HashMap<String, Cell>>,
    /// Caps simultaneous agent turns across the whole app.
    slots: Arc<tokio::sync::Semaphore>,
    /// The user's login-shell PATH: GUI apps start with a bare one, so npx and
    /// Homebrew tools would otherwise be invisible.
    pub user_path: String,
    /// The rest of the login-shell environment (API keys, base URLs, proxies set in
    /// ~/.zshrc): an app opened from the Dock never sees them otherwise. Empty on Windows,
    /// where apps already inherit the user's environment.
    pub user_env: Vec<(String, String)>,
    /// How each agent was launched, to run its sign-in command in a terminal.
    launches: std::sync::Mutex<HashMap<String, Launch>>,
    /// Port of the loopback Disco MCP endpoint (see mcp.rs), once it listens.
    pub mcp_port: std::sync::OnceLock<u16>,
    /// MCP token -> agent name. Each ACP agent gets its own token, so a tool call
    /// can only ever act as the agent it was handed to.
    pub mcp_tokens: std::sync::Mutex<HashMap<String, String>>,
    /// Agent -> the conversation of its latest turn. Agents that can't take MCP servers
    /// per session (pi reads them from its own mcp.json) call Disco without a session
    /// key; their tools act where the agent was last asked something.
    pub last_context: std::sync::Mutex<HashMap<String, String>>,
}

pub const MAX_CONCURRENT: usize = 8;

pub type CredLookup = dyn Fn(&str) -> Option<String> + Send + Sync;

impl Hub {
    pub fn new() -> Self {
        let (user_path, user_env) = login_env();
        Self { live: Mutex::default(), slots: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT)), user_path, user_env, launches: Default::default(), mcp_port: Default::default(), mcp_tokens: Default::default(), last_context: Default::default() }
    }

    async fn runtime(&self, row: &AgentRow, secret: &CredLookup) -> Result<Runtime, String> {
        let cell = self.live.lock().await.entry(row.name.clone()).or_default().clone();
        let res = cell.get_or_try_init(|| self.start(row, secret)).await.cloned();
        if res.is_err() { self.forget(&row.name, &cell).await; }
        res
    }

    async fn forget(&self, name: &str, cell: &Cell) {
        let mut live = self.live.lock().await;
        if live.get(name).is_some_and(|c| Arc::ptr_eq(c, cell)) { live.remove(name); }
    }

    /// Start an agent (and its session for `context`) in the background, so the
    /// first message doesn't wait for npx, process start-up and session creation.
    /// Err when the agent cannot even start (the add-time self check uses this).
    pub async fn warm(&self, row: &AgentRow, secret: &CredLookup, context: &str) -> Result<(), String> {
        if let Runtime::Acp(a) = self.runtime(row, secret).await? {
            let _ = a.warm(context).await;
        }
        Ok(())
    }

    /// Slash commands a running agent advertises (empty when it isn't running).
    pub async fn commands(&self, agent: &str) -> Vec<(String, String)> {
        let cell = self.live.lock().await.get(agent).cloned();
        match cell.as_ref().and_then(|c| c.get()) {
            Some(Runtime::Acp(a)) => a.commands(),
            _ => Vec::new(),
        }
    }

    async fn start(&self, row: &AgentRow, secret: &CredLookup) -> Result<Runtime, String> {
        let rt = match row.transport.as_str() {
            "a2a" => {
                let cred = row
                    .credential_ref
                    .as_deref()
                    .and_then(secret)
                    .and_then(|s| s.split_once('\n').map(|(n, v)| Credential::Header { name: n.into(), value: v.into() }))
                    .unwrap_or(Credential::None);
                Runtime::A2a(Arc::new(RemoteAgent::connect(&row.endpoint, cred).await.map_err(|e| e.to_string())?))
            }
            "acp" => {
                let spec: LocalSpec = row.card_json.as_deref().and_then(|j| serde_json::from_str(j).ok()).ok_or("缺少启动配置")?;
                let mut cwd = match spec.cwd.strip_prefix('~') {
                    _ if spec.cwd.is_empty() => dirs_home(),
                    Some(rest) => dirs_home().join(rest.trim_start_matches('/')),
                    None => PathBuf::from(&spec.cwd),
                };
                if !cwd.is_dir() {
                    return Err(format!("工作目录不存在：{}", cwd.display()));
                }
                if spec.worktree && let Some(wt) = ensure_worktree(&cwd, if spec.worktree_name.is_empty() { &row.name } else { &spec.worktree_name }, &self.user_path) {
                    cwd = wt;
                }
                let mut path = self.user_path.clone();
                let mut env = self.user_env.clone();
                if !spec.conda.is_empty() {
                    let prefix = PathBuf::from(&spec.conda);
                    if !prefix.join("conda-meta").is_dir() { return Err(format!("conda 环境不存在：{}", spec.conda)); }
                    let bins = if cfg!(windows) { vec![prefix.clone(), prefix.join("Scripts"), prefix.join("Library").join("bin")] } else { vec![prefix.join("bin")] };
                    path = join_path(bins.iter().map(|b| b.to_string_lossy().into_owned()).chain(std::iter::once(path)));
                    let name = if prefix.parent().and_then(|d| d.file_name()).is_some_and(|n| n == "envs") {
                        prefix.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
                    } else { "base".into() };
                    env.extend([("CONDA_PREFIX".to_owned(), spec.conda.clone()), ("CONDA_DEFAULT_ENV".into(), name), ("CONDA_SHLVL".into(), "1".into())]);
                }
                env.push(("PATH".into(), path.clone()));
                // Same agent as in the terminal: the ACP adapters drive the user's own CLI
                // (same login, same settings, same default model) instead of the copy npx
                // bundles, which has never been logged in and ignores the user's setup.
                let uses = |name: &str| std::iter::once(&spec.command).chain(&spec.args).any(|a| a.contains(name));
                for (agent, cli, var) in [("claude", "claude", "CLAUDE_CODE_EXECUTABLE"), ("codex", "codex", "CODEX_PATH")] {
                    if uses(agent) && std::env::var_os(var).is_none() && !env.iter().any(|(k, _)| k == var)
                        && let Some(exe) = own_cli(cli, &path) {
                        env.push((var.into(), exe.to_string_lossy().into_owned()));
                    }
                }
                if let Some(need) = crate::templates::node_min(&spec.template) && let Some(have) = node_major(&path) && have < need {
                    return Err(format!("Node.js 版本太旧（v{have}）：这个 agent 需要 Node {need} 以上。升级 Node（如 `nvm install {need}` 或从 nodejs.org 安装）后完全退出并重新打开 Disco"));
                }
                let command = which(&spec.command, &path).ok_or_else(|| match spec.command.as_str() {
                    "npx" | "node" => "找不到 npx：请先安装 Node.js（https://nodejs.org），然后重启 Disco".to_owned(),
                    c => format!("找不到命令 {c}：请先安装它，或在终端确认 `which {c}` 有输出"),
                })?;
                let mut args = spec.args;
                // npx otherwise asks the npm registry on every launch (seconds over a VPN).
                if spec.command == "npx" && !args.iter().any(|a| a == "--prefer-offline") {
                    args.insert(0, "--prefer-offline".into());
                }
                // Windows: run npx as `node npx-cli.js` rather than through npx.cmd,
                // so no cmd.exe sits between Disco and the agent (quoting, os error 193).
                let (command, args) = match npx_cli(&command) {
                    Some((node, cli)) => (node, std::iter::once(cli).chain(args).collect()),
                    None => (command, args),
                };
                // A PowerShell script picked as the agent program runs through PowerShell.
                let (command, args) = if command.extension().is_some_and(|e| e.eq_ignore_ascii_case("ps1")) {
                    let mut a = vec!["-NoProfile".into(), "-ExecutionPolicy".into(), "Bypass".into(), "-File".into(), command.to_string_lossy().into_owned()];
                    a.extend(args);
                    (PathBuf::from("powershell"), a)
                } else { (command, args) };
                let mcp = self.mcp_for(&row.name);
                // The port and token also go in the agent's own environment, for agents that
                // load MCP servers from their config instead of from ACP (pi).
                if let Some(m) = &mcp { env.extend(m.env.iter().cloned()); }
                if spec.template == "pi" && let Some(m) = &mcp { register_pi_mcp(&env, &m.command); }
                let launch = Launch {
                    command: command.to_string_lossy().into_owned(),
                    args,
                    env,
                    cwd,
                    prompt_timeout: Duration::from_secs(15 * 60),
                    config: spec.config.into_iter().collect(),
                    mcp,
                };
                self.launches.lock().unwrap_or_else(|p| p.into_inner()).insert(row.name.clone(), launch.clone());
                Runtime::Acp(Arc::new(AcpAgent::spawn(launch).await.map_err(|e| e.to_string())?))
            }
            t => return Err(format!("暂不支持的类型 {t}")),
        };
        Ok(rt)
    }

    /// The Disco MCP server for `agent`'s sessions: this same executable in `--mcp` mode.
    fn mcp_for(&self, agent: &str) -> Option<McpStdio> {
        let port = *self.mcp_port.get()?;
        let exe = std::env::current_exe().ok()?;
        let token = uuid::Uuid::new_v4().simple().to_string();
        let mut tokens = self.mcp_tokens.lock().unwrap_or_else(|p| p.into_inner());
        tokens.retain(|_, a| a != agent);
        tokens.insert(token.clone(), agent.to_owned());
        Some(McpStdio {
            name: "disco".into(),
            command: exe,
            args: vec!["--mcp".into()],
            env: vec![("DISCO_MCP_PORT".into(), port.to_string()), ("DISCO_MCP_TOKEN".into(), token)],
        })
    }

    /// Run one prompt; every event goes to `on_event`; returns the answer text.
    pub async fn ask(
        &self,
        row: &AgentRow,
        secret: &CredLookup,
        context: &str,
        text: &str,
        on_event: impl Fn(&AgentEvent),
    ) -> Result<String, String> {
        let _slot = self.slots.acquire().await.map_err(|e| e.to_string())?;
        let rt = self.runtime(row, secret).await?;
        let mut answer = String::new();
        let mut finish = |ev: AgentEvent| -> Option<Result<(), String>> {
            on_event(&ev);
            match ev {
                AgentEvent::TextChunk { text } => { answer.push_str(&text); None }
                AgentEvent::Done { phase: TaskPhase::Completed, .. } => Some(Ok(())),
                AgentEvent::Done { phase, stop_reason } => Some(Err(format!("{phase:?}{}", stop_reason.map(|r| format!(": {r}")).unwrap_or_default()))),
                _ => None,
            }
        };
        let outcome = match rt {
            Runtime::A2a(a) => {
                let mut s = a.prompt(context, text).await.map_err(|e| e.to_string())?;
                let mut out = None;
                while let Some(ev) = s.next().await {
                    if let Some(r) = finish(ev) { out = Some(r); break; }
                }
                out.unwrap_or(Ok(()))
            }
            Runtime::Acp(a) => {
                self.last_context.lock().unwrap_or_else(|p| p.into_inner()).insert(row.name.clone(), context.to_owned());
                let mut rx = a.prompt(context, text).await.map_err(|e| e.to_string())?;
                let mut out = None;
                while let Some(ev) = rx.recv().await {
                    if let Some(r) = finish(ev) { out = Some(r); break; }
                }
                // Only a closed stream means the process died; a failed turn keeps it warm.
                out.unwrap_or_else(|| Err("agent 进程已退出".into()))
            }
        };
        outcome.map(|()| answer)
    }

    /// Settings the agent offers; starts it (and its `context` session) if needed.
    pub async fn configs(&self, row: &AgentRow, secret: &CredLookup, context: &str) -> Result<Vec<ConfigView>, String> {
        let Runtime::Acp(a) = self.runtime(row, secret).await? else { return Ok(Vec::new()) };
        let list = a.configs(context).await.map_err(|e| e.to_string())?;
        Ok(list.into_iter().map(|c| ConfigView { id: c.id, name: c.name, category: c.category, current: c.current, options: c.options }).collect())
    }

    /// Apply a setting to a running agent (no-op when it isn't running).
    pub async fn set_config(&self, agent: &str, id: &str, value: &str) -> Result<(), String> {
        let cell = self.live.lock().await.get(agent).cloned();
        match cell.as_ref().and_then(|c| c.get()) {
            Some(Runtime::Acp(a)) => a.set_config(id, value).await.map_err(|e| e.to_string()),
            _ => Ok(()),
        }
    }

    /// Stop the reply `agent` is giving in `context`. False if nothing could be stopped.
    pub async fn cancel(&self, agent: &str, context: &str) -> bool {
        let cell = self.live.lock().await.get(agent).cloned();
        match cell.as_ref().and_then(|c| c.get()) {
            Some(Runtime::Acp(a)) => a.cancel(context).await.is_ok(),
            _ => false,
        }
    }

    pub async fn decide(&self, agent: &str, request_id: &str, d: PermissionDecision) -> bool {
        let cell = self.live.lock().await.get(agent).cloned();
        match cell.as_ref().and_then(|c| c.get()) {
            Some(Runtime::Acp(a)) => a.decide(request_id, d),
            _ => false,
        }
    }

    async fn acp(&self, agent: &str) -> Option<Arc<AcpAgent>> {
        let cell = self.live.lock().await.get(agent).cloned();
        match cell.as_ref().and_then(|c| c.get()) { Some(Runtime::Acp(a)) => Some(a.clone()), _ => None }
    }

    /// The sign-in methods `agent` offers, when its last session failed for want of a login.
    pub async fn wants_login(&self, agent: &str) -> Option<Vec<Login>> {
        let a = self.acp(agent).await?;
        let (_, needs) = a.problem()?;
        Some(a.logins()).filter(|l| needs && !l.is_empty())
    }

    /// Sign `agent` in with method `id`: the agent does it itself (browser, saved key),
    /// or its own command runs the login in a terminal window. Returns what to tell the user.
    pub async fn login(&self, agent: &str, id: &str) -> Result<String, String> {
        let a = self.acp(agent).await.ok_or("agent 没在运行，先给它发一条消息")?;
        let m = a.logins().into_iter().find(|l| l.id == id).ok_or("没有这种登录方式")?;
        match &m.terminal {
            None => {
                a.login(id).await.map_err(|e| e.to_string())?;
                Ok(format!("{agent} 已登录（{}），可以继续聊了。", m.name))
            }
            Some(t) => {
                let launch = self.launches.lock().unwrap_or_else(|p| p.into_inner()).get(agent).cloned().ok_or("找不到启动方式")?;
                let args: Vec<String> = launch.args.iter().chain(&t.args).cloned().collect();
                let path = launch.env.iter().find(|(k, _)| k == "PATH").map(|(_, v)| v.clone()).unwrap_or_else(|| self.user_path.clone());
                let env: Vec<(String, String)> = std::iter::once(("PATH".to_owned(), path)).chain(t.env.iter().cloned()).collect();
                open_terminal(&launch.command, &args, &env, &launch.cwd, agent)?;
                // A fresh process picks up the new credentials on the next message.
                self.drop_agent(agent).await;
                Ok(format!("已在终端打开 {agent} 的登录（{}）。按提示完成后回到这里再发一次消息。", m.name))
            }
        }
    }

    pub async fn is_live(&self, agent: &str) -> bool {
        self.live.lock().await.get(agent).is_some_and(|c| c.initialized())
    }

    pub async fn drop_agent(&self, agent: &str) {
        let cell = self.live.lock().await.remove(agent);
        if let Some(Runtime::Acp(a)) = cell.as_ref().and_then(|c| c.get()) {
            a.kill();
        }
    }

    /// Kill switch: stop every running agent process and forget every client.
    pub async fn stop_all(&self) -> usize {
        let all: Vec<_> = self.live.lock().await.drain().collect();
        for (_, c) in &all {
            if let Some(Runtime::Acp(a)) = c.get() { a.kill(); }
        }
        all.len()
    }
}

/// If `dir` is inside a git repo, return (creating on first use) a worktree for
/// `name` at `<repo>/.disco-worktrees/<name>` on branch `disco/<name>`.
fn ensure_worktree(dir: &std::path::Path, name: &str, path: &str) -> Option<PathBuf> {
    let git = |args: &[&str], cwd: &std::path::Path| {
        quiet_cmd("git").args(args).current_dir(cwd).env("PATH", path)
            .stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null()).output().ok()
            .filter(|o| o.status.success())
    };
    let top = PathBuf::from(String::from_utf8(git(&["rev-parse", "--show-toplevel"], dir)?.stdout).ok()?.trim());
    let wt = top.join(".disco-worktrees").join(name);
    if !wt.exists() {
        let branch = format!("disco/{name}");
        let wt_s = wt.to_string_lossy().into_owned();
        // Reuse an existing branch as is: `-B` would reset it and drop the agent's earlier commits.
        let exists = git(&["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")], &top).is_some();
        if exists { git(&["worktree", "prune"], &top); git(&["worktree", "add", &wt_s, &branch], &top)?; }
        else { git(&["worktree", "add", "-b", &branch, &wt_s], &top)?; }
        // Keep the worktrees out of the user's `git status`.
        let exclude = top.join(".git/info/exclude");
        let cur = std::fs::read_to_string(&exclude).unwrap_or_default();
        if !cur.lines().any(|l| l.trim() == ".disco-worktrees/") {
            let _ = std::fs::write(&exclude, format!("{cur}{}.disco-worktrees/\n", if cur.ends_with('\n') || cur.is_empty() { "" } else { "\n" }));
        }
    }
    Some(wt)
}

pub fn dirs_home() -> PathBuf {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from).unwrap_or_else(std::env::temp_dir)
}

/// PATH list separator of this OS.
pub const PATH_SEP: char = if cfg!(windows) { ';' } else { ':' };

fn join_path(parts: impl Iterator<Item = String>) -> String {
    let mut out: Vec<String> = Vec::new();
    for p in parts.flat_map(|p| p.split(PATH_SEP).map(str::to_owned).collect::<Vec<_>>()) {
        if !p.is_empty() && !out.contains(&p) { out.push(p); }
    }
    out.join(&PATH_SEP.to_string())
}

/// Windows: a GUI app's PATH is already the user's PATH; add where npm, volta,
/// scoop and bun put their shims.
#[cfg(windows)]
fn login_env() -> (String, Vec<(String, String)>) { (login_path(), Vec::new()) }

#[cfg(windows)]
fn login_path() -> String {
    let env = |k: &str| std::env::var(k).unwrap_or_default();
    let home = dirs_home();
    let extra = [format!("{}\\npm", env("APPDATA")), format!("{}\\Volta\\bin", env("LOCALAPPDATA")), format!("{}\\nodejs", env("ProgramFiles")),
        home.join("scoop\\shims").to_string_lossy().into_owned(), home.join(".bun\\bin").to_string_lossy().into_owned(), home.join(".cargo\\bin").to_string_lossy().into_owned(),
        home.join(".local\\bin").to_string_lossy().into_owned()];
    join_path(std::iter::once(env("PATH")).chain(extra))
}

/// What the user's terminal would have: PATH plus every variable their shell rc files
/// export. GUI apps start with a bare environment, and Node from nvm/volta/fnm, API keys,
/// base URLs and proxies usually live in ~/.zshrc. Asks an interactive login shell
/// (bounded to 4 s in case an rc file waits for input) and adds the usual install
/// locations to PATH as a fallback.
#[cfg(not(windows))]
fn login_env() -> (String, Vec<(String, String)>) {
    // Started from a terminal: the environment is already the user's, and an interactive
    // shell would grab that terminal (and stop Disco when it next writes to it).
    if std::fs::File::open("/dev/tty").is_ok() { return (login_path(String::new()), Vec::new()); }
    const MARK: &str = "__DISCO_ENV__";
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    use std::os::unix::process::CommandExt;
    // Its own process group, so an interactive shell can never take over the terminal
    // Disco was started from; killed as a group if it doesn't answer in time.
    let child = std::process::Command::new(shell)
        .args(["-ilc", &format!("printf '{MARK}'; env -0")])
        .process_group(0)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn();
    let raw = match child {
        Ok(mut child) => {
            let pid = child.id();
            let mut stdout = child.stdout.take();
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                if let Some(o) = stdout.as_mut() { let _ = std::io::Read::read_to_end(o, &mut buf); }
                let _ = tx.send(String::from_utf8_lossy(&buf).into_owned());
            });
            let raw = rx.recv_timeout(std::time::Duration::from_secs(4)).unwrap_or_default();
            let _ = std::process::Command::new("kill").args(["-KILL", &format!("-{pid}")])
                .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();
            let _ = child.wait();
            raw
        }
        Err(_) => String::new(),
    };
    let vars = parse_env(raw.rsplit_once(MARK).map(|(_, e)| e).unwrap_or(""));
    let shell_path = vars.iter().find(|(k, _)| k == "PATH").map(|(_, v)| v.clone()).unwrap_or_default();
    let keep = vars.into_iter().filter(|(k, _)| k != "PATH" && !SHELL_ONLY.contains(&k.as_str()) && !k.starts_with("TERM")).collect();
    (login_path(shell_path), keep)
}

/// Shell bookkeeping that must not leak into an agent's environment.
#[cfg(not(windows))]
const SHELL_ONLY: &[&str] = &["PWD", "OLDPWD", "SHLVL", "_", "PS1", "PS2", "PROMPT", "RPROMPT", "ZDOTDIR", "SHELL_SESSION_ID", "TMUX", "STY", "ITERM_SESSION_ID", "ITERM_PROFILE", "COLORTERM", "LINES", "COLUMNS"];

/// `env -0` output: NUL-separated `KEY=value` pairs (values may contain newlines).
#[cfg(not(windows))]
fn parse_env(raw: &str) -> Vec<(String, String)> {
    raw.split('\0').filter_map(|kv| kv.split_once('=')).filter(|(k, _)| !k.is_empty() && !k.contains(char::is_whitespace))
        .map(|(k, v)| (k.to_owned(), v.to_owned())).collect()
}

#[cfg(not(windows))]
fn login_path(from_shell: String) -> String {
    let base = std::env::var("PATH").unwrap_or_default();
    let home = dirs_home();
    let mut extra: Vec<String> = [".volta/bin", ".local/bin", ".bun/bin", ".cargo/bin", ".npm-global/bin", "Library/pnpm"]
        .iter().map(|d| home.join(d).to_string_lossy().into_owned()).collect();
    // Newest nvm node, if any (by version number: v9 is older than v22).
    if let Ok(rd) = std::fs::read_dir(home.join(".nvm/versions/node")) {
        let ver = |p: &PathBuf| p.file_name().and_then(|n| n.to_str()).map(|n| n.trim_start_matches('v').split('.').map(|x| x.parse::<u32>().unwrap_or(0)).collect::<Vec<_>>()).unwrap_or_default();
        if let Some(latest) = rd.filter_map(|e| e.ok()).map(|e| e.path()).max_by_key(ver) { extra.push(latest.join("bin").to_string_lossy().into_owned()); }
    }
    join_path(std::iter::once(from_shell).chain(["/opt/homebrew/bin".into(), "/usr/local/bin".into()]).chain(extra).chain(std::iter::once(base)))
}

/// pi's ACP adapter ignores the MCP servers Disco hands each session; pi itself reads
/// them from `<agent dir>/mcp.json`. Add (or repoint) a `disco` entry there once. It
/// inherits DISCO_MCP_PORT/TOKEN from the environment, so it only does anything when pi
/// runs inside Disco; in a plain terminal it answers with no tools.
fn register_pi_mcp(env: &[(String, String)], exe: &std::path::Path) {
    let dir = env.iter().rev().find(|(k, _)| k == "PI_CODING_AGENT_DIR").map(|(_, v)| PathBuf::from(v))
        .or_else(|| std::env::var_os("PI_CODING_AGENT_DIR").map(PathBuf::from))
        .unwrap_or_else(|| dirs_home().join(".pi").join("agent"));
    let file = dir.join("mcp.json");
    let mut doc: serde_json::Value = match std::fs::read_to_string(&file) {
        Ok(text) => match serde_json::from_str(&text) { Ok(v) => v, Err(_) => return }, // never clobber a file we can't read
        Err(_) => serde_json::json!({}),
    };
    let want = serde_json::json!({ "command": exe.to_string_lossy(), "args": ["--mcp"], "exposure": "direct" });
    let Some(root) = doc.as_object_mut() else { return };
    let servers = root.entry("mcpServers").or_insert_with(|| serde_json::json!({}));
    let Some(servers) = servers.as_object_mut() else { return };
    if servers.get("disco") == Some(&want) { return; }
    servers.insert("disco".into(), want);
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(text) = serde_json::to_string_pretty(&doc) { let _ = std::fs::write(&file, text + "\n"); }
}

/// The user's own `claude` / `codex` on `path` (not an ACP adapter of the same name).
fn own_cli(name: &str, path: &str) -> Option<PathBuf> {
    // Windows: only a real .exe; node can't spawn npm's .cmd shims without a shell.
    let exe = if cfg!(windows) { which(&format!("{name}.exe"), path)? } else { which(name, path)? };
    let real = std::fs::canonicalize(&exe).unwrap_or_else(|_| exe.clone());
    (!real.to_string_lossy().contains("-acp")).then_some(exe)
}

/// Major version of the `node` on `path`.
fn node_major(path: &str) -> Option<u32> {
    let node = which("node", path)?;
    let out = quiet_cmd(node).arg("--version").env("PATH", path).stdin(std::process::Stdio::null()).output().ok()?;
    String::from_utf8_lossy(&out.stdout).trim().trim_start_matches('v').split('.').next()?.parse().ok()
}

/// Open a terminal window that runs `cmd args` (an agent's own login command) with `env`.
fn open_terminal(cmd: &str, args: &[String], env: &[(String, String)], cwd: &std::path::Path, agent: &str) -> Result<(), String> {
    let name: String = agent.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    let file = std::env::temp_dir().join(format!("disco-login-{name}.{}", if cfg!(windows) { "cmd" } else if cfg!(target_os = "macos") { "command" } else { "sh" }));
    std::fs::write(&file, login_script(cmd, args, env, cwd)).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o700));
    }
    let f = file.to_string_lossy().into_owned();
    let opened = if cfg!(windows) {
        quiet_cmd("cmd").args(["/c", "start", "", &f]).spawn().is_ok()
    } else if cfg!(target_os = "macos") {
        std::process::Command::new("open").args(["-a", "Terminal", &f]).status().is_ok_and(|s| s.success())
    } else {
        ["x-terminal-emulator", "gnome-terminal", "konsole", "xterm"].iter().any(|t| std::process::Command::new(t).args(["-e", "sh", &f]).spawn().is_ok())
    };
    if opened { Ok(()) } else { Err(format!("打不开终端，请手动运行：{f}")) }
}

#[cfg(windows)]
fn login_script(cmd: &str, args: &[String], env: &[(String, String)], cwd: &std::path::Path) -> String {
    let q = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
    let mut script = format!("@echo off\r\ncd /d {}\r\n", q(&cwd.to_string_lossy()));
    for (k, v) in env { script.push_str(&format!("set \"{k}={v}\"\r\n")); }
    let line: Vec<String> = std::iter::once(cmd).chain(args.iter().map(String::as_str)).map(q).collect();
    script.push_str(&format!("{}\r\necho.\r\necho 完成后可以关闭这个窗口，回到 Disco。\r\npause\r\n", line.join(" ")));
    script
}

#[cfg(not(windows))]
fn login_script(cmd: &str, args: &[String], env: &[(String, String)], cwd: &std::path::Path) -> String {
    let q = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
    let mut script = format!("#!/bin/sh\ncd {}\n", q(&cwd.to_string_lossy()));
    for (k, v) in env { script.push_str(&format!("export {k}={}\n", q(v))); }
    let line: Vec<String> = std::iter::once(cmd).chain(args.iter().map(String::as_str)).map(q).collect();
    script.push_str(&format!("{}\necho\necho '完成后可以关闭这个窗口，回到 Disco。'\n", line.join(" ")));
    script
}

/// Full path of `cmd` on `path`, if it is there.
/// On Windows `npx` is `npx.cmd`, so the usual executable extensions are tried too.
pub fn which(cmd: &str, path: &str) -> Option<PathBuf> {
    // Windows: npm drops an extensionless POSIX shell script (`claude`) next to
    // `claude.cmd`; launching that script fails with os error 193 ("not a valid
    // Win32 application"). So try real executable extensions first, and take a
    // bare name only when it already carries one of them.
    let exts: &[&str] = if cfg!(windows) { &[".exe", ".cmd", ".bat", ".com", ""] } else { &[""] };
    let runnable = |p: &std::path::Path| p.is_file() && (!cfg!(windows) || p.extension().and_then(|e| e.to_str())
        .is_some_and(|e| ["exe", "cmd", "bat", "com", "ps1"].contains(&e.to_ascii_lowercase().as_str())));
    let with_ext = |p: PathBuf| exts.iter().map(|e| PathBuf::from(format!("{}{e}", p.display()))).find(|p| runnable(p));
    if cmd.contains('/') || cmd.contains('\\') { return with_ext(PathBuf::from(cmd)); }
    path.split(PATH_SEP).filter(|d| !d.is_empty()).find_map(|d| with_ext(PathBuf::from(d).join(cmd)))
}

/// `npx(.cmd)` in a Node.js folder → that folder's `node.exe` and `npx-cli.js`.
fn npx_cli(command: &std::path::Path) -> Option<(PathBuf, String)> {
    if !cfg!(windows) || !command.file_stem().is_some_and(|s| s.eq_ignore_ascii_case("npx")) { return None; }
    let dir = command.parent()?;
    let node = dir.join("node.exe");
    let cli = dir.join("node_modules").join("npm").join("bin").join("npx-cli.js");
    (node.is_file() && cli.is_file()).then(|| (node, cli.to_string_lossy().into_owned()))
}

/// A command that never flashes a console window on Windows.
pub fn quiet_cmd(program: impl AsRef<std::ffi::OsStr>) -> std::process::Command {
    #[allow(unused_mut)]
    let mut c = std::process::Command::new(program);
    #[cfg(windows)]
    { use std::os::windows::process::CommandExt; c.creation_flags(0x0800_0000); }
    c
}

#[cfg(all(test, not(windows)))]
mod env_tests {
    #[test]
    fn parses_env_dump() {
        let v = super::parse_env("A=1\0MULTI=x\ny\0BAD\0=skip\0B=a=b\0");
        assert_eq!(v, vec![("A".into(), "1".into()), ("MULTI".into(), "x\ny".into()), ("B".into(), "a=b".into())]);
    }

    #[test]
    fn reads_login_shell_env_quickly() {
        let t = std::time::Instant::now();
        let (path, vars) = super::login_env();
        assert!(!path.is_empty() && t.elapsed().as_secs() < 6, "{:?}", t.elapsed());
        assert!(vars.iter().all(|(k, _)| k != "PATH" && k != "PWD"));
    }

    #[test]
    fn pi_mcp_entry_merges() {
        let d = std::env::temp_dir().join(format!("disco-pi-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("mcp.json"), r#"{"mcpServers":{"mine":{"command":"x"}},"other":1}"#).unwrap();
        let env = vec![("PI_CODING_AGENT_DIR".to_owned(), d.to_string_lossy().into_owned())];
        super::register_pi_mcp(&env, std::path::Path::new("/Apps/Disco"));
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(d.join("mcp.json")).unwrap()).unwrap();
        assert_eq!(v["mcpServers"]["mine"]["command"], "x");
        assert_eq!(v["other"], 1);
        assert_eq!(v["mcpServers"]["disco"]["command"], "/Apps/Disco");
        assert_eq!(v["mcpServers"]["disco"]["args"][0], "--mcp");
        std::fs::write(d.join("mcp.json"), "{ broken").unwrap();
        super::register_pi_mcp(&env, std::path::Path::new("/Apps/Disco"));
        assert_eq!(std::fs::read_to_string(d.join("mcp.json")).unwrap(), "{ broken", "never clobber an unreadable config");
    }

    #[test]
    fn finds_own_cli_not_adapter() {
        use std::os::unix::fs::PermissionsExt;
        let d = std::env::temp_dir().join(format!("disco-cli-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        let real = d.join("claude-agent-acp");
        std::fs::write(&real, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o755)).unwrap();
        let _ = std::fs::remove_file(d.join("claude"));
        std::os::unix::fs::symlink(&real, d.join("claude")).unwrap();
        let p = d.to_string_lossy().into_owned();
        assert!(super::own_cli("claude", &p).is_none(), "an adapter is not the user's CLI");
        std::fs::remove_file(d.join("claude")).unwrap();
        std::fs::copy(&real, d.join("claude")).unwrap();
        assert_eq!(super::own_cli("claude", &p), Some(d.join("claude")));
    }

    #[test]
    fn login_script_quotes() {
        let s = super::login_script("/x/agent", &["--cli".into(), "it's".into()], &[("PATH".into(), "/a b".into())], std::path::Path::new("/tmp"));
        assert!(s.contains("export PATH='/a b'") && s.contains("'/x/agent' '--cli' 'it'\\''s'"), "{s}");
    }
}
