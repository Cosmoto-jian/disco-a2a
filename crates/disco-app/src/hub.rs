//! Agent runtimes. A2A clients and ACP child processes are started lazily and
//! cached; `ask` drives one prompt to completion and returns the full answer.
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use disco_a2a::{Credential, RemoteAgent};
use disco_acp::{AcpAgent, Launch};
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
}

pub const MAX_CONCURRENT: usize = 8;

pub type CredLookup = dyn Fn(&str) -> Option<String> + Send + Sync;

impl Hub {
    pub fn new() -> Self {
        Self { live: Mutex::default(), slots: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT)), user_path: login_path() }
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
    pub async fn warm(&self, row: &AgentRow, secret: &CredLookup, context: &str) {
        if let Ok(Runtime::Acp(a)) = self.runtime(row, secret).await {
            let _ = a.warm(context).await;
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
                if spec.worktree && let Some(wt) = ensure_worktree(&cwd, &row.name, &self.user_path) {
                    cwd = wt;
                }
                let mut path = self.user_path.clone();
                let mut env = Vec::new();
                if !spec.conda.is_empty() {
                    let prefix = PathBuf::from(&spec.conda);
                    if !prefix.join("conda-meta").is_dir() { return Err(format!("conda 环境不存在：{}", spec.conda)); }
                    path = format!("{}:{path}", prefix.join("bin").display());
                    let name = if prefix.parent().and_then(|d| d.file_name()).is_some_and(|n| n == "envs") {
                        prefix.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
                    } else { "base".into() };
                    env.extend([("CONDA_PREFIX".to_owned(), spec.conda.clone()), ("CONDA_DEFAULT_ENV".into(), name), ("CONDA_SHLVL".into(), "1".into())]);
                }
                env.push(("PATH".into(), path.clone()));
                let command = which(&spec.command, &path).ok_or_else(|| match spec.command.as_str() {
                    "npx" | "node" => "找不到 npx：请先安装 Node.js（https://nodejs.org 或 brew install node），然后重启 Disco".to_owned(),
                    c => format!("找不到命令 {c}：请先安装它，或在终端确认 `which {c}` 有输出"),
                })?;
                let mut args = spec.args;
                // npx otherwise asks the npm registry on every launch (seconds over a VPN).
                if spec.command == "npx" && !args.iter().any(|a| a == "--prefer-offline") {
                    args.insert(0, "--prefer-offline".into());
                }
                let launch = Launch {
                    command: command.to_string_lossy().into_owned(),
                    args,
                    env,
                    cwd,
                    prompt_timeout: Duration::from_secs(15 * 60),
                };
                Runtime::Acp(Arc::new(AcpAgent::spawn(launch).await.map_err(|e| e.to_string())?))
            }
            t => return Err(format!("暂不支持的类型 {t}")),
        };
        Ok(rt)
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

    pub async fn decide(&self, agent: &str, request_id: &str, d: PermissionDecision) -> bool {
        let cell = self.live.lock().await.get(agent).cloned();
        match cell.as_ref().and_then(|c| c.get()) {
            Some(Runtime::Acp(a)) => a.decide(request_id, d),
            _ => false,
        }
    }

    pub async fn is_live(&self, agent: &str) -> bool {
        self.live.lock().await.get(agent).is_some_and(|c| c.initialized())
    }

    pub async fn drop_agent(&self, agent: &str) {
        if let Some(Runtime::Acp(a)) = self.live.lock().await.remove(agent).as_ref().and_then(|c| c.get()) {
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
        std::process::Command::new("git").args(args).current_dir(cwd).env("PATH", path)
            .stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null()).output().ok()
            .filter(|o| o.status.success())
    };
    let top = PathBuf::from(String::from_utf8(git(&["rev-parse", "--show-toplevel"], dir)?.stdout).ok()?.trim());
    let wt = top.join(".disco-worktrees").join(name);
    if !wt.exists() {
        let branch = format!("disco/{name}");
        let wt_s = wt.to_string_lossy().into_owned();
        git(&["worktree", "add", "-B", &branch, &wt_s], &top)?;
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
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(std::env::temp_dir)
}

/// The PATH the user's terminal would have. GUI apps start with a bare PATH, and
/// Node from nvm/volta/fnm usually lives in ~/.zshrc, so ask an interactive login
/// shell (bounded to 4 s in case an rc file waits for input) and add the usual
/// install locations as a fallback.
fn login_path() -> String {
    const MARK: &str = "__DISCO_PATH__";
    let base = std::env::var("PATH").unwrap_or_default();
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let out = std::process::Command::new(shell)
            .args(["-ilc", &format!("printf '{MARK}%s{MARK}' \"$PATH\"")])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output();
        let _ = tx.send(out.ok().and_then(|o| String::from_utf8(o.stdout).ok()).unwrap_or_default());
    });
    let raw = rx.recv_timeout(std::time::Duration::from_secs(4)).unwrap_or_default();
    let from_shell = raw.split(MARK).nth(1).unwrap_or("").to_owned();
    let home = dirs_home();
    let mut extra: Vec<String> = [".volta/bin", ".local/bin", ".bun/bin", ".cargo/bin", ".npm-global/bin", "Library/pnpm"]
        .iter().map(|d| home.join(d).to_string_lossy().into_owned()).collect();
    // Newest nvm node, if any.
    if let Ok(rd) = std::fs::read_dir(home.join(".nvm/versions/node")) {
        let mut v: Vec<_> = rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
        v.sort();
        if let Some(latest) = v.last() { extra.push(latest.join("bin").to_string_lossy().into_owned()); }
    }
    let mut parts: Vec<&str> = Vec::new();
    for p in from_shell.split(':').chain(["/opt/homebrew/bin", "/usr/local/bin"]).chain(extra.iter().map(String::as_str)).chain(base.split(':')) {
        if !p.is_empty() && !parts.contains(&p) { parts.push(p); }
    }
    parts.join(":")
}

/// Full path of `cmd` on `path`, if it is there.
pub fn which(cmd: &str, path: &str) -> Option<PathBuf> {
    if cmd.contains('/') { return Some(PathBuf::from(cmd)).filter(|p| p.is_file()); }
    path.split(':').map(|d| PathBuf::from(d).join(cmd)).find(|p| p.is_file())
}
