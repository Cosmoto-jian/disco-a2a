//! Disco desktop shell (Tauri 2). The webview only calls the typed commands
//! below; secrets live in the macOS Keychain and never reach SQLite or the UI.
#![forbid(unsafe_code)]

mod hub;
mod templates;

use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};

use disco_a2a::{Credential, RemoteAgent};
use disco_discord::{Incoming, Rest};
use disco_store::{AgentRow, GroupRow, Store};
use disco_types::{AgentEvent, PermissionDecision};
use hub::{Hub, LocalSpec};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::{mpsc, watch};

const KEYCHAIN_SERVICE: &str = "dev.cosmoto.disco";
const DISCORD_KEY: &str = "discord-bot-token";
/// Hand-off rounds and total agent turns allowed per human message in a group.
const MAX_WAVES: usize = 4;
const MAX_TURNS: usize = 12;
const TRANSCRIPT: u32 = 20;

type CmdResult<T> = Result<T, String>;

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

fn secret_get(account: &str) -> Option<String> {
    keyring::Entry::new(KEYCHAIN_SERVICE, account).ok()?.get_password().ok()
}

fn secret_set(account: &str, value: &str) -> CmdResult<()> {
    keyring::Entry::new(KEYCHAIN_SERVICE, account).map_err(err)?.set_password(value).map_err(err)
}

fn secret_del(account: &str) {
    if let Ok(e) = keyring::Entry::new(KEYCHAIN_SERVICE, account) {
        let _ = e.delete_credential();
    }
}

struct Discord {
    rest: Rest,
    stop: watch::Sender<bool>,
}

pub struct Core {
    store: Mutex<Store>,
    hub: Hub,
    discord: Mutex<Option<Discord>>,
    discord_status: Mutex<String>,
    /// Kill switch: while set, nothing is routed to any agent.
    paused: AtomicBool,
    app: AppHandle,
}

type St<'a> = State<'a, Arc<Core>>;

impl Core {
    fn db(&self) -> std::sync::MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn agent(&self, name: &str) -> CmdResult<AgentRow> {
        self.db().list_agents().map_err(err)?.into_iter().find(|a| a.name == name).ok_or_else(|| format!("没有名为 {name} 的 agent"))
    }

    fn post_message(&self, thread: &str, author: &str, body: &str) {
        let ts = {
            let db = self.db();
            let _ = db.add_message(thread, author, body);
            now()
        };
        let _ = self.app.emit("message", MessageEvt { thread: thread.into(), author: author.into(), body: body.into(), ts });
    }

    fn set_discord_status(&self, s: String) {
        *self.discord_status.lock().unwrap_or_else(|p| p.into_inner()) = s.clone();
        let _ = self.app.emit("discord-status", s);
    }

    /// One agent turn: prompt, stream to the UI, return the answer.
    async fn turn(&self, thread: &str, agent: &str, context: &str, prompt: &str) -> CmdResult<String> {
        if self.paused.load(Ordering::Relaxed) {
            return Err("已全部停止".into());
        }
        let row = self.agent(agent)?;
        let _ = self.app.emit("typing", TypingEvt { thread: thread.into(), agent: agent.into(), on: true });
        let app = self.app.clone();
        let (t, a) = (thread.to_owned(), agent.to_owned());
        let res = self
            .hub
            .ask(&row, &secret_get, context, prompt, move |ev| {
                let _ = app.emit("stream", StreamEvt { thread: t.clone(), agent: a.clone(), event: ev.clone() });
            })
            .await;
        let _ = self.app.emit("typing", TypingEvt { thread: thread.into(), agent: agent.into(), on: false });
        let _ = self.db().audit("disco", Some(agent), "prompt", if res.is_ok() { "ok" } else { "error" });
        res
    }

    async fn dm(&self, agent: String, text: String) {
        let thread = format!("dm:{agent}");
        self.post_message(&thread, "me", &text);
        match self.turn(&thread, &agent, &thread, &text).await {
            Ok(answer) => self.post_message(&thread, &agent, answer.trim()),
            Err(e) => self.post_message(&thread, "system", &format!("{agent} 出错：{e}")),
        }
    }

    /// Group routing: @mentioned members answer, otherwise the lead does.
    /// An answer that @mentions another member hands off to it, up to MAX_HOPS.
    async fn group(&self, channel: String, author: String, text: String, from_discord: bool) {
        let Some(g) = self.db().list_groups().ok().and_then(|gs| gs.into_iter().find(|g| g.id == channel)) else { return };
        let thread = format!("group:{channel}");
        self.post_message(&thread, &author, &text);
        if !from_discord && let Some(d) = self.discord_rest() {
            let _ = d.post(&channel, &format!("**{author}**：{text}")).await;
        }
        let mut wave = mentions(&text, &g.members, None);
        if wave.is_empty() && let Some(lead) = g.lead_agent.clone().or_else(|| g.members.first().cloned()) {
            wave.push(lead);
        }
        // Agents addressed together run concurrently; their @handoffs form the next
        // wave. Bounded by MAX_WAVES and MAX_TURNS so agents can't ping-pong forever.
        let (mut waves, mut turns) = (0, 0);
        while !wave.is_empty() && waves < MAX_WAVES && !self.paused.load(Ordering::Relaxed) {
            waves += 1;
            wave.truncate(MAX_TURNS.saturating_sub(turns));
            turns += wave.len();
            let runs = wave.iter().map(|agent| {
                let prompt = self.group_prompt(&g, &thread, agent);
                let thread = &thread;
                let g = &g;
                async move {
                    match self.turn(thread, agent, thread, &prompt).await {
                        Ok(answer) => {
                            let answer = answer.trim().to_owned();
                            if answer.is_empty() { return Vec::new(); }
                            // Post as soon as this agent finishes, not when the wave does.
                            self.post_message(thread, agent, &answer);
                            if let (Some(d), Some(hook)) = (self.discord_rest(), g.webhook_ref.as_deref().and_then(secret_get))
                                && let Err(e) = d.post_as(&hook, agent, &answer).await
                            {
                                self.post_message(thread, "system", &format!("Discord 发送失败：{e}"));
                            }
                            mentions(&answer, &g.members, Some(agent))
                        }
                        Err(e) => {
                            self.post_message(thread, "system", &format!("{agent} 出错：{e}"));
                            Vec::new()
                        }
                    }
                }
            });
            let mut next: Vec<String> = Vec::new();
            for m in futures::future::join_all(runs).await.into_iter().flatten() {
                if !next.contains(&m) { next.push(m); }
            }
            wave = next;
        }
    }

    fn group_prompt(&self, g: &GroupRow, thread: &str, me: &str) -> String {
        let history = self.db().messages(thread, TRANSCRIPT).unwrap_or_default();
        let mut p = format!(
            "你是 {me}，正在群聊「{}」里和其他 agent 及人类协作。成员：{}。\n\
             需要别人接手时，在回复里写 @名字。只输出你要发到群里的那段话，不要复述聊天记录。\n\n最近的聊天记录：\n",
            g.name,
            g.members.join("、")
        );
        for m in history {
            let who = if m.author == "me" { "我（人类）" } else { m.author.as_str() };
            p.push_str(&format!("[{who}] {}\n", m.body));
        }
        p
    }

    fn discord_rest(&self) -> Option<Rest> {
        self.discord.lock().unwrap_or_else(|p| p.into_inner()).as_ref().map(|d| d.rest.clone())
    }
}

/// Members named as `@name` in `text` (case-insensitive, whole names only, so
/// `@coder` does not match `@coder-2`), in order of first mention, excluding `me`.
/// `@all` / `@所有人` addresses every member at once.
fn mentions(text: &str, members: &[String], me: Option<&str>) -> Vec<String> {
    let lower = text.to_lowercase();
    let others = members.iter().filter(|m| Some(m.as_str()) != me);
    if lower.contains("@all") || text.contains("@所有人") {
        return others.cloned().collect();
    }
    let is_name_char = |c: char| c.is_alphanumeric() || c == '-' || c == '_';
    let mut hits: Vec<(usize, &String)> = others
        .filter_map(|m| {
            let needle = format!("@{}", m.to_lowercase());
            lower.match_indices(&needle).map(|(i, _)| i).find(|&i| {
                !lower[i + needle.len()..].chars().next().is_some_and(is_name_char)
            }).map(|i| (i, m))
        })
        .collect();
    hits.sort_unstable_by_key(|(i, _)| *i);
    hits.into_iter().map(|(_, m)| m.clone()).collect()
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

#[derive(Clone, Serialize)]
struct MessageEvt { thread: String, author: String, body: String, ts: i64 }
#[derive(Clone, Serialize)]
struct StreamEvt { thread: String, agent: String, event: AgentEvent }
#[derive(Clone, Serialize)]
struct TypingEvt { thread: String, agent: String, on: bool }

// ------------------------------------------------------------------ commands

#[derive(Serialize)]
struct AgentView { name: String, kind: String, template: String, detail: String, live: bool }
#[derive(Serialize)]
struct GroupView { id: String, name: String, guild_id: String, lead: Option<String>, members: Vec<String> }
#[derive(Serialize)]
struct Head { thread: String, author: String, body: String, ts: i64 }
#[derive(Serialize)]
struct Overview {
    agents: Vec<AgentView>,
    groups: Vec<GroupView>,
    heads: Vec<Head>,
    discord_token: bool,
    discord_status: String,
    paused: bool,
}

#[tauri::command]
async fn overview(core: St<'_>) -> CmdResult<Overview> {
    let (rows, groups, heads) = {
        let db = core.db();
        (db.list_agents().map_err(err)?, db.list_groups().map_err(err)?, db.thread_heads().map_err(err)?)
    };
    let mut agents = Vec::with_capacity(rows.len());
    for a in rows {
        let spec: LocalSpec = a.card_json.as_deref().and_then(|j| serde_json::from_str(j).ok()).unwrap_or_default();
        let detail = if a.transport == "a2a" {
            a.card_json.as_deref()
                .and_then(|j| serde_json::from_str::<serde_json::Value>(j).ok())
                .and_then(|v| v.get("description")?.as_str().map(str::to_owned))
                .unwrap_or_else(|| a.endpoint.clone())
        } else if spec.description.is_empty() { a.endpoint.clone() } else { spec.description };
        let template = if a.transport == "a2a" { "a2a".into() } else { spec.template };
        agents.push(AgentView { live: core.hub.is_live(&a.name).await, name: a.name, kind: a.transport, template, detail });
    }
    Ok(Overview {
        agents,
        groups: groups.into_iter().map(|g| GroupView { id: g.id, name: g.name, guild_id: g.guild_id, lead: g.lead_agent, members: g.members }).collect(),
        heads: heads.into_iter().map(|(t, m)| Head { thread: t, author: m.author, body: m.body, ts: m.ts }).collect(),
        discord_token: secret_get(DISCORD_KEY).is_some(),
        discord_status: core.discord_status.lock().unwrap_or_else(|p| p.into_inner()).clone(),
        paused: core.paused.load(Ordering::Relaxed),
    })
}

#[tauri::command]
fn templates() -> &'static [templates::Template] {
    templates::TEMPLATES
}

fn valid_name(name: &str) -> CmdResult<String> {
    let n = name.trim();
    if n.is_empty() || n.len() > 32 || !n.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_') {
        return Err("名称只能用字母、数字、- 或 _，最多 32 个字符".into());
    }
    Ok(n.to_owned())
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct NewAgent {
    template: String,
    name: String,
    url: Option<String>,
    header_name: Option<String>,
    header_value: Option<String>,
    command: Option<String>,
    cwd: Option<String>,
    /// Local agents only: how many instances to add (name, name-2, name-3 …).
    count: Option<u8>,
    worktree: Option<bool>,
}

#[tauri::command]
async fn add_agent(core: St<'_>, spec: NewAgent) -> CmdResult<Vec<String>> {
    let NewAgent { template, name, url, header_name, header_value, command, cwd, count, worktree } = spec;
    let name = valid_name(&name)?;
    let t = templates::get(&template).ok_or("未知模板")?;
    let row = match t.kind {
        "a2a" => {
            let url = url.filter(|u| u.starts_with("http")).ok_or("URL 需以 http(s) 开头")?;
            let cred = match (header_name.filter(|s| !s.is_empty()), header_value.filter(|s| !s.is_empty())) {
                (Some(n), Some(v)) => Credential::Header { name: n, value: v },
                _ => Credential::None,
            };
            let agent = RemoteAgent::connect(&url, cred.clone()).await.map_err(err)?;
            let credential_ref = if let Credential::Header { name: h, value } = &cred {
                let account = format!("agent/{name}");
                secret_set(&account, &format!("{h}\n{value}"))?;
                Some(account)
            } else { None };
            let row = AgentRow { id: uuid::Uuid::new_v4().to_string(), name: name.clone(), transport: "a2a".into(), endpoint: url, credential_ref, card_json: serde_json::to_string(&agent.card).ok() };
            core.db().upsert_agent(&row).map_err(err)?;
            return Ok(vec![name]);
        }
        kind => {
            let (cmd, args) = if kind == "command" {
                let line = command.unwrap_or_default();
                let mut it = line.split_whitespace().map(str::to_owned);
                (it.next().ok_or("请填写命令")?, it.collect())
            } else {
                (t.command.to_owned(), t.args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
            };
            let n = count.unwrap_or(1).clamp(1, 8) as usize;
            let spec = LocalSpec {
                template: t.id.into(), command: cmd.clone(), args: args.clone(),
                cwd: cwd.unwrap_or_default().trim().into(), description: t.blurb.into(),
                worktree: worktree.unwrap_or(n > 1),
            };
            let endpoint = std::iter::once(cmd).chain(args).collect::<Vec<_>>().join(" ");
            let card = serde_json::to_string(&spec).ok();
            let existing: Vec<String> = core.db().list_agents().map_err(err)?.into_iter().map(|a| a.name).collect();
            let mut names = Vec::with_capacity(n);
            let mut i = 1;
            while names.len() < n {
                let candidate = if i == 1 { name.clone() } else { format!("{name}-{i}") };
                i += 1;
                if n > 1 && existing.contains(&candidate) { continue; }
                names.push(candidate);
            }
            let mut db = core.db();
            for nm in &names {
                db.upsert_agent(&AgentRow { id: uuid::Uuid::new_v4().to_string(), name: nm.clone(), transport: "acp".into(), endpoint: endpoint.clone(), credential_ref: None, card_json: card.clone() }).map_err(err)?;
            }
            names
        }
    };
    Ok(row)
}


#[tauri::command]
async fn remove_agent(core: St<'_>, name: String) -> CmdResult<()> {
    core.hub.drop_agent(&name).await;
    let row = core.agent(&name)?;
    if let Some(c) = row.credential_ref { secret_del(&c); }
    core.db().delete_agent(&name).map_err(err)?;
    Ok(())
}

#[derive(Serialize)]
struct Msg { author: String, body: String, ts: i64 }

#[tauri::command]
fn messages(core: St<'_>, thread: String) -> CmdResult<Vec<Msg>> {
    Ok(core.db().messages(&thread, 200).map_err(err)?.into_iter().map(|m| Msg { author: m.author, body: m.body, ts: m.ts }).collect())
}

#[tauri::command]
fn send(core: St<'_>, thread: String, text: String) -> CmdResult<()> {
    let text = text.trim().to_owned();
    if text.is_empty() { return Ok(()); }
    if core.paused.load(Ordering::Relaxed) { return Err("已全部停止，先点「恢复」".into()); }
    let core = core.inner().clone();
    if let Some(agent) = thread.strip_prefix("dm:") {
        let agent = agent.to_owned();
        tauri::async_runtime::spawn(async move { core.dm(agent, text).await });
    } else if let Some(ch) = thread.strip_prefix("group:") {
        let ch = ch.to_owned();
        tauri::async_runtime::spawn(async move { core.group(ch, "me".into(), text, false).await });
    } else {
        return Err("未知会话".into());
    }
    Ok(())
}

#[tauri::command]
async fn decide_permission(core: St<'_>, agent: String, request_id: String, option_id: Option<String>) -> CmdResult<bool> {
    let d = option_id.map_or(PermissionDecision::Cancel, |option_id| PermissionDecision::Select { option_id });
    Ok(core.hub.decide(&agent, &request_id, d).await)
}

#[tauri::command]
async fn stop_all(core: St<'_>) -> CmdResult<usize> {
    core.paused.store(true, Ordering::Relaxed);
    let n = core.hub.stop_all().await;
    core.db().audit("me", None, "kill_switch", &format!("stopped {n}")).map_err(err)?;
    Ok(n)
}

#[tauri::command]
fn resume(core: St<'_>) {
    core.paused.store(false, Ordering::Relaxed);
}

#[tauri::command]
async fn save_discord_token(core: St<'_>, token: String) -> CmdResult<()> {
    let token = token.trim();
    if token.len() < 50 { return Err("token 看起来不完整".into()); }
    secret_set(DISCORD_KEY, token)?;
    connect_discord(core.inner().clone());
    Ok(())
}

#[tauri::command]
fn discord_connect(core: St<'_>) -> CmdResult<()> {
    if secret_get(DISCORD_KEY).is_none() { return Err("还没有保存 Discord token".into()); }
    connect_discord(core.inner().clone());
    Ok(())
}

#[derive(Serialize)]
struct Channel { guild_id: String, guild: String, id: String, name: String }

#[tauri::command]
async fn discord_channels(core: St<'_>) -> CmdResult<Vec<Channel>> {
    let rest = core.discord_rest().ok_or("Discord 未连接")?;
    Ok(rest.text_channels().await?.into_iter().map(|c| Channel { guild_id: c.guild_id, guild: c.guild_name, id: c.channel_id, name: c.channel_name }).collect())
}

#[tauri::command]
async fn save_group(core: St<'_>, channel_id: String, guild_id: String, name: String, members: Vec<String>, lead: Option<String>) -> CmdResult<()> {
    if members.is_empty() { return Err("至少选一个 agent".into()); }
    let existing = core.db().list_groups().map_err(err)?.into_iter().find(|g| g.id == channel_id).and_then(|g| g.webhook_ref);
    let webhook_ref = match existing {
        Some(r) if secret_get(&r).is_some() => Some(r),
        _ => {
            let rest = core.discord_rest().ok_or("Discord 未连接")?;
            let hook = rest.create_webhook(&channel_id).await?;
            let account = format!("webhook/{channel_id}");
            secret_set(&account, &hook)?;
            Some(account)
        }
    };
    let lead = lead.filter(|l| members.contains(l)).or_else(|| members.first().cloned());
    core.db().upsert_group(&GroupRow { id: channel_id, guild_id, name, lead_agent: lead, webhook_ref, members }).map_err(err)
}

#[tauri::command]
fn delete_group(core: St<'_>, id: String) -> CmdResult<()> {
    secret_del(&format!("webhook/{id}"));
    core.db().delete_group(&id).map_err(err)?;
    Ok(())
}

fn connect_discord(core: Arc<Core>) {
    let Some(token) = secret_get(DISCORD_KEY) else { return };
    let (stop_tx, stop_rx) = watch::channel(false);
    if let Some(old) = core.discord.lock().unwrap_or_else(|p| p.into_inner()).replace(Discord { rest: Rest::new(token.clone()), stop: stop_tx }) {
        let _ = old.stop.send(true);
    }
    core.set_discord_status("连接中…".into());
    let (tx, mut rx) = mpsc::channel::<Incoming>(64);
    let c = core.clone();
    tauri::async_runtime::spawn(disco_discord::run_gateway(token, tx, stop_rx, move |s| c.set_discord_status(s)));
    tauri::async_runtime::spawn(async move {
        while let Some(m) = rx.recv().await {
            let c = core.clone();
            // Each channel message is handled on its own task so one slow agent
            // never blocks other groups.
            tauri::async_runtime::spawn(async move { c.group(m.channel_id, m.author, m.content, true).await });
        }
    });
}

pub fn run() {
    disco_discord::install_crypto();
    tauri::Builder::default()
        .setup(|app| {
            let dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&dir)?;
            let core = Arc::new(Core {
                store: Mutex::new(Store::open(dir.join("disco.db"))?),
                hub: Hub::new(),
                discord: Mutex::new(None),
                discord_status: Mutex::new("未连接".into()),
                paused: AtomicBool::new(false),
                app: app.handle().clone(),
            });
            if secret_get(DISCORD_KEY).is_some() { connect_discord(core.clone()); }
            app.manage(core);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            overview, templates, add_agent, remove_agent, messages, send, decide_permission,
            stop_all, resume, save_discord_token, discord_connect, discord_channels, save_group, delete_group
        ])
        .run(tauri::generate_context!())
        .expect("error while running Disco");
}

#[cfg(test)]
mod tests {
    use super::mentions;

    #[test]
    fn mention_order_and_self_exclusion() {
        let m = vec!["planner".to_string(), "coder".to_string(), "reviewer".to_string()];
        assert_eq!(mentions("@Reviewer 看下，@coder 先写", &m, None), ["reviewer", "coder"]);
        assert_eq!(mentions("@coder 我自己来 @planner", &m, Some("coder")), ["planner"]);
        assert!(mentions("没有提到谁", &m, None).is_empty());
        let m2 = vec!["coder".to_string(), "coder-2".to_string(), "coder-3".to_string()];
        assert_eq!(mentions("@coder-2 和 @coder 分头做", &m2, None), ["coder-2", "coder"]);
        assert_eq!(mentions("@all 并行开工", &m2, Some("coder")), ["coder-2", "coder-3"]);
    }
}
