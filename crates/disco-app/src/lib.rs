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
// Settings keys (SQLite; not secret).
const SET_TOKEN_SAVED: &str = "discord_token_saved";
/// Extra Discord user ids (comma-separated) allowed to drive agents, besides the bot owner.
const SET_ALLOW: &str = "discord_allow";
/// Guild where each agent gets its own channel.
const SET_HOME_GUILD: &str = "home_guild";
/// `agent_channel/<agent>` -> Discord channel id of that agent's 1:1 channel.
const SET_AGENT_CH: &str = "agent_channel/";
const AGENT_CATEGORY: &str = "Disco Agents";
/// Hand-off rounds and total agent turns allowed per human message in a group.
const MAX_WAVES: usize = 4;
const MAX_TURNS: usize = 12;
const TRANSCRIPT: u32 = 20;

type CmdResult<T> = Result<T, String>;

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

// All secrets live in ONE Keychain item ("vault", a JSON map) that is read once
// per launch and cached. With an ad-hoc signed app macOS asks for permission per
// item, so one item means at most one prompt (choose "Always Allow").
const VAULT_ACCOUNT: &str = "vault";
static VAULT: Mutex<Option<std::collections::HashMap<String, String>>> = Mutex::new(None);

fn keychain(account: &str) -> Option<keyring::Entry> {
    keyring::Entry::new(KEYCHAIN_SERVICE, account).ok()
}

fn with_vault<R>(f: impl FnOnce(&mut std::collections::HashMap<String, String>) -> R) -> R {
    let mut guard = VAULT.lock().unwrap_or_else(|p| p.into_inner());
    let map = guard.get_or_insert_with(|| {
        let mut m: std::collections::HashMap<String, String> = keychain(VAULT_ACCOUNT)
            .and_then(|e| e.get_password().ok())
            .and_then(|j| serde_json::from_str(&j).ok())
            .unwrap_or_default();
        // One-time move of the bot token from the per-item layout used before v0.4.1.
        if !m.contains_key("migrated") {
            if let Some(t) = keychain(DISCORD_KEY).and_then(|e| e.get_password().ok()) { m.insert(DISCORD_KEY.into(), t); }
            m.insert("migrated".into(), "1".into());
            let _ = persist_vault(&m);
        }
        m
    });
    f(map)
}

fn persist_vault(m: &std::collections::HashMap<String, String>) -> CmdResult<()> {
    let json = serde_json::to_string(m).map_err(err)?;
    keychain(VAULT_ACCOUNT).ok_or("钥匙串不可用")?.set_password(&json).map_err(|e| format!("写入钥匙串失败：{e}"))
}

fn secret_get(account: &str) -> Option<String> {
    with_vault(|m| m.get(account).cloned())
}

fn secret_set(account: &str, value: &str) -> CmdResult<()> {
    with_vault(|m| {
        m.insert(account.to_owned(), value.to_owned());
        persist_vault(m)
    })
}

fn secret_del(account: &str) {
    with_vault(|m| {
        if m.remove(account).is_some() { let _ = persist_vault(m); }
    });
}

struct Discord {
    rest: Rest,
    stop: watch::Sender<bool>,
}

fn parse_ids(s: &str) -> Vec<String> {
    s.split([',', ' ', '\n', '，']).filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())).map(str::to_owned).collect()
}

pub struct Core {
    store: Mutex<Store>,
    hub: Hub,
    discord: Mutex<Option<Discord>>,
    discord_status: Mutex<String>,
    /// Discord user ids of the bot application's owner(s); always allowed.
    owners: Mutex<Vec<String>>,
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

    /// 1:1 chat. The same thread is mirrored to the agent's own Discord channel
    /// (when there is one), whichever side the message came from.
    async fn dm(&self, agent: String, author: String, text: String, from_discord: bool) {
        let thread = format!("dm:{agent}");
        self.post_message(&thread, &author, &text);
        let mirror = self.agent_channel(&agent).and_then(|ch| Some((self.discord_rest()?, ch)));
        // Mirror in the background: the agent must not wait on Discord.
        if !from_discord && let Some((d, ch)) = mirror.clone() {
            let line = format!("**我**：{text}");
            tauri::async_runtime::spawn(async move { let _ = d.post(&ch, &line).await; });
        }
        match self.turn(&thread, &agent, &thread, &text).await {
            Ok(answer) => {
                let answer = answer.trim();
                self.post_message(&thread, &agent, answer);
                if let Some((d, ch)) = &mirror && let Some(hook) = secret_get(&format!("webhook/{ch}"))
                    && let Err(e) = d.post_as(&hook, &agent, None, answer).await
                {
                    self.post_message(&thread, "system", &format!("Discord 发送失败：{e}"));
                }
            }
            Err(e) => self.post_message(&thread, "system", &format!("{agent} 出错：{e}")),
        }
    }

    /// Start agents and their DM sessions ahead of use (all of them, or `only`).
    fn warm(self: &Arc<Self>, only: Option<Vec<String>>) {
        let core = self.clone();
        tauri::async_runtime::spawn(async move {
            let Ok(rows) = core.db().list_agents() else { return };
            let runs = rows.into_iter().filter(|r| only.as_ref().is_none_or(|o| o.contains(&r.name))).map(|r| {
                let core = core.clone();
                async move { core.hub.warm(&r, &secret_get, &format!("dm:{}", r.name)).await }
            });
            futures::future::join_all(runs).await;
        });
    }

    fn agent_channel(&self, agent: &str) -> Option<String> {
        self.db().setting(&format!("{SET_AGENT_CH}{agent}"))
    }

    fn allowed(&self, discord_user: &str) -> bool {
        self.owners.lock().unwrap_or_else(|p| p.into_inner()).iter().any(|o| o == discord_user)
            || parse_ids(&self.db().setting(SET_ALLOW).unwrap_or_default()).iter().any(|o| o == discord_user)
    }

    /// Give every agent (or just `only`) its own channel + webhook in the home guild.
    async fn sync_agent_channels(&self, only: Option<&[String]>) -> CmdResult<usize> {
        let rest = self.discord_rest().ok_or("Discord 未连接")?;
        let guild = self.db().setting(SET_HOME_GUILD).ok_or("还没选主服务器")?;
        let agents = self.db().list_agents().map_err(err)?;
        let mut n = 0;
        for a in agents.iter().filter(|a| only.is_none_or(|o| o.contains(&a.name))) {
            let spec: LocalSpec = a.card_json.as_deref().and_then(|j| serde_json::from_str(j).ok()).unwrap_or_default();
            let topic = if a.transport == "a2a" { format!("A2A agent · {}", a.endpoint) } else { spec.description.clone() };
            let (ch, created) = rest.ensure_channel(&guild, AGENT_CATEGORY, &a.name, &topic).await?;
            let hook_key = format!("webhook/{ch}");
            if secret_get(&hook_key).is_none() {
                secret_set(&hook_key, &rest.create_webhook(&ch).await?)?;
            }
            self.db().set_setting(&format!("{SET_AGENT_CH}{}", a.name), &ch).map_err(err)?;
            if created && let Some(hook) = secret_get(&hook_key) {
                let _ = rest.post_as(&hook, &a.name, None, &format!("我是 **{}**，已加入 Disco。在这个频道直接对我说话即可；在群聊频道里用 @{} 叫我。", a.name, a.name)).await;
            }
            n += 1;
        }
        Ok(n)
    }

    /// After the gateway connects: learn the owners, pick a home guild if there is
    /// only one, and make sure every agent has its channel.
    async fn on_discord_connected(&self) {
        let Some(rest) = self.discord_rest() else { return };
        if let Ok(ids) = rest.owner_ids().await {
            *self.owners.lock().unwrap_or_else(|p| p.into_inner()) = ids;
        }
        let has_home = self.db().setting(SET_HOME_GUILD).is_some();
        if !has_home && let Ok(gs) = rest.guilds().await && gs.len() == 1 {
            let _ = self.db().set_setting(SET_HOME_GUILD, &gs[0].id);
        }
        if let Err(e) = self.sync_agent_channels(None).await {
            let _ = self.db().audit("disco", None, "discord.sync", &e);
        }
    }

    /// Group routing: @mentioned members answer, otherwise the lead does.
    /// An answer that @mentions another member hands off to it, in waves (see MAX_WAVES / MAX_TURNS).
    async fn group(&self, channel: String, author: String, text: String, from_discord: bool) {
        let Some(g) = self.db().list_groups().ok().and_then(|gs| gs.into_iter().find(|g| g.id == channel)) else { return };
        let thread = format!("group:{channel}");
        self.post_message(&thread, &author, &text);
        if !from_discord && let Some(d) = self.discord_rest() {
            let (ch, line) = (channel.clone(), format!("**{author}**：{text}"));
            tauri::async_runtime::spawn(async move { let _ = d.post(&ch, &line).await; });
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
                                && let Err(e) = d.post_as(&hook, agent, None, &answer).await
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
        discord_token: core.db().setting(SET_TOKEN_SAVED).is_some(),
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
    let names = add_agent_rows(&core, spec).await?;
    core.inner().warm(Some(names.clone()));
    if core.discord_rest().is_some() {
        let (c, n) = (core.inner().clone(), names.clone());
        tauri::async_runtime::spawn(async move {
            if let Err(e) = c.sync_agent_channels(Some(&n)).await {
                c.post_message(&format!("dm:{}", n[0]), "system", &format!("没能在 Discord 建频道：{e}"));
            }
        });
    }
    Ok(names)
}

async fn add_agent_rows(core: &Core, spec: NewAgent) -> CmdResult<Vec<String>> {
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
    if let Some(ch) = core.agent_channel(&name) {
        let _ = core.db().set_setting(&format!("{SET_AGENT_CH}{name}"), "");
        secret_del(&format!("webhook/{ch}"));
        if let Some(d) = core.discord_rest() { let _ = d.delete_channel(&ch).await; }
    }
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
        tauri::async_runtime::spawn(async move { core.dm(agent, "me".into(), text, false).await });
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
    core.inner().warm(None);
}

#[tauri::command]
async fn save_discord_token(core: St<'_>, token: String) -> CmdResult<()> {
    let token = token.trim().trim_start_matches("Bot ").to_owned();
    if token.len() < 50 { return Err("token 看起来不完整".into()); }
    // Validate before saving so a typo shows up here, not as a silent failure.
    Rest::new(token.clone()).me().await?;
    secret_set(DISCORD_KEY, &token)?;
    core.db().set_setting(SET_TOKEN_SAVED, "1").map_err(err)?;
    connect_discord(core.inner().clone());
    Ok(())
}

#[derive(Serialize)]
struct DiscordSettings { home_guild: Option<String>, guilds: Vec<Channel>, allow: String, invite_url: Option<String> }

#[tauri::command]
async fn discord_settings(core: St<'_>, home_guild: Option<String>, allow: Option<String>) -> CmdResult<DiscordSettings> {
    if let Some(a) = allow { core.db().set_setting(SET_ALLOW, &parse_ids(&a).join(", ")).map_err(err)?; }
    let changed_home = home_guild.is_some();
    if let Some(g) = home_guild { core.db().set_setting(SET_HOME_GUILD, &g).map_err(err)?; }
    if changed_home {
        let c = core.inner().clone();
        tauri::async_runtime::spawn(async move { let _ = c.sync_agent_channels(None).await; });
    }
    let (guilds, invite_url) = match core.discord_rest() {
        Some(r) => {
            let gs = r.guilds().await.unwrap_or_default();
            let id = r.me().await.ok().map(|(id, _)| id);
            (gs.into_iter().map(|g| Channel { guild_id: g.id.clone(), guild: g.name, id: g.id, name: String::new() }).collect(),
             id.map(|id| format!("https://discord.com/oauth2/authorize?client_id={id}&scope=bot&permissions={}", disco_discord::BOT_PERMISSIONS)))
        }
        None => (Vec::new(), None),
    };
    let db = core.db();
    Ok(DiscordSettings { home_guild: db.setting(SET_HOME_GUILD), guilds, allow: db.setting(SET_ALLOW).unwrap_or_default(), invite_url })
}

#[tauri::command]
async fn discord_sync(core: St<'_>) -> CmdResult<usize> {
    core.sync_agent_channels(None).await
}

/// Opens Discord pages (bot invite / developer portal) in the default browser.
#[tauri::command]
fn open_url(url: String) -> CmdResult<()> {
    if !(url.starts_with("https://discord.com/") || url.starts_with("https://github.com/Cosmoto-jian/")) {
        return Err("不允许打开该链接".into());
    }
    std::process::Command::new("/usr/bin/open").arg(&url).spawn().map_err(err)?;
    Ok(())
}

#[derive(Serialize)]
struct Check { name: String, ok: bool, detail: String }

/// Step-by-step health check of everything Disco depends on.
#[tauri::command]
async fn diagnose(core: St<'_>) -> CmdResult<Vec<Check>> {
    let mut out = Vec::new();
    let mut add = |name: &str, r: Result<String, String>| {
        let (ok, detail) = match r { Ok(d) => (true, d), Err(e) => (false, e) };
        out.push(Check { name: name.into(), ok, detail });
    };
    add("网络代理", Ok(disco_discord::proxy_url().map_or("未检测到，直连".into(), |p| format!("使用系统代理 {p}"))));
    add("macOS 钥匙串", with_vault(|m| persist_vault(m)).map(|_| "可读写（所有密钥存在一条钥匙串项里）".into()));
    let token = secret_get(DISCORD_KEY);
    add("Discord Bot token", token.as_ref().map(|_| "已保存".into()).ok_or("还没保存，或钥匙串拒绝访问".into()));
    if let Some(token) = token {
        let rest = Rest::new(token.clone());
        let me = rest.me().await;
        add("Discord API", me.as_ref().map(|(_, n)| format!("Bot：{n}")).map_err(Clone::clone));
        add("Discord 实时连接", disco_discord::gateway_check(token, std::time::Duration::from_secs(15)).await
            .map(|r| format!("READY，用时 {} ms，{} 个服务器", r.ready_after.as_millis(), r.guilds)));
        if let Ok((bot_id, _)) = me {
            let home = core.db().setting(SET_HOME_GUILD);
            match home {
                Some(g) => add("Bot 权限", rest.bot_permissions(&g, &bot_id).await.and_then(|bits| {
                    let need = [(1u64 << 4, "管理频道"), (1 << 29, "管理 Webhook"), (1 << 11, "发送消息"), (1 << 10, "查看频道")];
                    let missing: Vec<&str> = need.iter().filter(|(b, _)| bits & b == 0).map(|(_, n)| *n).collect();
                    if missing.is_empty() { Ok("权限齐全".into()) } else { Err(format!("缺少：{}，请点「重新授权 Bot」", missing.join("、"))) }
                })),
                None => add("主服务器", Err("还没选主服务器".into())),
            }
        }
    }
    let path = core.hub.user_path.clone();
    add("Node.js / npx", hub::which("npx", &path).map(|p| p.display().to_string()).ok_or("找不到 npx：Claude Code、Codex、pi 需要先安装 Node.js".into()));
    for a in core.db().list_agents().map_err(err)?.into_iter().filter(|a| a.transport == "acp") {
        let cmd = a.endpoint.split_whitespace().next().unwrap_or_default().to_owned();
        add(&format!("agent {}", a.name), hub::which(&cmd, &path).map(|_| format!("命令 {cmd} 可用")).ok_or(format!("找不到命令 {cmd}")));
    }
    Ok(out)
}

#[tauri::command]
fn discord_connect(core: St<'_>) -> CmdResult<()> {
    if core.db().setting(SET_TOKEN_SAVED).is_none() { return Err("还没有保存 Discord token".into()); }
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
    let Some(token) = secret_get(DISCORD_KEY) else {
        core.set_discord_status("读不到 token：请在设置里重新保存（或在钥匙串弹窗中允许）".into());
        return;
    };
    let (stop_tx, stop_rx) = watch::channel(false);
    if let Some(old) = core.discord.lock().unwrap_or_else(|p| p.into_inner()).replace(Discord { rest: Rest::new(token.clone()), stop: stop_tx }) {
        let _ = old.stop.send(true);
    }
    let (tx, mut rx) = mpsc::channel::<Incoming>(64);
    let c = core.clone();
    let online = Arc::new(AtomicBool::new(false));
    let o = online.clone();
    tauri::async_runtime::spawn(disco_discord::run_gateway(token, tx, stop_rx, move |s| {
        let now_online = s.starts_with("在线");
        if now_online && !o.swap(true, Ordering::Relaxed) {
            let c2 = c.clone();
            tauri::async_runtime::spawn(async move { c2.on_discord_connected().await });
        }
        if !now_online { o.store(false, Ordering::Relaxed); }
        c.set_discord_status(s);
    }));
    tauri::async_runtime::spawn(async move {
        while let Some(m) = rx.recv().await {
            // Only the bot's owner(s) and explicitly allowed users may drive agents:
            // anyone else in the guild could otherwise run tools on this Mac.
            if !core.allowed(&m.author_id) {
                let _ = core.db().audit(&format!("discord:{}", m.author_id), None, "discord.ignored", "sender not allowed");
                continue;
            }
            let c = core.clone();
            // The " (Discord)" suffix keeps a Discord display name from
            // impersonating the local user ("me") or an agent.
            let author = format!("{} (Discord)", m.author);
            let agent = c.db().settings_with_prefix(SET_AGENT_CH).unwrap_or_default().into_iter().find(|(_, ch)| *ch == m.channel_id).map(|(a, _)| a);
            // Each message runs on its own task so one slow agent never blocks others.
            tauri::async_runtime::spawn(async move {
                match agent {
                    Some(a) => c.dm(a, author, m.content, true).await,
                    None => c.group(m.channel_id, author, m.content, true).await,
                }
            });
        }
    });
}

// ---------------------------------------------------------------- self-update
// Release flow: CI attaches Disco_<ver>_universal.app.tar.gz to the GitHub
// release; scripts/publish_update.py signs it with the offline updater key and
// commits updates/latest.json, which every installed copy polls.

#[derive(Serialize, Clone)]
struct UpdateInfo { current: String, version: Option<String>, notes: String }

async fn find_update(app: &AppHandle) -> CmdResult<UpdateInfo> {
    use tauri_plugin_updater::UpdaterExt;
    let found = app.updater().map_err(err)?.check().await.map_err(err)?;
    Ok(UpdateInfo {
        current: app.package_info().version.to_string(),
        version: found.as_ref().map(|u| u.version.clone()),
        notes: found.and_then(|u| u.body).unwrap_or_default(),
    })
}

#[tauri::command]
async fn check_update(app: AppHandle) -> CmdResult<UpdateInfo> {
    find_update(&app).await
}

#[tauri::command]
async fn install_update(core: St<'_>, app: AppHandle) -> CmdResult<()> {
    use tauri_plugin_updater::UpdaterExt;
    let update = app.updater().map_err(err)?.check().await.map_err(err)?.ok_or("已经是最新版本")?;
    let _ = core.db().audit("me", None, "app.update", &update.version);
    update.download_and_install(|_, _| {}, || {}).await.map_err(err)?;
    core.hub.stop_all().await;
    app.restart();
}

pub fn run() {
    disco_discord::install_crypto();
    tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(|app| {
            let dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&dir)?;
            let core = Arc::new(Core {
                store: Mutex::new(Store::open(dir.join("disco.db"))?),
                hub: Hub::new(),
                discord: Mutex::new(None),
                discord_status: Mutex::new("未连接".into()),
                owners: Mutex::default(),
                paused: AtomicBool::new(false),
                app: app.handle().clone(),
            });
            // Installs from before v0.4 kept only the Keychain entry.
            if core.db().setting(SET_TOKEN_SAVED).is_none() && secret_get(DISCORD_KEY).is_some() {
                let _ = core.db().set_setting(SET_TOKEN_SAVED, "1");
            }
            if core.db().setting(SET_TOKEN_SAVED).is_some() { connect_discord(core.clone()); }
            core.warm(None);
            app.manage(core);
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                // Quietly check a few seconds after launch; errors (offline) are ignored.
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                if let Ok(info) = find_update(&handle).await && info.version.is_some() {
                    let _ = handle.emit("update-available", info);
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            overview, templates, add_agent, remove_agent, messages, send, decide_permission,
            stop_all, resume, save_discord_token, discord_connect, discord_settings, discord_sync, diagnose, open_url, discord_channels, save_group, delete_group,
            check_update, install_update
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
