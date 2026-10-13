//! Disco desktop shell (Tauri 2). The webview only calls the typed commands
//! below; secrets live in the macOS Keychain and never reach SQLite or the UI.
#![forbid(unsafe_code)]

mod hub;
mod inbox;
mod mcp;
mod memory;
mod team;
pub use mcp::stdio_proxy as mcp_stdio;
mod share;
mod files;
mod tailnet;
mod templates;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};

use disco_a2a::{Credential, RemoteAgent};
use disco_discord::{Incoming, Rest};
use disco_store::{AgentRow, GroupRow, Store};
use disco_types::AgentEvent;
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
/// Chosen avatar (`<form>.<palette>`, see ui/app.js); empty = generated from the name.
const SET_AVATAR: &str = "avatar/";
const AGENT_CATEGORY: &str = "Disco Agents";
const GROUP_CATEGORY: &str = "Disco 群聊";
/// Memory notes live in settings as `mem/<scope>/<millis>`; scope is `all` or an agent name.
pub(crate) const MEM: &str = "mem/";
/// A line an agent starts with this is saved to its memory.
pub(crate) const MEM_TAG: &str = "【记住】";
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
        let mut m: std::collections::HashMap<String, String> = vault_read()
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
    vault_write(&serde_json::to_string(m).map_err(err)?)
}

#[cfg(not(windows))]
fn vault_read() -> Option<String> { keychain(VAULT_ACCOUNT)?.get_password().ok() }

#[cfg(not(windows))]
fn vault_write(json: &str) -> CmdResult<()> {
    keychain(VAULT_ACCOUNT).ok_or("钥匙串不可用")?.set_password(json).map_err(|e| format!("写入钥匙串失败：{e}"))
}

// Windows Credential Manager caps one credential at 2560 bytes, so the vault is
// stored in pieces: `vault`, `vault.1`, `vault.2`, … (no per-item prompts there).
#[cfg(windows)]
const VAULT_PIECE: usize = 1000;

#[cfg(windows)]
fn vault_read() -> Option<String> {
    let mut out = keychain(VAULT_ACCOUNT)?.get_password().ok()?;
    for i in 1.. {
        match keychain(&format!("{VAULT_ACCOUNT}.{i}")).and_then(|e| e.get_password().ok()) { Some(p) => out.push_str(&p), None => break }
    }
    Some(out)
}

#[cfg(windows)]
fn vault_write(json: &str) -> CmdResult<()> {
    let chars: Vec<char> = json.chars().collect();
    let pieces: Vec<String> = chars.chunks(VAULT_PIECE).map(|c| c.iter().collect()).collect();
    for (i, p) in pieces.iter().enumerate() {
        let acct = if i == 0 { VAULT_ACCOUNT.to_owned() } else { format!("{VAULT_ACCOUNT}.{i}") };
        keychain(&acct).ok_or("凭据管理器不可用")?.set_password(p).map_err(|e| format!("写入 Windows 凭据管理器失败：{e}"))?;
    }
    for i in pieces.len().max(1).. {
        let Some(e) = keychain(&format!("{VAULT_ACCOUNT}.{i}")) else { break };
        if e.delete_credential().is_err() { break; }
    }
    Ok(())
}

/// A webhook this Disco created (its agents speaking), by webhook id.
fn own_webhook(id: &str) -> bool {
    with_vault(|m| m.iter().any(|(k, v)| k.starts_with("webhook/") && v.split('\n').next() == Some(id)))
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
    /// `agent|context` pairs whose session already got the memory preamble.
    primed: Mutex<std::collections::HashSet<String>>,
    /// `agent|thread` -> id of the newest group message that agent's session has seen,
    /// so later turns send only what is new instead of the whole transcript again.
    seen: Mutex<HashMap<String, i64>>,
    /// Everything waiting on the user (see inbox.rs).
    inbox: Arc<inbox::Inbox>,
    /// Recent hand-offs from other Discos' agents, per channel (see `foreign_turn_ok`).
    foreign: Mutex<HashMap<String, Vec<i64>>>,
    /// Discord display name -> user id of people seen in group channels (for real @pings).
    people: Mutex<HashMap<String, String>>,
    /// Warnings already shown once (`group|name`).
    warned: Mutex<std::collections::HashSet<String>>,
    /// Team runs per group (see team.rs).
    teams: team::Teams,
    /// The Tailscale share server (see share.rs).
    server: Mutex<share::Server>,
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

    fn discord_status(&self) -> String {
        self.discord_status.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    fn set_discord_status(&self, s: String) {
        *self.discord_status.lock().unwrap_or_else(|p| p.into_inner()) = s.clone();
        let _ = self.app.emit("discord-status", s);
    }

    /// One agent turn: prompt, stream to the UI, return the answer.
    async fn turn(&self, thread: &str, agent: &str, context: &str, prompt: &str) -> CmdResult<String> {
        self.turn_with(thread, agent, context, prompt, |_| true).await
    }

    /// Like `turn`, with `tap` seeing every event first; it returns false to keep
    /// an event away from the UI.
    async fn turn_with(&self, thread: &str, agent: &str, context: &str, prompt: &str, tap: impl Fn(&AgentEvent) -> bool + Send + Sync + 'static) -> CmdResult<String> {
        if self.paused.load(Ordering::Relaxed) {
            return Err("已全部停止".into());
        }
        let row = self.agent(agent)?;
        // Memory goes only to agents running on this Mac, never to a remote one.
        let first = row.transport == "acp" && self.memory_on(context) && self.primed.lock().unwrap_or_else(|p| p.into_inner()).insert(format!("{agent}|{context}"));
        let with_mem;
        let prompt = if first { with_mem = format!("{}{prompt}", self.memory_preamble(agent, context)); with_mem.as_str() } else { prompt };
        // Team members work behind the scenes: no bubbles, only the activity strip
        // (permission prompts still reach the inbox).
        let hidden = context.starts_with("team:");
        if !hidden { let _ = self.app.emit("typing", TypingEvt { thread: thread.into(), agent: agent.into(), on: true }); }
        let app = self.app.clone();
        let (t, a) = (thread.to_owned(), agent.to_owned());
        let asked = Mutex::new(Vec::new());
        let res = self
            .hub
            .ask(&row, &secret_get, context, prompt, |ev| {
                if !tap(ev) { return; }
                if let AgentEvent::PermissionAsked { request_id, title, options } = ev {
                    self.inbox_permission(&t, &a, request_id, title, options);
                    asked.lock().unwrap_or_else(|p| p.into_inner()).push(request_id.clone());
                }
                if hidden {
                    if let AgentEvent::Tool { title, .. } = ev && !title.is_empty() { self.team_activity(&t, &a, title); }
                    return;
                }
                let _ = app.emit("stream", StreamEvt { thread: t.clone(), agent: a.clone(), event: ev.clone() });
            })
            .await;
        self.inbox_expire(&asked.into_inner().unwrap_or_else(|p| p.into_inner()));
        if !hidden { let _ = self.app.emit("typing", TypingEvt { thread: thread.into(), agent: agent.into(), on: false }); }
        if matches!(&res, Err(e) if e.contains("进程已退出")) { self.forget_sessions(Some(agent)); }
        let _ = self.db().audit("disco", Some(agent), "prompt", if res.is_ok() { "ok" } else { "error" });
        res
    }

    /// Save every `【记住】` line of an agent's answer to its memory (unless the chat has memory off).
    fn remember_from(&self, agent: &str, thread: &str, answer: &str) {
        if !self.memory_on(thread) { return; }
        for l in answer.lines().filter_map(|l| l.trim().strip_prefix(MEM_TAG)) {
            self.remember(agent, l);
        }
    }

    /// `记住 …` / `/remember …` from the user: save it, no agent turn. Returns true if handled.
    fn user_remember(&self, thread: &str, scope: &str, text: &str) -> bool {
        let t = text.trim();
        let Some(rest) = ["记住：", "记住:", "记住", "/remember ", "/记住 "].iter().find_map(|p| t.strip_prefix(p)) else { return false };
        let rest = rest.trim();
        if rest.is_empty() { return false; }
        if !self.memory_on(thread) {
            self.post_message(thread, "system", "这个对话的记忆已关闭，没有保存。打开顶部的「记忆」开关后再说一次。");
            return true;
        }
        self.remember(scope, rest);
        self.post_message(thread, "system", &format!("已记住{}：{rest}", if scope == "all" { "（所有 agent）" } else if scope.starts_with("group-") { "（本群）" } else { "" }));
        true
    }

    /// 1:1 chat. The same thread is mirrored to the agent's own Discord channel
    /// (when there is one), whichever side the message came from.
    async fn dm(&self, agent: String, author: String, text: String, from_discord: bool) {
        let thread = format!("dm:{agent}");
        self.post_message(&thread, &author, &text);
        if self.user_remember(&thread, &agent, &text) { return; }
        let mirror = self.agent_channel(&agent).and_then(|ch| Some((self.discord_rest()?, ch)));
        // Mirror in the background: the agent must not wait on Discord.
        if !from_discord && let Some((d, ch)) = mirror.clone() {
            let line = format!("**我**：{text}");
            tauri::async_runtime::spawn(async move { let _ = d.post(&ch, &line).await; });
        }
        match self.turn(&thread, &agent, &thread, &text).await {
            Ok(answer) => {
                let answer = answer.trim();
                self.remember_from(&agent, &thread, answer);
                self.post_message(&thread, &agent, answer);
                if let Some((d, ch)) = &mirror && let Some(hook) = secret_get(&format!("webhook/{ch}"))
                    && let Err(e) = d.post_as(&hook, &agent, None, answer).await
                {
                    self.post_message(&thread, "system", &format!("Discord 发送失败：{e}"));
                }
            }
            Err(e) => self.post_message(&thread, "system", &fail_line(&agent, &e)),
        }
    }

    /// Start agents and their DM sessions ahead of use (all of them, or `only`).
    fn warm(self: &Arc<Self>, only: Option<Vec<String>>) {
        let core = self.clone();
        tauri::async_runtime::spawn(async move {
            let Ok(rows) = core.db().list_agents() else { return };
            let groups = core.db().list_groups().unwrap_or_default();
            // Agents just added are also self-checked: a start-up failure shows in their chat now,
            // with a hint, instead of on the first message.
            let check = only.is_some();
            let runs = rows.into_iter().filter(|r| only.as_ref().is_none_or(|o| o.contains(&r.name))).map(|r| {
                let core = core.clone();
                let ctxs: Vec<String> = std::iter::once(format!("dm:{}", r.name))
                    .chain(groups.iter().filter(|g| g.members.contains(&r.name)).map(|g| format!("group:{}", g.id))).collect();
                async move {
                    for c in ctxs {
                        if let Err(e) = core.hub.warm(&r, &secret_get, &c).await {
                            if check { core.post_message(&format!("dm:{}", r.name), "system", &fail_line(&r.name, &e)); }
                            break;
                        }
                    }
                }
            });
            futures::future::join_all(runs).await;
        });
    }

    fn agent_channel(&self, agent: &str) -> Option<String> {
        self.db().setting(&format!("{SET_AGENT_CH}{agent}"))
    }

    /// Who wrote a Discord message, as Disco records it, and its text; None to ignore it.
    /// - A group channel is the group: anyone who can post there takes part (Discord's
    ///   channel permissions decide who), and approvals still only reach this Disco's owner.
    /// - An agent's own channel stays limited to the owner and the allow list.
    /// - Webhooks are other Discos' agents (ours are skipped); a bot account is another
    ///   Disco relaying its owner's words as `**Name**：text`.
    fn speaker(&self, m: &Incoming, group: Option<&GroupRow>, agent_channel: bool) -> Option<(String, String)> {
        if let Some(hook) = &m.webhook_id {
            let g = group?;
            if own_webhook(hook) { return None; }
            if g.members.contains(&m.author) && self.warned.lock().unwrap_or_else(|p| p.into_inner()).insert(format!("{}|{}", g.id, m.author)) {
                self.post_message(&format!("group:{}", g.id), "system", &format!("频道里另一个 Disco 也有叫 {} 的 agent，@{} 会同时叫到两边。建议其中一方改名。", m.author, m.author));
            }
            return Some((m.author.clone(), m.content.clone()));
        }
        if m.bot {
            group?;
            let relayed = m.content.strip_prefix("**").and_then(|r| r.split_once("**："));
            return Some(match relayed {
                Some((name, text)) if !name.is_empty() => (format!("{name}{HUMAN_TAG}"), text.to_owned()),
                _ => (format!("{}{HUMAN_TAG}", m.author), m.content.clone()),
            });
        }
        let is_owner = self.owners.lock().unwrap_or_else(|p| p.into_inner()).contains(&m.author_id);
        if !is_owner && !(group.is_some() && !agent_channel) && !self.allowed(&m.author_id) {
            let _ = self.db().audit(&format!("discord:{}", m.author_id), None, "discord.ignored", "sender not allowed");
            return None;
        }
        self.people.lock().unwrap_or_else(|p| p.into_inner()).insert(m.author.clone(), m.author_id.clone());
        // The tag keeps a Discord display name from passing for the owner ("me") or an agent.
        Some((if is_owner { "me".into() } else { format!("{}{HUMAN_TAG}", m.author) }, m.content.clone()))
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
            // Agents address the owner by their computer name: make that a real ping too.
            if let Some(id) = ids.first() { self.people.lock().unwrap_or_else(|p| p.into_inner()).insert(share::owner_name(), id.clone()); }
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

    /// Group routing. The rules (one line each, see `group_help`):
    /// - an @mentioned agent of this Disco answers at once (several: concurrently);
    /// - a message that @s only people wakes no agent;
    /// - no @ at all: the lead answers while the owner is the only person in the group,
    ///   nobody once other people (or other people's agents) take part;
    /// - every agent answer starts with @ whoever it answers; an @agent in it hands off
    ///   (bounded by MAX_WAVES / MAX_TURNS), an @person ends the chain.
    ///
    /// Agents of other Discos in the same Discord channel answer through their own Disco.
    async fn group(&self, channel: String, author: String, text: String, from_discord: bool) {
        let Some(g) = self.db().list_groups().ok().and_then(|gs| gs.into_iter().find(|g| g.id == channel)) else { return };
        let thread = format!("group:{channel}");
        let owner = author == "me";
        // A member agent posting through its Disco tools: only the members it @s answer.
        let by_agent = g.members.contains(&author);
        let human = is_human(&author);
        self.post_message(&thread, &author, &text);
        if owner && self.user_remember(&thread, &format!("group-{channel}"), &text) { return; }
        if !from_discord && !channel.starts_with("local-") && let Some(d) = self.discord_rest() {
            let hook = if by_agent { g.webhook_ref.as_deref().and_then(secret_get) } else { None };
            let shown = if owner { share::owner_name() } else { author.clone() };
            let (ch, body) = (channel.clone(), self.with_pings(&text));
            tauri::async_runtime::spawn(async move {
                let _ = match hook {
                    Some(h) => d.post_as(&h, &shown, None, &body).await,
                    None => d.post(&ch, &format!("**{shown}**：{body}")).await,
                };
            });
        }
        if owner && let Some(task) = team::command(&text) {
            self.team(&g, task).await;
            return;
        }
        if human && self.group_command(&g, &thread, &text).await { return; }
        let roster = self.roster(&g, &thread);
        let multi = roster.multi();
        let mut wave: Vec<(String, String)> = mentions(&text, &g.members, by_agent.then_some(author.as_str()))
            .into_iter().map(|a| (a, author.clone())).collect();
        if wave.is_empty() && human && !multi && let Some(lead) = g.lead_agent.clone().or_else(|| g.members.first().cloned()) {
            wave.push((lead, author.clone()));
        }
        // Another Disco's agent handing off to ours: rate-limited per channel, so two
        // Discos can never ping-pong forever.
        if !human && !by_agent && !wave.is_empty() && !self.foreign_turn_ok(&channel) {
            self.post_message(&thread, "system", "其他 Disco 的 agent 在这个群里触发得太频繁，暂停自动接力 10 分钟；有人 @ 时照常回答。");
            return;
        }
        let text = &text;
        let (mut waves, mut turns) = (0, 0);
        while !wave.is_empty() && waves < MAX_WAVES && !self.paused.load(Ordering::Relaxed) {
            waves += 1;
            wave.truncate(MAX_TURNS.saturating_sub(turns));
            turns += wave.len();
            let runs = wave.iter().map(|(agent, by)| {
                let thread = &thread;
                let g = &g;
                let roster = &roster;
                let first = waves == 1;
                async move {
                    // `@agent /command` from a person runs that agent's own command.
                    let slash = if first && human { slash_for(text, agent) } else { None };
                    if let Some(cmd) = &slash && !owner && restricted(cmd) {
                        self.post_message(thread, "system", &format!("{cmd} 会改变 {agent} 的状态，只有它的主人能用。"));
                        return Vec::new();
                    }
                    let prompt = slash.unwrap_or_else(|| self.group_prompt(g, thread, agent, by, roster));
                    match self.turn(thread, agent, thread, &prompt).await {
                        Ok(answer) => {
                            let answer = answer.trim();
                            if answer.is_empty() { return Vec::new(); }
                            let answer = if multi { lead_mention(answer, &display(by)) } else { answer.to_owned() };
                            self.remember_from(agent, thread, &answer);
                            // Post as soon as this agent finishes, not when the wave does.
                            self.post_message(thread, agent, &answer);
                            if let (Some(d), Some(hook)) = (self.discord_rest(), g.webhook_ref.as_deref().and_then(secret_get))
                                && let Err(e) = d.post_as(&hook, agent, None, &self.with_pings(&answer)).await
                            {
                                self.post_message(thread, "system", &format!("Discord 发送失败：{e}"));
                            }
                            mentions(&answer, &g.members, Some(agent)).into_iter().map(|m| (m, agent.clone())).collect()
                        }
                        Err(e) => {
                            self.post_message(thread, "system", &fail_line(agent, &e));
                            Vec::new()
                        }
                    }
                }
            });
            let mut next: Vec<(String, String)> = Vec::new();
            for m in futures::future::join_all(runs).await.into_iter().flatten() {
                if !next.iter().any(|(a, _)| *a == m.0) { next.push(m); }
            }
            wave = next;
        }
    }

    /// `/stop` and `/help`, typed by a person in a group. True if handled.
    async fn group_command(&self, g: &GroupRow, thread: &str, text: &str) -> bool {
        match text.trim() {
            "/stop" | "/停止" => {
                let mut n = 0;
                for m in &g.members { if self.hub.cancel(m, thread).await { n += 1; } }
                self.post_message(thread, "system", &if n > 0 { format!("已停止 {n} 个 agent 的回复") } else { "现在没有 agent 在回复".into() });
                true
            }
            "/help" | "/帮助" => {
                let help = group_help();
                self.post_message(thread, "system", help);
                if !g.id.starts_with("local-") && let Some(d) = self.discord_rest() {
                    let ch = g.id.clone();
                    tauri::async_runtime::spawn(async move { let _ = d.post(&ch, help).await; });
                }
                true
            }
            _ => false,
        }
    }

    /// Who besides the owner and this Disco's agents has spoken in the group lately.
    fn roster(&self, g: &GroupRow, thread: &str) -> Roster {
        let mut r = Roster::default();
        for m in self.db().messages(thread, 100).unwrap_or_default() {
            let a = m.author;
            if a == "me" || a == "system" || g.members.contains(&a) { continue; }
            let list = if a.ends_with(HUMAN_TAG) { &mut r.people } else { &mut r.agents };
            let name = display(&a);
            if !list.contains(&name) { list.push(name); }
        }
        r
    }

    /// At most FOREIGN_TURNS agent hand-offs from other Discos per channel per 10 minutes.
    fn foreign_turn_ok(&self, channel: &str) -> bool {
        let now = now();
        let mut f = self.foreign.lock().unwrap_or_else(|p| p.into_inner());
        let v = f.entry(channel.to_owned()).or_default();
        v.retain(|t| now - t < 600);
        if v.len() >= FOREIGN_TURNS { return false; }
        v.push(now);
        true
    }

    /// `@Name` of people we know on Discord written as `<@id>`, so Discord notifies them.
    fn with_pings(&self, text: &str) -> String {
        let ids = self.people.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let mut out = text.to_owned();
        for (name, id) in ids {
            out = replace_mention(&out, &name, &format!("<@{id}>"));
        }
        out
    }

    fn group_prompt(&self, g: &GroupRow, thread: &str, me: &str, by: &str, roster: &Roster) -> String {
        let history = self.db().messages(thread, TRANSCRIPT).unwrap_or_default();
        let key = format!("{me}|{thread}");
        let last = self.seen.lock().unwrap_or_else(|p| p.into_inner()).get(&key).copied();
        if let Some(newest) = history.last() {
            self.seen.lock().unwrap_or_else(|p| p.into_inner()).insert(key, newest.id);
        }
        let owner = share::owner_name();
        let mut p = match last {
            None => {
                let mut people = vec![format!("{owner}（群主）")];
                people.extend(roster.people.iter().cloned());
                let mut agents: Vec<String> = g.members.iter().filter(|m| *m != me).cloned().collect();
                agents.extend(roster.agents.iter().map(|a| format!("{a}（其他人的）")));
                format!(
                    "你是 {me}，正在群聊「{}」里和人类及其他 agent 协作。\n人类：{}。\nagent：{}。\n\
                     群规：只在被 @ 时发言；回复第一个字必须是 @对方（回答谁就 @谁）；需要别的 agent 接手就 @它的名字，它会被触发；\
                     @人类只是通知，不会触发任何 agent。只输出你要发到群里的那段话，不要复述聊天记录。\n\n最近的聊天记录：\n",
                    g.name, people.join("、"), if agents.is_empty() { "（只有你）".into() } else { agents.join("、") }
                )
            }
            // The session already holds everything up to `last`, including our own replies.
            Some(_) => String::from("群里的新消息：\n"),
        };
        for m in history.iter().filter(|m| last.is_none_or(|l| m.id > l) && m.author != me) {
            let who = if m.author == "me" { format!("{owner}（人类）") } else if is_human(&m.author) { format!("{}（人类）", display(&m.author)) } else { m.author.clone() };
            p.push_str(&format!("[{who}] {}\n", m.body));
        }
        p.push_str(&format!("\n（这次是 {} 叫你，回复以 @{} 开头。）\n", display(by), display(by)));
        p
    }

    /// The agent's sessions are gone (stopped or crashed): forget what they had seen.
    fn forget_sessions(&self, agent: Option<&str>) {
        let keep = |k: &String| agent.is_some_and(|a| !k.starts_with(&format!("{a}|")));
        self.primed.lock().unwrap_or_else(|p| p.into_inner()).retain(keep);
        self.seen.lock().unwrap_or_else(|p| p.into_inner()).retain(|k, _| keep(k));
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
    let is_name_char = |c: char| c.is_alphanumeric() || c == '-' || c == '_';
    // `@all` as a word (so a member called `alloy` is not everyone).
    let all = lower.match_indices("@all").any(|(i, _)| !lower[i + 4..].chars().next().is_some_and(is_name_char)) && !members.iter().any(|m| m.eq_ignore_ascii_case("all"));
    if all || text.contains("@所有人") {
        return others.cloned().collect();
    }
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

/// People who write from Discord are stored as `Name (Discord)`, so a display name
/// can never pass for the owner (`me`) or an agent.
pub(crate) const HUMAN_TAG: &str = " (Discord)";
/// Hand-offs from other Discos' agents allowed per channel per 10 minutes.
const FOREIGN_TURNS: usize = 6;
/// Agent commands that change its state: only the agent's owner may run them from a group.
const RESTRICTED: &[&str] = &["clear", "compact", "model", "config", "login", "logout", "permissions", "init", "mcp", "resume", "reset", "new", "exit"];

fn is_human(author: &str) -> bool { author == "me" || author.ends_with(HUMAN_TAG) }

/// The name people see: `me` is the owner, `Name (Discord)` is `Name`.
fn display(author: &str) -> String {
    if author == "me" { share::owner_name() } else { author.strip_suffix(HUMAN_TAG).unwrap_or(author).to_owned() }
}

/// Other people and other Discos' agents seen in a group.
#[derive(Default)]
struct Roster { people: Vec<String>, agents: Vec<String> }

impl Roster {
    fn multi(&self) -> bool { !self.people.is_empty() || !self.agents.is_empty() }
}

fn group_help() -> &'static str {
    "群规：@agent 它立刻回答（可以同时 @ 几个）；只 @ 人时 agent 不插话；谁都没 @ 时，只有群主一个人的群由组长回答，多人群里 agent 不插话。\n\
     agent 的每条回复都以 @对方 开头；它 @ 另一个 agent 就是交接。\n\
     命令：@agent /命令 运行它自己的命令（清空、换模型等只有主人能用）；/stop 停止所有回复；/team 任务 分工协作（群主）；/help 看这段说明。"
}

/// The command line after `@agent` when the text is `@agent /command …`.
fn slash_for(text: &str, agent: &str) -> Option<String> {
    let lower = text.to_lowercase();
    let needle = format!("@{}", agent.to_lowercase());
    let i = lower.find(&needle)? + needle.len();
    // Lower-casing can change byte lengths outside ASCII: then no match rather than a bad slice.
    let rest = if lower.len() == text.len() { text.get(i..)? } else { return None }.trim_start();
    rest.starts_with('/').then(|| rest.lines().next().unwrap_or(rest).trim().to_owned())
}

fn restricted(cmd: &str) -> bool {
    let name = cmd.trim_start_matches('/').split_whitespace().next().unwrap_or_default().to_lowercase();
    RESTRICTED.contains(&name.as_str())
}

/// Make sure an agent's group answer starts by addressing someone.
fn lead_mention(answer: &str, to: &str) -> String {
    if answer.starts_with('@') { answer.to_owned() } else { format!("@{to} {answer}") }
}

/// Replace whole-word `@name` (case-insensitive) with `with`.
fn replace_mention(text: &str, name: &str, with: &str) -> String {
    let needle = format!("@{}", name.to_lowercase());
    let lower = text.to_lowercase();
    if lower.len() != text.len() || name.is_empty() { return text.to_owned(); }
    let is_name_char = |c: char| c.is_alphanumeric() || c == '-' || c == '_';
    let (mut out, mut last) = (String::new(), 0);
    for (i, _) in lower.match_indices(&needle) {
        let end = i + needle.len();
        if i < last || text[end..].chars().next().is_some_and(is_name_char) { continue; }
        out.push_str(&text[last..i]);
        out.push_str(with);
        last = end;
    }
    out.push_str(&text[last..]);
    out
}

/// Errors starting with this are notices, not failures ("sent, no reply yet").
pub(crate) const QUIET: &str = "⏳";

/// The system line for a failed turn, with a one-line fix for errors we recognise.
pub(crate) fn fail_line(agent: &str, e: &str) -> String {
    match e.strip_prefix(QUIET) {
        Some(n) => format!("{agent} {n}"),
        None => match hint(e) { Some(h) => format!("{agent} 出错：{e}\n💡 {h}"), None => format!("{agent} 出错：{e}") },
    }
}

/// Common start-up and auth failures, in plain words. First match wins.
fn hint(e: &str) -> Option<&'static str> {
    const HINTS: &[(&[&str], &str)] = &[
        (&["native binary not found", "claude_code_executable"], if cfg!(windows) {
            "需要原生版 Claude Code：在 PowerShell 运行 `irm https://claude.ai/install.ps1 | iex`，然后完全退出并重新打开 Disco"
        } else {
            "需要原生版 Claude Code：在终端运行 `curl -fsSL https://claude.ai/install.sh | bash`，然后完全退出并重新打开 Disco"
        }),
        (&["os error 193", "not a valid win32"], "Windows 无法直接运行这个命令：升级到最新版 Disco，或在添加 agent 时填 .cmd / .exe 的完整路径"),
        (&["no llm provider", "api key", "api_key", "unauthorized", "401", "not logged in", "authentication"],
            "这个 agent 还没登录或缺少 API Key：先在终端里运行一次它完成登录（如 `claude` 后输入 /login），再完全退出并重新打开 Disco"),
        (&["timed out", "timeout", "超时"], "agent 太久没有回应：点停止后再发一遍，或到设置里一键诊断"),
        (&["进程已退出"], "agent 进程退出了，下一条消息会自动重新启动它"),
    ];
    let low = e.to_lowercase();
    HINTS.iter().find(|(keys, _)| keys.iter().any(|k| low.contains(k))).map(|(_, h)| *h)
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
struct AgentView { name: String, kind: String, template: String, detail: String, live: bool, avatar: String }
#[derive(Serialize)]
struct GroupView { id: String, name: String, guild_id: String, lead: Option<String>, members: Vec<String>, people: Vec<String>, guests: Vec<String> }
#[derive(Serialize)]
struct Head { thread: String, author: String, body: String, ts: i64 }
#[derive(Serialize)]
struct PeerView { thread: String, guest: String, agent: String }
#[derive(Serialize)]
struct Overview {
    peers: Vec<PeerView>,
    agents: Vec<AgentView>,
    groups: Vec<GroupView>,
    heads: Vec<Head>,
    discord_token: bool,
    discord_status: String,
    paused: bool,
    /// Chats with memory switched off.
    memory_off: Vec<String>,
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
        let avatar = core.db().setting(&format!("{SET_AVATAR}{}", a.name)).unwrap_or_default();
        agents.push(AgentView { live: core.hub.is_live(&a.name).await, name: a.name, kind: a.transport, template, detail, avatar });
    }
    let peers = core.shares().into_iter().map(|(id, s)| PeerView { thread: format!("peer:{id}"), guest: s.guest, agent: s.agent }).collect();
    Ok(Overview {
        peers,
        agents,
        groups: groups.into_iter().map(|g| {
            let r = core.roster(&g, &format!("group:{}", g.id));
            GroupView { id: g.id, name: g.name, guild_id: g.guild_id, lead: g.lead_agent, members: g.members, people: r.people, guests: r.agents }
        }).collect(),
        heads: heads.into_iter().map(|(t, m)| Head { thread: t, author: m.author, body: m.body, ts: m.ts }).collect(),
        discord_token: core.db().setting(SET_TOKEN_SAVED).is_some(),
        discord_status: core.discord_status.lock().unwrap_or_else(|p| p.into_inner()).clone(),
        paused: core.paused.load(Ordering::Relaxed),
        memory_off: core.db().settings_with_prefix(memory::MEM_OFF).unwrap_or_default().into_iter()
            .filter(|(_, v)| v == "1").map(|(k, _)| k).collect(),
    })
}

#[tauri::command]
fn detect_agents(core: St<'_>) -> Vec<templates::Detected> {
    templates::detect(&core.hub.user_path)
}

#[tauri::command]
fn conda_envs() -> Vec<templates::CondaEnv> {
    templates::conda_envs(&hub::dirs_home())
}

/// Native folder (or, with `file`, file) picker: AppleScript on macOS, Windows
/// Forms through PowerShell on Windows, so no extra plugin. None if cancelled.
#[tauri::command]
async fn choose_path(file: Option<bool>) -> Option<String> {
    let file = file == Some(true);
    tauri::async_runtime::spawn_blocking(move || {
        #[cfg(windows)]
        let out = {
            let dialog = if file { "$d=New-Object System.Windows.Forms.OpenFileDialog; $d.Title='选择 agent 程序'; if($d.ShowDialog() -eq 'OK'){[Console]::Out.Write($d.FileName)}" }
                else { "$d=New-Object System.Windows.Forms.FolderBrowserDialog; $d.Description='选择 agent 的工作目录'; if($d.ShowDialog() -eq 'OK'){[Console]::Out.Write($d.SelectedPath)}" };
            hub::quiet_cmd("powershell").args(["-NoProfile", "-STA", "-Command", &format!("[Console]::OutputEncoding=[Text.Encoding]::UTF8; Add-Type -AssemblyName System.Windows.Forms; {dialog}")]).output().ok()?
        };
        #[cfg(not(windows))]
        let out = {
            let script = if file { "POSIX path of (choose file with prompt \"选择 agent 程序\")" } else { "POSIX path of (choose folder with prompt \"选择 agent 的工作目录\")" };
            std::process::Command::new("osascript").args(["-e", script]).output().ok()?
        };
        let p = String::from_utf8(out.stdout).ok()?.trim().trim_end_matches(['/', '\\']).to_owned();
        (out.status.success() && !p.is_empty()).then_some(p)
    }).await.ok().flatten()
}

/// Show `path` selected in Finder / Explorer.
fn reveal(path: &std::path::Path) -> CmdResult<()> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        std::process::Command::new("explorer").raw_arg(format!("/select,\"{}\"", path.display())).spawn().map_err(err)?;
    }
    #[cfg(not(windows))]
    std::process::Command::new("/usr/bin/open").arg("-R").arg(path).spawn().map_err(err)?;
    Ok(())
}

/// Open a web page in the default browser.
fn open_in_browser(url: &str) -> CmdResult<()> {
    // `explorer <url>` is unreliable for URLs with a query string; the URL
    // protocol handler opens the default browser the same way a link click does.
    #[cfg(windows)]
    hub::quiet_cmd("rundll32").args(["url.dll,FileProtocolHandler", url]).spawn().map_err(err)?;
    #[cfg(not(windows))]
    std::process::Command::new("/usr/bin/open").arg(url).spawn().map_err(err)?;
    Ok(())
}

#[tauri::command]
fn memories(core: St<'_>) -> Vec<memory::Note> { core.memory_list() }

#[tauri::command]
fn forget(core: St<'_>, key: String) -> CmdResult<()> { core.memory_forget(&key) }

/// Split a command line on spaces, keeping "double" or 'single' quoted parts together.
fn split_args(line: &str) -> Vec<String> {
    let (mut out, mut cur, mut quote, mut any) = (Vec::new(), String::new(), None, false);
    for c in line.chars() {
        match (quote, c) {
            (None, '"' | '\'') => { quote = Some(c); any = true; }
            (Some(q), c) if c == q => quote = None,
            (None, c) if c.is_whitespace() => { if any { out.push(std::mem::take(&mut cur)); any = false; } }
            (_, c) => { cur.push(c); any = true; }
        }
    }
    if any { out.push(cur); }
    out
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
pub(crate) struct NewAgent {
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
    /// Conda environment prefix the agent runs in.
    conda: Option<String>,
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

impl NewAgent {
    /// A remote A2A agent behind a bearer token (Disco invites).
    pub(crate) fn remote(name: String, url: String, token: String) -> Self {
        Self { template: "a2a".into(), name, url: Some(url), header_name: Some("authorization".into()), header_value: Some(format!("Bearer {token}")),
            command: None, cwd: None, count: None, worktree: None, conda: None }
    }
}

pub(crate) async fn add_agent_rows(core: &Core, spec: NewAgent) -> CmdResult<Vec<String>> {
    let NewAgent { template, name, url, header_name, header_value, command, cwd, count, worktree, conda } = spec;
    let name = valid_name(&name)?;
    let kind = match template.as_str() { "a2a" => "a2a", "command" => "command", _ => "acp" };
    let row = match kind {
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
            let (cmd, args, blurb) = if kind == "command" {
                let line = command.unwrap_or_default();
                let mut it = split_args(&line).into_iter();
                (it.next().ok_or("请填写命令")?, it.collect(), "自定义 ACP 命令")
            } else {
                templates::resolve(&template, &core.hub.user_path).ok_or("未知 agent 类型")?
            };
            let n = count.unwrap_or(1).clamp(1, 8) as usize;
            let spec = LocalSpec {
                template: template.clone(), command: cmd.clone(), args: args.clone(),
                cwd: cwd.unwrap_or_default().trim().into(), description: blurb.into(),
                worktree: worktree.unwrap_or(n > 1), conda: conda.unwrap_or_default().trim().into(),
                ..Default::default()
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
async fn remove_agent(core: St<'_>, name: String, purge: Option<bool>) -> CmdResult<()> {
    core.hub.drop_agent(&name).await;
    core.forget_sessions(Some(&name));
    let row = core.agent(&name)?;
    if let Some(c) = row.credential_ref { secret_del(&c); }
    core.db().delete_agent(&name, purge.unwrap_or(false)).map_err(err)?;
    core.shares_follow(&name, None);
    let _ = core.db().set_setting(&format!("{SET_AVATAR}{name}"), "");
    let _ = core.db().audit("user", Some(&name), "remove_agent", "ok");
    if let Some(ch) = core.agent_channel(&name) {
        let _ = core.db().set_setting(&format!("{SET_AGENT_CH}{name}"), "");
        secret_del(&format!("webhook/{ch}"));
        if let Some(d) = core.discord_rest() { let _ = d.delete_channel(&ch).await; }
    }
    Ok(())
}

/// Pick an agent's avatar (`<form>.<palette>`, both short ASCII words; "" = automatic).
#[tauri::command]
fn set_avatar(core: St<'_>, name: String, avatar: String) -> CmdResult<()> {
    core.agent(&name)?;
    if avatar.len() > 32 || !avatar.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-') {
        return Err("无效的形象".into());
    }
    core.db().set_setting(&format!("{SET_AVATAR}{name}"), &avatar).map_err(err)
}

/// Rename an agent: history, groups, memories and its Discord channel follow it.
#[tauri::command]
async fn rename_agent(core: St<'_>, name: String, new_name: String) -> CmdResult<()> {
    let new = valid_name(&new_name)?;
    if new == name { return Ok(()); }
    let mut row = core.agent(&name)?;
    if core.db().list_agents().map_err(err)?.iter().any(|a| a.name == new) {
        return Err(format!("已经有名为 {new} 的 agent"));
    }
    core.hub.drop_agent(&name).await;
    core.forget_sessions(Some(&name));
    core.shares_follow(&name, Some(&new));
    if let Some(mut spec) = row.card_json.as_deref().and_then(|j| serde_json::from_str::<LocalSpec>(j).ok()) {
        if spec.worktree && spec.worktree_name.is_empty() { spec.worktree_name = name.clone(); }
        row.card_json = Some(serde_json::to_string(&spec).map_err(err)?);
        core.db().upsert_agent(&row).map_err(err)?;
    }
    core.db().rename_agent(&name, &new).map_err(err)?;
    core.memory_rename(&name, &new);
    if let Some(av) = core.db().setting(&format!("{SET_AVATAR}{name}")).filter(|v| !v.is_empty()) {
        core.db().set_setting(&format!("{SET_AVATAR}{name}"), "").map_err(err)?;
        core.db().set_setting(&format!("{SET_AVATAR}{new}"), &av).map_err(err)?;
    }
    if let Some(ch) = core.agent_channel(&name) {
        core.db().set_setting(&format!("{SET_AGENT_CH}{name}"), "").map_err(err)?;
        core.db().set_setting(&format!("{SET_AGENT_CH}{new}"), &ch).map_err(err)?;
        if let Some(d) = core.discord_rest() {
            tauri::async_runtime::spawn(async move { let _ = d.rename_channel(&ch, &new).await; });
        }
    }
    let _ = core.db().audit("user", Some(&name), "rename_agent", &new_name);
    core.inner().warm(Some(vec![new_name.trim().to_owned()]));
    Ok(())
}

/// Stop the reply an agent is giving in a thread.
#[tauri::command]
async fn stop_reply(core: St<'_>, thread: String, agent: String) -> CmdResult<bool> {
    Ok(core.hub.cancel(&agent, &thread).await)
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
async fn agent_configs(core: St<'_>, agent: String) -> CmdResult<Vec<hub::ConfigView>> {
    let row = core.agent(&agent)?;
    core.hub.configs(&row, &secret_get, &format!("dm:{agent}")).await
}

/// Pick a model / reasoning level / mode for an agent: applied to its open sessions
/// right away and saved so every later session starts with it.
#[tauri::command]
async fn set_agent_config(core: St<'_>, agent: String, id: String, value: String) -> CmdResult<()> {
    let mut row = core.agent(&agent)?;
    let mut spec: hub::LocalSpec = row.card_json.as_deref().and_then(|j| serde_json::from_str(j).ok()).ok_or("这个 agent 没有可调的设置")?;
    spec.config.insert(id.clone(), value.clone());
    row.card_json = Some(serde_json::to_string(&spec).map_err(err)?);
    core.db().upsert_agent(&row).map_err(err)?;
    core.hub.set_config(&agent, &id, &value).await
}

#[tauri::command]
fn team_state(core: St<'_>, thread: String) -> Option<team::State> { core.team_state(&thread) }

#[tauri::command]
fn team_files(core: St<'_>, thread: String) -> Vec<(String, String)> { core.team_files(&thread) }

#[tauri::command]
fn inbox(core: St<'_>) -> Vec<inbox::Item> { core.inbox.list() }

// ---------------------------------------------------------------- tailnet

#[tauri::command]
fn net_policy(core: St<'_>) -> tailnet::Policy { core.net_policy() }

/// Who may reach this Mac over the tailnet, and who may wake its agents.
#[tauri::command]
fn net_policy_set(core: St<'_>, policy: tailnet::Policy) -> CmdResult<tailnet::Policy> {
    let p = policy.cleaned()?;
    core.db().set_setting(tailnet::POLICY, &serde_json::to_string(&p).map_err(err)?).map_err(err)?;
    let _ = core.db().audit("me", None, "tailnet.policy", &format!("{} allow={} deny={} quiet={}-{}", p.mode, p.allow.len(), p.deny.len(), p.quiet_start, p.quiet_end));
    Ok(p)
}

/// The user hands a file to a chat (see files.rs).
#[tauri::command]
async fn send_file(core: St<'_>, thread: String, path: String, note: Option<String>) -> CmdResult<String> {
    core.share_file(&thread, "me", &path, note.as_deref().unwrap_or("")).await
}

#[tauri::command]
async fn inbox_answer(core: St<'_>, id: String, option_id: Option<String>, text: Option<String>) -> CmdResult<String> {
    core.inbox_answer(&id, inbox::Answer { option: option_id, text }, "Disco").await
}

#[tauri::command]
async fn stop_all(core: St<'_>) -> CmdResult<usize> {
    core.paused.store(true, Ordering::Relaxed);
    let n = core.hub.stop_all().await;
    core.forget_sessions(None);
    core.inbox_clear();
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
struct DiscordSettings { home_guild: Option<String>, guilds: Vec<Channel>, allow: String, invite_url: Option<String>, problem: Option<String> }

#[tauri::command]
async fn discord_settings(core: St<'_>, home_guild: Option<String>, allow: Option<String>) -> CmdResult<DiscordSettings> {
    if let Some(a) = allow { core.db().set_setting(SET_ALLOW, &parse_ids(&a).join(", ")).map_err(err)?; }
    let changed_home = home_guild.is_some();
    if let Some(g) = home_guild { core.db().set_setting(SET_HOME_GUILD, &g).map_err(err)?; }
    if changed_home {
        let c = core.inner().clone();
        tauri::async_runtime::spawn(async move { let _ = c.sync_agent_channels(None).await; });
    }
    // Errors are shown, not swallowed: an empty list must mean "not invited yet".
    let (guilds, invite_url, problem) = match core.discord_rest() {
        Some(r) => {
            let (gs, me) = tokio::join!(r.guilds(), r.me());
            let problem = gs.as_ref().err().or(me.as_ref().err()).cloned();
            let gs = gs.unwrap_or_default();
            let id = me.ok().map(|(id, _)| id);
            (gs.into_iter().map(|g| Channel { guild_id: g.id.clone(), guild: g.name, id: g.id, name: String::new() }).collect(),
             id.map(|id| format!("https://discord.com/oauth2/authorize?client_id={id}&scope=bot&permissions={}", disco_discord::BOT_PERMISSIONS)),
             problem)
        }
        None => (Vec::new(), None, Some(core.discord_status())),
    };
    let db = core.db();
    // The bot just joined its first server: make it home and build the channels.
    if db.setting(SET_HOME_GUILD).is_none() && guilds.len() == 1 {
        db.set_setting(SET_HOME_GUILD, &guilds[0].id).map_err(err)?;
        let c = core.inner().clone();
        tauri::async_runtime::spawn(async move { let _ = c.sync_agent_channels(None).await; });
    }
    Ok(DiscordSettings { home_guild: db.setting(SET_HOME_GUILD), guilds, allow: db.setting(SET_ALLOW).unwrap_or_default(), invite_url, problem })
}

#[tauri::command]
async fn discord_sync(core: St<'_>) -> CmdResult<usize> {
    core.sync_agent_channels(None).await
}

/// Opens Discord pages (bot invite / developer portal) in the default browser.
#[derive(Serialize)]
struct Space { todos: Vec<mcp::Todo>, doc: String, doc_path: String }

/// A group's or DM's shared to-dos and doc (`thread` is `group:<id>` or `dm:<agent>`).
#[tauri::command]
fn space(core: St<'_>, thread: String) -> CmdResult<Space> {
    let path = mcp::doc_path(&core, &thread);
    Ok(Space { todos: mcp::todos(&core, &thread), doc: std::fs::read_to_string(&path).unwrap_or_default(), doc_path: path.display().to_string() })
}

#[tauri::command]
fn todo_new(core: St<'_>, thread: String, text: String) -> CmdResult<String> {
    let text = text.trim();
    if text.is_empty() { return Err("内容不能为空".into()); }
    Ok(mcp::add_todo(&core, &thread, text, None, "me"))
}

#[tauri::command]
fn todo_set(core: St<'_>, thread: String, id: String, done: bool) -> CmdResult<()> {
    mcp::set_todo_done(&core, &thread, &id, done).map(|_| ())
}

#[tauri::command]
fn todo_remove(core: St<'_>, thread: String, id: String) -> CmdResult<()> {
    mcp::delete_todo(&core, &thread, &id)
}

#[tauri::command]
fn doc_save(core: St<'_>, thread: String, content: String) -> CmdResult<()> {
    mcp::write_doc(&mcp::doc_path(&core, &thread), &content)?;
    use mcp::EmitSpace;
    let _ = core.app.emit_space(&thread);
    Ok(())
}

/// Show the doc in Finder (creating an empty one first), so it can be opened in any editor.
#[tauri::command]
fn reveal_doc(core: St<'_>, thread: String) -> CmdResult<()> {
    let path = mcp::doc_path(&core, &thread);
    if !path.exists() { mcp::write_doc(&path, "")?; }
    reveal(&path)
}

/// Show a file Disco keeps (a received or shared file) in Finder. Only paths inside
/// Disco's own data folder: a message's text must not be able to point anywhere else.
#[tauri::command]
fn reveal_path(core: St<'_>, path: String) -> CmdResult<()> {
    let base = core.app.path().app_data_dir().map_err(err)?;
    let p = std::path::PathBuf::from(path.trim());
    if p.components().any(|c| matches!(c, std::path::Component::ParentDir)) || !p.starts_with(&base) || !p.exists() {
        return Err("这个文件不在 Disco 的数据目录里，或已被删除".into());
    }
    reveal(&p)
}

#[tauri::command]
fn open_url(url: String) -> CmdResult<()> {
    if !(url.starts_with("https://discord.com/") || url.starts_with("https://github.com/Cosmoto-jian/") || url.starts_with("https://tailscale.com/")) {
        return Err("不允许打开该链接".into());
    }
    open_in_browser(&url)
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
    add(if cfg!(windows) { "Windows 凭据管理器" } else { "macOS 钥匙串" }, with_vault(|m| persist_vault(m)).map(|_| "可读写（所有密钥存在一条钥匙串项里）".into()));
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
    let me = tauri::async_runtime::spawn_blocking(tailnet::me).await.map_err(err)?;
    add("Tailscale", me.map(|n| format!("这台 Mac：{}", n.node)).ok_or("没检测到：共享 agent 需要先安装并登录 Tailscale".into()));
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
async fn save_group(core: St<'_>, channel_id: Option<String>, guild_id: Option<String>, name: String, members: Vec<String>, lead: Option<String>) -> CmdResult<String> {
    if members.is_empty() { return Err("至少选一个 agent".into()); }
    let name = name.trim();
    if name.is_empty() { return Err("给群起个名字".into()); }
    let rest = core.discord_rest();
    let home = core.db().setting(SET_HOME_GUILD);
    // No channel given: make one in the home guild, or keep the group local when Discord isn't set up.
    let (channel_id, guild_id) = match (channel_id.filter(|c| !c.is_empty()), &rest) {
        (Some(c), _) => (c, guild_id.unwrap_or_default()),
        (None, Some(r)) if let Some(guild) = home => {
            let (id, _) = r.ensure_channel(&guild, GROUP_CATEGORY, name, "Disco 群聊").await?;
            (id, guild)
        }
        _ => (format!("local-{}", uuid::Uuid::new_v4().simple()), String::new()),
    };
    let existing = core.db().list_groups().map_err(err)?.into_iter().find(|g| g.id == channel_id).and_then(|g| g.webhook_ref);
    let webhook_ref = match (existing, &rest) {
        (Some(r), _) if secret_get(&r).is_some() => Some(r),
        (_, Some(rest)) if !channel_id.starts_with("local-") => {
            let hook = rest.create_webhook(&channel_id).await?;
            let account = format!("webhook/{channel_id}");
            secret_set(&account, &hook)?;
            Some(account)
        }
        _ => None,
    };
    let lead = lead.filter(|l| members.contains(l)).or_else(|| members.first().cloned());
    core.db().upsert_group(&GroupRow { id: channel_id.clone(), guild_id, name: name.into(), lead_agent: lead, webhook_ref, members }).map_err(err)?;
    Ok(channel_id)
}

/// Delete a group. Its history and webhook stay, so `restore_group` can undo it.
#[tauri::command]
fn delete_group(core: St<'_>, id: String) -> CmdResult<()> {
    core.db().delete_group(&id).map_err(err).map(|_| ())
}

/// Undo `delete_group` (the UI offers it for a few seconds).
#[tauri::command]
fn restore_group(core: St<'_>, id: String, guild_id: String, name: String, members: Vec<String>, lead: Option<String>) -> CmdResult<()> {
    let hook = format!("webhook/{id}");
    let webhook_ref = secret_get(&hook).is_some().then_some(hook);
    core.db().upsert_group(&GroupRow { id, guild_id, name, lead_agent: lead, webhook_ref, members }).map_err(err)
}

/// Undo `forget` (the key carries scope, topic and text).
#[tauri::command]
fn memory_restore(core: St<'_>, key: String) -> CmdResult<()> {
    let mut parts = key.splitn(3, '/');
    let (Some(scope), Some(topic), Some(text)) = (parts.next(), parts.next(), parts.next()) else { return Err("bad key".into()) };
    core.remember_topic(scope, Some(topic), text);
    Ok(())
}

/// The chat's memory switch: off = its sessions neither read nor write memory.
#[tauri::command]
fn set_memory(core: St<'_>, thread: String, on: bool) -> CmdResult<()> {
    if memory::thread_of(&thread).as_deref() != Some(thread.as_str()) { return Err("未知会话".into()); }
    core.db().set_setting(&format!("{}{thread}", memory::MEM_OFF), if on { "" } else { "1" }).map_err(err)?;
    // Sessions primed before the switch keep what they were told; new ones follow the switch.
    let _ = core.db().audit("me", None, "memory", &format!("{thread} {}", if on { "on" } else { "off" }));
    Ok(())
}

/// Slash commands an agent offers (known once it has started a session).
#[tauri::command]
async fn agent_commands(core: St<'_>, agent: String) -> CmdResult<Vec<(String, String)>> {
    Ok(core.hub.commands(&agent).await)
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
            if let Some(press) = m.press {
                let c = core.clone();
                tauri::async_runtime::spawn(async move { c.inbox_press(&m.author_id, &m.author, press).await });
                continue;
            }
            let c = core.clone();
            let agent = c.db().settings_with_prefix(SET_AGENT_CH).unwrap_or_default().into_iter().find(|(_, ch)| *ch == m.channel_id).map(|(a, _)| a);
            let group = c.db().list_groups().unwrap_or_default().into_iter().find(|g| g.id == m.channel_id);
            let Some((author, content)) = c.speaker(&m, group.as_ref(), agent.is_some()) else { continue };
            // Each message runs on its own task so one slow agent never blocks others.
            tauri::async_runtime::spawn(async move {
                match agent {
                    Some(a) => c.dm(a, author, content, true).await,
                    None => c.group(m.channel_id, author, content, true).await,
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
                foreign: Mutex::default(),
                people: Mutex::default(),
                warned: Mutex::default(),
                paused: AtomicBool::new(false),
                primed: Mutex::default(),
                seen: Mutex::default(),
                inbox: Arc::default(),
                teams: team::Teams::default(),
                server: Mutex::default(),
                app: app.handle().clone(),
            });
            // Installs from before v0.4 kept only the Keychain entry.
            if core.db().setting(SET_TOKEN_SAVED).is_none() && secret_get(DISCORD_KEY).is_some() {
                let _ = core.db().set_setting(SET_TOKEN_SAVED, "1");
            }
            if core.db().setting(SET_TOKEN_SAVED).is_some() { connect_discord(core.clone()); }
            core.memory_migrate();
            mcp::start(core.clone());
            core.warm(None);
            if !core.shares().is_empty() { core.serve_shares(); }
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
            overview, detect_agents, conda_envs, choose_path, memories, forget, add_agent, remove_agent, rename_agent, set_avatar, stop_reply, messages, send, inbox, inbox_answer, net_policy, net_policy_set, send_file, team_state, team_files, agent_configs, set_agent_config,
            stop_all, resume, save_discord_token, discord_connect, discord_settings, discord_sync, diagnose, open_url, reveal_path, discord_channels, save_group, delete_group, restore_group, memory_restore, set_memory, agent_commands,
            check_update, install_update, space, todo_new, todo_set, todo_remove, doc_save, reveal_doc, share::share_status, share::create_share, share::share_invite, share::revoke_share, share::accept_invite
        ])
        .run(tauri::generate_context!())
        .expect("error while running Disco");
}

#[cfg(test)]
mod tests {
    use super::mentions;

    #[test]
    fn split_args_quotes() {
        assert_eq!(super::split_args(r#""/Applications/My Agent/bin/agent" --acp  'a b'"#), vec!["/Applications/My Agent/bin/agent", "--acp", "a b"]);
    }

    #[test]
    fn group_rules_helpers() {
        assert_eq!(super::slash_for("@coder /review src", "coder").as_deref(), Some("/review src"));
        assert_eq!(super::slash_for("@coder 看下 /x", "coder"), None);
        assert!(super::restricted("/model opus") && !super::restricted("/review"));
        assert_eq!(super::lead_mention("好的", "amy"), "@amy 好的");
        assert_eq!(super::lead_mention("@bob 你来", "amy"), "@bob 你来");
        assert_eq!(super::replace_mention("@Amy hi @amyx @amy", "amy", "<@1>"), "<@1> hi @amyx <@1>");
        assert!(super::is_human("me") && super::is_human("amy (Discord)") && !super::is_human("coder"));
    }

    #[test]
    fn hints_for_known_errors() {
        assert!(super::fail_line("c", "Claude native binary not found for win32-x64").contains("💡"));
        assert!(super::fail_line("h", "No LLM provider configured").contains("登录"));
        assert!(!super::fail_line("x", "something odd").contains("💡"));
    }

    #[test]
    fn mention_order_and_self_exclusion() {
        let m = vec!["planner".to_string(), "coder".to_string(), "reviewer".to_string()];
        assert_eq!(mentions("@Reviewer 看下，@coder 先写", &m, None), ["reviewer", "coder"]);
        assert_eq!(mentions("@coder 我自己来 @planner", &m, Some("coder")), ["planner"]);
        assert!(mentions("没有提到谁", &m, None).is_empty());
        let m2 = vec!["coder".to_string(), "coder-2".to_string(), "coder-3".to_string()];
        assert_eq!(mentions("@coder-2 和 @coder 分头做", &m2, None), ["coder-2", "coder"]);
        assert_eq!(mentions("@all 并行开工", &m2, Some("coder")), ["coder-2", "coder-3"]);
        let m3 = vec!["alloy".to_string(), "coder".to_string()];
        assert_eq!(mentions("@alloy 看下", &m3, None), ["alloy"]);
    }
}
