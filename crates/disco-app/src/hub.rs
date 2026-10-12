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
}

#[derive(Clone)]
enum Runtime {
    A2a(Arc<RemoteAgent>),
    Acp(Arc<AcpAgent>),
}

pub struct Hub {
    live: Mutex<HashMap<String, Runtime>>,
    /// The user's login-shell PATH: GUI apps start with a bare one, so npx and
    /// Homebrew tools would otherwise be invisible.
    user_path: String,
}

pub type CredLookup = dyn Fn(&str) -> Option<String> + Send + Sync;

impl Hub {
    pub fn new() -> Self {
        Self { live: Mutex::default(), user_path: login_path() }
    }

    async fn runtime(&self, row: &AgentRow, secret: &CredLookup) -> Result<Runtime, String> {
        if let Some(r) = self.live.lock().await.get(&row.name) {
            return Ok(r.clone());
        }
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
                let cwd = match spec.cwd.strip_prefix('~') {
                    _ if spec.cwd.is_empty() => dirs_home(),
                    Some(rest) => dirs_home().join(rest.trim_start_matches('/')),
                    None => PathBuf::from(&spec.cwd),
                };
                let launch = Launch {
                    command: spec.command,
                    args: spec.args,
                    env: vec![("PATH".into(), self.user_path.clone())],
                    cwd,
                    prompt_timeout: Duration::from_secs(15 * 60),
                };
                Runtime::Acp(Arc::new(AcpAgent::spawn(launch).await.map_err(|e| e.to_string())?))
            }
            t => return Err(format!("暂不支持的类型 {t}")),
        };
        self.live.lock().await.insert(row.name.clone(), rt.clone());
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
                let mut rx = a.prompt(text).await.map_err(|e| e.to_string())?;
                let mut out = Some(Err("agent 进程已退出".into()));
                while let Some(ev) = rx.recv().await {
                    if let Some(r) = finish(ev) { out = Some(r); break; }
                }
                if matches!(out, Some(Err(_))) { self.live.lock().await.remove(&row.name); }
                out.unwrap_or(Ok(()))
            }
        };
        outcome.map(|()| answer)
    }

    pub async fn decide(&self, agent: &str, request_id: &str, d: PermissionDecision) -> bool {
        match self.live.lock().await.get(agent) {
            Some(Runtime::Acp(a)) => a.decide(request_id, d),
            _ => false,
        }
    }

    pub async fn is_live(&self, agent: &str) -> bool {
        self.live.lock().await.contains_key(agent)
    }

    pub async fn drop_agent(&self, agent: &str) {
        if let Some(Runtime::Acp(a)) = self.live.lock().await.remove(agent) {
            a.kill();
        }
    }

    /// Kill switch: stop every running agent process and forget every client.
    pub async fn stop_all(&self) -> usize {
        let all: Vec<_> = self.live.lock().await.drain().collect();
        for (_, rt) in &all {
            if let Runtime::Acp(a) = rt { a.kill(); }
        }
        all.len()
    }
}

fn dirs_home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(std::env::temp_dir)
}

fn login_path() -> String {
    let base = std::env::var("PATH").unwrap_or_default();
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    let from_shell = std::process::Command::new(shell)
        .args(["-lc", "printf %s \"$PATH\""])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_default();
    let mut parts: Vec<&str> = Vec::new();
    for p in from_shell.split(':').chain(["/opt/homebrew/bin", "/usr/local/bin"]).chain(base.split(':')) {
        if !p.is_empty() && !parts.contains(&p) { parts.push(p); }
    }
    parts.join(":")
}
