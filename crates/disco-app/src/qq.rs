//! QQ as a second place for group chats, through the official QQ Bot API (crate disco-qq).
//!
//! Set-up is one scan: Settings shows q.qq.com's bind QR code, the user scans it with
//! mobile QQ and picks or creates a bot; the AppID and AppSecret come back encrypted and
//! go straight to the OS keychain. A Disco group is tied to a QQ group by sending the
//! code Disco shows (`绑定 1234`) in that group, which also tells Disco which group
//! member is its owner. From then on the group works like a Discord-synced one: one bot
//! speaks for every local agent as `**name**：text`, people @ the bot and name an agent.
//!
//! An agent's 1:1 chat can sync to QQ instead of a Discord channel (chosen when it is
//! imported, changeable later): it mirrors into the bot's private (C2C) chat with the
//! owner, the person who scanned the bind code. That one private chat is shared by every
//! QQ-synced agent: answers come as `**name**：text`, and the owner's messages go to the
//! agent they start with (`@name …`, which also makes it the current one) or else to the
//! current one. Nobody but the owner can talk to agents there.
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use disco_qq::{Bind, BindState, Creds, Endpoints, Incoming, Kind, Rest, Status, To};
use serde::Serialize;
use tauri::Emitter;
use tokio::sync::{mpsc, watch};

use crate::{CmdResult, Core, St, err, secret_del, secret_get, secret_set};

const VAULT: &str = "qq_bot";
/// "1" once a bot is bound (the keychain is only read when this is set).
const SET_ON: &str = "qq_on";
const SET_NAME: &str = "qq_name";
/// Disco group id → QQ group openid, and back.
const GROUP: &str = "qq_group/";
const OF: &str = "qq_of/";
/// QQ group openid → the owner's member openid in it (learned from the bind code).
const ME: &str = "qq_me/";
/// The bot's owner: the scanner's openid from the bind (the only one heard in private chat).
const OWNER: &str = "qq_owner";
/// The agent the owner's private QQ messages go to when they name none.
const CUR: &str = "qq_cur";
/// Per agent: where its 1:1 chat syncs, "qq" or "discord" (unset = Discord, as before v1.11).
pub(crate) const SYNC: &str = "sync/";
/// The last choice made when importing (the next import's default).
const SYNC_LAST: &str = "sync_last";
/// A bind code is valid this long.
const CODE_TTL: Duration = Duration::from_secs(600);
/// QQ accepts at most 5 passive replies to a message (within 5 minutes in a group, 60 in
/// a private chat: `To::reply_window`).
const REPLY_MAX: u32 = 5;

struct Conn { rest: Rest, stop: watch::Sender<bool> }
static CONN: Mutex<Option<Conn>> = Mutex::new(None);
static STATUS: Mutex<String> = Mutex::new(String::new());
/// The bind task on screen: (task, its QR code as SVG, generation).
static QR: Mutex<Option<(Bind, String, u64)>> = Mutex::new(None);
static QR_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Bind codes waiting to be sent in a QQ group: (code, Disco group id, issued).
static CODES: Mutex<Vec<(String, String, Instant)>> = Mutex::new(Vec::new());
/// Per QQ group / private chat: the newest message to reply to, when it came, the next sequence number.
type Replies = HashMap<To, (String, Instant, u32)>;
static REPLY: Mutex<Option<Replies>> = Mutex::new(None);
/// Message ids already handled (QQ may deliver one twice).
static SEEN: Mutex<VecDeque<String>> = Mutex::new(VecDeque::new());
/// Test endpoints (selftest only): API + token, and the bind host.
static AT: Mutex<Option<(Endpoints, String)>> = Mutex::new(None);
/// What the connection has been doing, for Settings → QQ and the diagnostics.
static DIAG: Mutex<Diag> = Mutex::new(Diag::new());
/// Private-chat senders already told they are not the owner (told once per launch).
static TOLD: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// The bot's name as READY gave it.
static READY_NAME: Mutex<Option<String>> = Mutex::new(None);

/// The QQ connection's recent history (times are Unix milliseconds, 0 = never).
#[derive(Clone, Serialize, Default)]
pub struct Diag {
    /// 取 token / 取网关地址 / 连网关 / 鉴权 / 在线 / 重试中 / 已停止 / 已断开 (empty = not started).
    stage: String,
    /// READY (or RESUMED) and not lost since.
    ready: bool,
    /// The gateway is reached through the system proxy.
    proxy: bool,
    ready_at: u64,
    /// The last thing that went wrong on the connection.
    error: String,
    error_at: u64,
    /// The last gateway event of any kind, and how many came since launch.
    event: String,
    event_at: u64,
    events: u64,
    /// The last message that could not be sent to QQ.
    send_error: String,
    send_error_at: u64,
    /// Someone other than the owner wrote to the bot privately (the tail of their openid).
    stranger: String,
}

impl Diag {
    const fn new() -> Self {
        Self { stage: String::new(), ready: false, proxy: false, ready_at: 0, error: String::new(), error_at: 0, event: String::new(), event_at: 0, events: 0,
            send_error: String::new(), send_error_at: 0, stranger: String::new() }
    }
}

fn now_ms() -> u64 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64) }

fn tail(id: &str) -> String { id.chars().rev().take(6).collect::<Vec<_>>().into_iter().rev().collect() }

/// A message could not be sent to QQ: keep the reason for the status page.
fn note_send_error(e: &str) {
    let mut d = lock(&DIAG);
    d.send_error = e.to_owned();
    d.send_error_at = now_ms();
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> { m.lock().unwrap_or_else(|p| p.into_inner()) }

#[cfg(debug_assertions)]
pub(crate) fn use_endpoints(api: &str, bind_host: &str) {
    *lock(&AT) = Some((Endpoints { api: api.into(), token: format!("{api}/app/getAppAccessToken"), proxy: None }, bind_host.into()));
}

/// The real platform (with the system proxy as the fallback route), or the selftest mock.
fn endpoints() -> Endpoints {
    lock(&AT).as_ref().map(|a| a.0.clone()).unwrap_or_else(|| Endpoints { proxy: disco_discord::proxy_url(), ..Endpoints::default() })
}
fn bind_host() -> Option<String> { lock(&AT).as_ref().map(|a| a.1.clone()) }

pub(crate) fn rest() -> Option<Rest> { lock(&CONN).as_ref().map(|c| c.rest.clone()) }

/// The QQ group a Disco group is tied to.
pub(crate) fn openid_of(core: &Core, group: &str) -> Option<String> {
    core.db().setting(&format!("{GROUP}{group}")).filter(|s| !s.is_empty())
}

fn group_of(core: &Core, openid: &str) -> Option<String> {
    core.db().setting(&format!("{OF}{openid}")).filter(|s| !s.is_empty())
}

/// The bot's owner (whose private chat QQ-synced agents use), once known.
pub(crate) fn owner(core: &Core) -> Option<String> {
    core.db().setting(OWNER).filter(|s| !s.is_empty())
}

fn bound(core: &Core) -> bool { core.db().setting(SET_ON).as_deref() == Some("1") }

/// Where an agent's 1:1 chat syncs: "qq" or "discord".
pub(crate) fn agent_sync(core: &Core, agent: &str) -> &'static str {
    if core.db().setting(&format!("{SYNC}{agent}")).as_deref() == Some("qq") { "qq" } else { "discord" }
}

/// The choice offered first when importing: the last one made (QQ only while a bot is
/// bound), else whatever is connected, Discord when both are.
pub(crate) fn sync_default(core: &Core) -> &'static str {
    let last = core.db().setting(SYNC_LAST).unwrap_or_default();
    let qq = bound(core);
    match last.as_str() {
        "qq" if qq => "qq",
        "discord" => "discord",
        _ if core.discord_rest().is_none() && qq => "qq",
        _ => "discord",
    }
}

/// Check a requested sync choice ("" / None = the default).
pub(crate) fn sync_choice(core: &Core, want: Option<&str>) -> Result<&'static str, String> {
    match want.unwrap_or_default() {
        "" => Ok(sync_default(core)),
        "discord" => Ok("discord"),
        "qq" if bound(core) => Ok("qq"),
        "qq" => Err("先在设置里扫码接入 QQ 机器人，才能同步到 QQ".into()),
        other => Err(format!("未知的同步位置：{other}")),
    }
}

/// Remember where agents sync (and, when the user picked it, the choice for next time).
pub(crate) fn set_sync(core: &Core, names: &[String], to: &str, remember: bool) -> Result<(), String> {
    for n in names { core.db().set_setting(&format!("{SYNC}{n}"), to).map_err(err)?; }
    if remember { core.db().set_setting(SYNC_LAST, to).map_err(err)?; }
    Ok(())
}

/// A renamed (`Some(new)`) or removed agent: its sync choice and "current" mark follow it.
pub(crate) fn agent_follow(core: &Core, old: &str, new: Option<&str>) {
    let was = core.db().setting(&format!("{SYNC}{old}")).filter(|v| !v.is_empty());
    if let Some(v) = was {
        let _ = core.db().set_setting(&format!("{SYNC}{old}"), "");
        if let Some(n) = new { let _ = core.db().set_setting(&format!("{SYNC}{n}"), &v); }
    }
    let cur = core.db().setting(CUR);
    if cur.as_deref() == Some(old) { let _ = core.db().set_setting(CUR, new.unwrap_or_default()); }
}

/// Agents whose 1:1 chat syncs to QQ, in list order.
pub(crate) fn qq_agents(core: &Core) -> Vec<String> {
    let names: Vec<String> = core.db().list_agents().map(|v| v.into_iter().map(|a| a.name).collect()).unwrap_or_default();
    let syncs = core.db().settings_with_prefix(SYNC).unwrap_or_default();
    names.into_iter().filter(|n| syncs.iter().any(|(k, v)| k == n && v == "qq")).collect()
}

/// Where to post a thread's messages in QQ: the bot, and the QQ group (group chats) or
/// the owner's private chat (an agent whose 1:1 chat syncs to QQ).
pub(crate) fn target(core: &Core, thread: &str) -> Option<(Rest, To)> {
    if let Some(g) = thread.strip_prefix("group:") {
        let openid = openid_of(core, g)?;
        return Some((rest()?, To::Group(openid)));
    }
    let agent = thread.strip_prefix("dm:")?;
    if agent_sync(core, agent) != "qq" { return None; }
    let owner = owner(core)?;
    Some((rest()?, To::User(owner)))
}

/// Reserve `n` passive replies to the newest message there, if QQ still allows them.
fn reply_slot(to: &To, n: u32) -> Option<(String, u32)> {
    let mut g = lock(&REPLY);
    let e = g.get_or_insert_with(HashMap::new).get_mut(to)?;
    if e.1.elapsed() > to.reply_window() || e.2 + n > REPLY_MAX + 1 { return None; }
    let seq = e.2;
    e.2 += n;
    Some((e.0.clone(), seq))
}

/// Send with a passive reply when one is left; a refused passive reply (the message got
/// too old on QQ's clock, or its replies are used up) is sent again as an active
/// message. A failure is kept for Settings → QQ and the diagnostics.
async fn deliver<F, Fut>(to: &To, n: u32, send: F) -> Result<(), String>
where F: Fn(Option<String>) -> Fut, Fut: std::future::Future<Output = Result<(), String>> {
    let slot = reply_slot(to, n).map(|s| s.0);
    let passive = slot.is_some();
    let r = match send(slot).await {
        Err(e) if passive => send(None).await.map_err(|e2| format!("{e2}（被动回复：{e}）")),
        r => r,
    };
    if let Err(e) = &r { note_send_error(e); }
    r
}

/// Post as `**who**：text` (or just `text` when `who` is empty).
pub(crate) async fn post(rest: &Rest, to: &To, who: &str, text: &str) -> Result<(), String> {
    let body = if who.is_empty() { text.to_owned() } else { format!("**{who}**：{text}") };
    let n = disco_qq::chunk(&body).len() as u32;
    deliver(to, n, |slot| { let body = &body; async move { rest.send(to, body, slot.as_deref(), &[]).await } }).await
}

/// A file.
pub(crate) async fn post_file(rest: &Rest, to: &To, name: &str, bytes: &[u8]) -> Result<(), String> {
    deliver(to, 1, |slot| async move { rest.send_file(to, name, bytes, slot.as_deref()).await }).await
}

/// A card with buttons (an inbox item).
pub(crate) async fn post_card(rest: &Rest, to: &To, text: &str, buttons: &[disco_qq::Button]) -> Result<(), String> {
    let n = disco_qq::chunk(text).len() as u32;
    deliver(to, n, |slot| async move { rest.send(to, text, slot.as_deref(), buttons).await }).await
}

/// `@name rest` naming one of `agents` (the longest that fits; ASCII case ignored) → (agent, rest).
fn pick<'a>(text: &'a str, agents: &[String]) -> Option<(String, &'a str)> {
    let t = text.trim_start().strip_prefix('@').or_else(|| text.trim_start().strip_prefix('＠'))?;
    let a = agents.iter().filter(|a| {
        t.get(..a.len()).is_some_and(|h| h.eq_ignore_ascii_case(a))
            && t[a.len()..].chars().next().is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.'))
    }).max_by_key(|a| a.len())?;
    Some((a.clone(), t[a.len()..].trim_start_matches([' ', '\u{3000}', ':', '：', ',', '，']).trim()))
}

fn set_status(core: &Core, s: String) {
    *lock(&STATUS) = s.clone();
    let _ = core.app.emit("qq", s);
}

/// One step of the gateway client: the status line, the diagnostics, the audit log.
fn on_gateway(core: &Core, s: Status) {
    let now = now_ms();
    let mut audit = None;
    let mut fresh_name = None;
    let line = {
        let mut d = lock(&DIAG);
        match s {
            Status::Event(t) => { d.event = t; d.event_at = now; d.events += 1; return; }
            Status::Step(step) => { d.stage = step.into(); d.ready = false; format!("连接中：{step}…") }
            Status::Ready(name, proxy) => {
                d.stage = "在线".into();
                d.ready = true;
                d.proxy = proxy;
                d.ready_at = now;
                audit = Some(("qq.ready", if proxy { "READY（经系统代理）" } else { "READY" }.to_owned()));
                let mut known = lock(&READY_NAME);
                if let Some(n) = name.filter(|n| !n.is_empty()) { *known = Some(n.clone()); fresh_name = Some(n); }
                format!("在线 · {}", known.clone().unwrap_or_else(|| "QQ 机器人".into()))
            }
            Status::Retry(e, secs) => {
                d.stage = "重试中".into();
                d.ready = false;
                if d.error != e { audit = Some(("qq.error", e.clone())); }
                d.error = e.clone();
                d.error_at = now;
                format!("{e}，{secs} 秒后重试")
            }
            Status::Fatal(e) => {
                d.stage = "已停止".into();
                d.ready = false;
                audit = Some(("qq.error", e.clone()));
                d.error = e.clone();
                d.error_at = now;
                format!("已停止：{e}")
            }
            Status::Stopped => { d.stage = "已断开".into(); d.ready = false; "已断开".into() }
        }
    };
    if let Some((action, detail)) = audit { let _ = core.db().audit("qq", None, action, &detail); }
    // The bot's name from READY (binding may not have been able to read it). One db() per statement.
    if let Some(n) = fresh_name {
        let saved = core.db().setting(SET_NAME);
        if saved.is_some_and(|s| !s.is_empty() && s != n) { let _ = core.db().set_setting(SET_NAME, &n); }
    }
    set_status(core, line);
}

/// Connect the bound bot's gateway (again); called at launch and after binding.
pub(crate) fn connect(core: &Arc<Core>) {
    if core.db().setting(SET_ON).as_deref() != Some("1") { return; }
    let Some(creds) = secret_get(VAULT).and_then(|j| serde_json::from_str::<Creds>(&j).ok()) else {
        set_status(core, "读不到 QQ 机器人凭据：请重新扫码绑定".into());
        return;
    };
    let rest = Rest::new(creds, endpoints());
    let (stop_tx, stop_rx) = watch::channel(false);
    if let Some(old) = lock(&CONN).replace(Conn { rest: rest.clone(), stop: stop_tx }) { let _ = old.stop.send(true); }
    let (tx, mut rx) = mpsc::channel::<Incoming>(64);
    let c = core.clone();
    tauri::async_runtime::spawn(disco_qq::run_gateway(rest, tx, stop_rx, move |s| on_gateway(&c, s)));
    let c = core.clone();
    tauri::async_runtime::spawn(async move { while let Some(m) = rx.recv().await { c.on_qq(m); } });
}

fn disconnect() {
    if let Some(old) = lock(&CONN).take() { let _ = old.stop.send(true); }
}

/// `绑定 1234` (also `/bind 1234`) → the code.
fn bind_code(text: &str) -> Option<&str> {
    let t = text.trim();
    let code = t.strip_prefix("绑定").or_else(|| t.strip_prefix("/bind")).or_else(|| t.strip_prefix("/绑定"))?.trim();
    (code.len() == 4 && code.bytes().all(|b| b.is_ascii_digit())).then_some(code)
}

/// `**Name**：text` from another Disco's bot.
fn relayed(text: &str) -> Option<(&str, &str)> {
    let (name, rest) = text.strip_prefix("**")?.split_once("**：")?;
    (!name.is_empty() && !name.contains('\n')).then_some((name, rest))
}

fn first_time(id: &str) -> bool {
    if id.is_empty() { return true; }
    let mut s = lock(&SEEN);
    if s.iter().any(|x| x == id) { return false; }
    s.push_back(id.to_owned());
    if s.len() > 512 { s.pop_front(); }
    true
}

impl Core {
    /// One event from the QQ gateway (or a test).
    pub(crate) fn on_qq(self: &Arc<Self>, m: Incoming) {
        let c = self.clone();
        match m.kind {
            Kind::Press => { tauri::async_runtime::spawn(async move { c.qq_press(m).await }); }
            Kind::Removed => {
                if let Some(g) = group_of(self, &m.group) {
                    let _ = self.db().set_setting(&format!("{GROUP}{g}"), "");
                    let _ = self.db().set_setting(&format!("{OF}{}", m.group), "");
                    self.post_message(&format!("group:{g}"), "system", "QQ 机器人已被移出 QQ 群，这个群聊不再同步到 QQ。");
                    self.agents_changed();
                }
            }
            Kind::Group if first_time(&m.id) => { tauri::async_runtime::spawn(async move { c.qq_group_message(m).await }); }
            Kind::Direct if first_time(&m.id) => { tauri::async_runtime::spawn(async move { c.qq_direct(m).await }); }
            _ => {}
        }
    }

    async fn qq_group_message(self: &Arc<Self>, m: Incoming) {
        lock(&REPLY).get_or_insert_with(HashMap::new).insert(To::Group(m.group.clone()), (m.id.clone(), Instant::now(), 1));
        let Some(gid) = group_of(self, &m.group) else {
            // Not tied to a Disco group yet: only a bind code means anything here.
            let Some(code) = bind_code(&m.content) else { return };
            let found = {
                let mut codes = lock(&CODES);
                codes.retain(|c| c.2.elapsed() < CODE_TTL);
                codes.iter().position(|c| c.0 == code && !c.1.is_empty()).map(|i| codes.remove(i).1)
            };
            let Some(gid) = found else { return };
            let name = self.db().list_groups().ok().and_then(|gs| gs.into_iter().find(|g| g.id == gid)).map(|g| g.name).unwrap_or_default();
            let _ = self.db().set_setting(&format!("{GROUP}{gid}"), &m.group);
            let _ = self.db().set_setting(&format!("{OF}{}", m.group), &gid);
            let _ = self.db().set_setting(&format!("{ME}{}", m.group), &m.author_id);
            self.post_message(&format!("group:{gid}"), "system", &format!("已和 QQ 群连上，绑定人 {} 算作你本人。在 QQ 群里 @机器人 并写 @agent 名字来叫 agent。", m.author));
            self.agents_changed();
            if let Some(rest) = rest() {
                let _ = post(&rest, &To::Group(m.group.clone()), "", &format!("已和 Disco 群聊「{name}」连上 ✅\n@我 说话，组长会回答；想叫某个 agent，就在话里写 @它的名字。\n/help 查看用法")).await;
            }
            return;
        };
        let (author, mut text) = if m.bot {
            // Another Disco's bot speaking for its agents (or its owner).
            let Some((name, said)) = relayed(&m.content) else { return };
            (format!("{name}{}", crate::FOREIGN_TAG), said.to_owned())
        } else {
            let me = self.db().setting(&format!("{ME}{}", m.group));
            (if me.as_deref() == Some(m.author_id.as_str()) { "me".to_owned() } else { format!("{}{}", m.author, crate::QQ_TAG) }, m.content.clone())
        };
        // Files and pictures go to the group's shared space; the agent gets their paths.
        if !m.attachments.is_empty() && let (Some(rest), Ok(dir)) = (rest(), crate::mcp::space_dir(self, &format!("group:{gid}")).map(|d| d.join("files"))) {
            for a in &m.attachments {
                match rest.download(&a.url, crate::files::MAX_SEND).await.and_then(|b| crate::files::save_new(&dir, &a.name, &b)) {
                    Ok((path, size)) => text.push_str(&format!("\n📎 {}（{}）\n{}", crate::files::safe_name(&a.name), crate::files::human_size(size), path.display())),
                    Err(e) => text.push_str(&format!("\n📎 {}（没收到：{e}）", a.name)),
                }
            }
        }
        if text.trim().is_empty() { return; }
        // @ the bot without naming an agent = ask the lead.
        let group = self.db().list_groups().ok().and_then(|gs| gs.into_iter().find(|g| g.id == gid));
        if m.at_me && !m.bot && let Some(g) = &group && crate::mentions(&text, &g.members, None).is_empty()
            && let Some(lead) = g.lead_agent.clone().or_else(|| g.members.first().cloned()) && !text.trim_start().starts_with('/')
        {
            text = format!("@{lead} {text}");
        }
        self.group(gid, author, text, true).await;
    }

    /// The owner's message in the bot's private chat: to the agent it names (`@name …`,
    /// which becomes the current one) or the current one. Anyone else is ignored.
    async fn qq_direct(self: &Arc<Self>, m: Incoming) {
        let to = To::User(m.author_id.clone());
        // Every private message opens a passive-reply window, the owner's or not.
        lock(&REPLY).get_or_insert_with(HashMap::new).insert(to.clone(), (m.id.clone(), Instant::now(), 1));
        let say = |text: String| { let to = to.clone(); async move { if let Some(r) = rest() { let _ = post(&r, &to, "", &text).await; } } };
        let me = owner(self);
        // `绑定 1234` with the code from Settings → QQ「认领私聊」: this sender is the owner.
        let claimed = bind_code(&m.content).is_some_and(|code| {
            let mut codes = lock(&CODES);
            codes.retain(|c| c.2.elapsed() < CODE_TTL);
            codes.iter().position(|c| c.0 == code && c.1.is_empty()).map(|i| codes.remove(i)).is_some()
        });
        if claimed || me.is_none() {
            // The scan did not say who scanned (or the owner re-claims): this sender is the owner.
            let _ = self.db().set_setting(OWNER, &m.author_id);
            let _ = self.db().audit(&format!("qq:{}", m.author_id), None, "qq.owner", if claimed { "claimed with a code" } else { "first private message after binding" });
            lock(&DIAG).stranger.clear();
            self.agents_changed();
            if claimed {
                say("好了，这个私聊现在连着你的 Disco。发 /help 看有哪些 agent。".into()).await;
                return;
            }
        } else if me.as_deref() != Some(m.author_id.as_str()) {
            let _ = self.db().audit(&format!("qq:{}", m.author_id), None, "qq.ignored", "private message from someone other than the owner");
            lock(&DIAG).stranger = tail(&m.author_id);
            let first = { let mut t = lock(&TOLD); let f = !t.contains(&m.author_id); if f { t.push(m.author_id.clone()); } f };
            if first { say("这个机器人连着别人的 Disco，只接主人的私聊。如果你就是主人：在 Disco 设置 → QQ 点「认领私聊」，把显示的「绑定 1234」发给我。".into()).await; }
            return;
        }
        let agents = qq_agents(self);
        if agents.is_empty() {
            say("还没有同步到 QQ 的 agent。在 Disco 里导入 agent 时选「同步到 QQ」，或在 agent 的信息里改成 QQ。".into()).await;
            return;
        }
        let saved = self.db().setting(CUR).filter(|c| agents.contains(c));
        let current = saved.unwrap_or_else(|| agents[0].clone());
        if matches!(m.content.trim(), "/help" | "/帮助" | "/agents") {
            say(format!("同步到这里的 agent：{}\n现在在和 **{current}** 说话。以 @名字 开头就换成它，之后的消息都发给它。", agents.join("、"))).await;
            return;
        }
        let (agent, mut text, named) = match pick(&m.content, &agents) {
            Some((a, rest)) => (a, rest.to_owned(), true),
            None => (current, m.content.trim().to_owned(), false),
        };
        if named { let _ = self.db().set_setting(CUR, &agent); }
        let thread = format!("dm:{agent}");
        // Files and pictures go to the agent's space, like group files; it gets their paths.
        if !m.attachments.is_empty() && let (Some(rest), Ok(dir)) = (rest(), crate::mcp::space_dir(self, &thread).map(|d| d.join("files"))) {
            for a in &m.attachments {
                match rest.download(&a.url, crate::files::MAX_SEND).await.and_then(|b| crate::files::save_new(&dir, &a.name, &b)) {
                    Ok((path, size)) => text.push_str(&format!("\n📎 {}（{}）\n{}", crate::files::safe_name(&a.name), crate::files::human_size(size), path.display())),
                    Err(e) => text.push_str(&format!("\n📎 {}（没收到：{e}）", a.name)),
                }
            }
        }
        if text.trim().is_empty() {
            if named { say(format!("好的，接下来的消息都发给 **{agent}**。")).await; }
            return;
        }
        if self.paused.load(std::sync::atomic::Ordering::Relaxed) {
            self.post_message(&thread, "me", &text);
            say("Disco 已全部停止，先在 Disco 里点「恢复」。".into()).await;
            return;
        }
        self.dm(agent, "me".into(), text, true).await;
    }

    /// A button on an inbox card: only the owner may answer (in a group, the member who
    /// sent the bind code; in the private chat, the bot's owner).
    async fn qq_press(&self, m: Incoming) {
        let Some(rest) = rest() else { return };
        let _ = rest.ack(&m.id).await;
        let press = m.press.unwrap_or_default();
        let mut parts = press.splitn(3, ':');
        let (Some("disco"), Some(id), Some(pick)) = (parts.next(), parts.next(), parts.next()) else { return };
        let allowed = if m.group.is_empty() { owner(self) } else { self.db().setting(&format!("{ME}{}", m.group)) };
        if allowed.as_deref() != Some(m.author_id.as_str()) { return; }
        let option = self.inbox_option(id, pick);
        let _ = self.inbox_answer(id, crate::inbox::Answer { option, text: None }, "QQ").await;
    }
}

// ---------- commands

#[derive(Serialize)]
pub struct QqStatus {
    /// A bot is bound.
    bound: bool,
    status: String,
    name: String,
    /// While binding: the QR code (SVG) and the page it encodes.
    qr: Option<String>,
    url: Option<String>,
    /// The bot's private chat knows who its owner is.
    owner: bool,
    /// QQ groups tied to Disco groups.
    groups: usize,
    diag: Diag,
}

fn status(core: &Core) -> QqStatus {
    let qr = lock(&QR).as_ref().map(|(b, svg, _)| (svg.clone(), b.url()));
    // One db() lock per statement: two in one struct literal would deadlock.
    let bound = core.db().setting(SET_ON).as_deref() == Some("1");
    let name = core.db().setting(SET_NAME).unwrap_or_default();
    let owner = owner(core).is_some();
    let groups = core.db().settings_with_prefix(GROUP).map(|v| v.iter().filter(|(_, g)| !g.is_empty()).count()).unwrap_or(0);
    let status = lock(&STATUS).clone();
    let diag = lock(&DIAG).clone();
    QqStatus { bound, status, name, url: qr.as_ref().map(|q| q.1.clone()), qr: qr.map(|q| q.0), owner, groups, diag }
}

/// For the overview: "" when no bot is bound.
pub(crate) fn overview_status(core: &Core) -> String {
    if core.db().setting(SET_ON).as_deref() != Some("1") { return String::new(); }
    let s = lock(&STATUS).clone();
    if s.is_empty() { "连接中…".into() } else { s }
}

#[tauri::command]
pub fn qq_status(core: St<'_>) -> QqStatus { status(&core) }

/// Show a bind QR code and wait (in the background) for the scan: the bot is saved and
/// connected as soon as the user confirms in mobile QQ; an expired code is replaced.
#[tauri::command]
pub async fn qq_bind(core: St<'_>) -> CmdResult<QqStatus> {
    let http = reqwest::Client::new();
    let host = bind_host();
    let bind = disco_qq::bind_start(&http, host.as_deref()).await?;
    let svg = disco_qq::qr_svg(&bind.url())?;
    let id = QR_GEN.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
    *lock(&QR) = Some((bind.clone(), svg, id));
    let core2 = core.inner().clone();
    tauri::async_runtime::spawn(async move {
        let (mut bind, mut fails) = (bind, 0);
        let started = Instant::now();
        let live = || lock(&QR).as_ref().is_some_and(|q| q.2 == id);
        while live() && started.elapsed() < Duration::from_secs(900) {
            tokio::time::sleep(Duration::from_millis(if crate::selftest_mode() { 200 } else { 2000 })).await;
            if !live() { return; }
            match disco_qq::bind_poll(&http, host.as_deref(), &bind).await {
                Ok(BindState::Waiting) => fails = 0,
                Ok(BindState::Expired) => match disco_qq::bind_start(&http, host.as_deref()).await {
                    Ok(b) => {
                        let svg = disco_qq::qr_svg(&b.url()).unwrap_or_default();
                        if let Some(q) = lock(&QR).as_mut().filter(|q| q.2 == id) { *q = (b.clone(), svg, id); }
                        bind = b;
                        let _ = core2.app.emit("qq", "");
                    }
                    Err(e) => { set_status(&core2, e); break; }
                },
                Ok(BindState::Done { creds, owner }) => {
                    let r = save_bot(&core2, creds, owner).await;
                    *lock(&QR) = None;
                    match r { Ok(()) => connect(&core2), Err(e) => set_status(&core2, e) }
                    return;
                }
                Err(e) => { fails += 1; if fails >= 5 { set_status(&core2, e); break; } }
            }
        }
        if live() { *lock(&QR) = None; let _ = core2.app.emit("qq", ""); }
    });
    Ok(status(&core))
}

/// Check the scanned bot works, then keep it (keychain) and its name.
async fn save_bot(core: &Core, creds: Creds, owner: Option<String>) -> Result<(), String> {
    // The credentials must give a token; the name is a nicety (READY also carries it).
    let rest = Rest::new(creds.clone(), endpoints());
    rest.token().await.map_err(|e| format!("扫码拿到的机器人用不了：{e}"))?;
    let name = rest.me().await.unwrap_or_else(|_| "QQ 机器人".into());
    secret_set(VAULT, &serde_json::to_string(&creds).map_err(err)?)?;
    core.db().set_setting(SET_NAME, &name).map_err(err)?;
    core.db().set_setting(OWNER, owner.as_deref().unwrap_or_default()).map_err(err)?;
    core.db().set_setting(SET_ON, "1").map_err(err)?;
    Ok(())
}

#[tauri::command]
pub fn qq_bind_cancel(core: St<'_>) -> QqStatus {
    *lock(&QR) = None;
    status(&core)
}

/// Forget the bot (the QQ groups stay tied, ready for when a bot is bound again).
#[tauri::command]
pub fn qq_unbind(core: St<'_>) -> CmdResult<QqStatus> {
    disconnect();
    secret_del(VAULT);
    core.db().set_setting(SET_ON, "").map_err(err)?;
    core.db().set_setting(SET_NAME, "").map_err(err)?;
    core.db().set_setting(OWNER, "").map_err(err)?;
    *lock(&STATUS) = String::new();
    *lock(&DIAG) = Diag::default();
    *lock(&READY_NAME) = None;
    core.agents_changed();
    Ok(status(&core))
}

/// Tie a Disco group to a QQ group: a 4-digit code to send in the QQ group (also put in
/// the chat as a note). `unbind`: untie it instead.
#[tauri::command]
pub fn qq_group_code(core: St<'_>, group: String, unbind: Option<bool>) -> CmdResult<String> {
    let g = core.db().list_groups().map_err(err)?.into_iter().find(|g| g.id == group).ok_or("没有这个群聊")?;
    if unbind == Some(true) {
        if let Some(openid) = openid_of(&core, &group) { core.db().set_setting(&format!("{OF}{openid}"), "").map_err(err)?; }
        core.db().set_setting(&format!("{GROUP}{group}"), "").map_err(err)?;
        core.agents_changed();
        return Ok(String::new());
    }
    if !g.id.starts_with("local-") { return Err("这个群聊已经同步到 Discord 频道，QQ 群请新建一个群聊绑定".into()); }
    if core.db().setting(SET_ON).as_deref() != Some("1") { return Err("先在设置里扫码绑定 QQ 机器人".into()); }
    let code = {
        let mut codes = lock(&CODES);
        codes.retain(|c| c.2.elapsed() < CODE_TTL && c.1 != group);
        let mut n = (uuid::Uuid::new_v4().as_u128() % 9000 + 1000) as u32;
        while codes.iter().any(|c| c.0 == n.to_string()) { n = n % 9000 + 1000 + 1; }
        codes.push((n.to_string(), group.clone(), Instant::now()));
        n.to_string()
    };
    core.post_message(&format!("group:{group}"), "system", &format!("把 QQ 机器人拉进你的 QQ 群，在群里 @机器人 发送：绑定 {code}\n（10 分钟内有效；发送的人会被当作你本人）"));
    Ok(code)
}

/// A 4-digit code that makes whoever sends `绑定 <code>` to the bot privately its owner
/// (for when the scan did not say who scanned, or the owner's QQ changed).
#[tauri::command]
pub fn qq_claim_code(core: St<'_>) -> CmdResult<String> {
    if !bound(&core) { return Err("先在设置里扫码接入 QQ 机器人".into()); }
    let mut codes = lock(&CODES);
    codes.retain(|c| c.2.elapsed() < CODE_TTL && !c.1.is_empty());
    let mut n = (uuid::Uuid::new_v4().as_u128() % 9000 + 1000) as u32;
    while codes.iter().any(|c| c.0 == n.to_string()) { n = n % 9000 + 1000 + 1; }
    codes.push((n.to_string(), String::new(), Instant::now()));
    Ok(n.to_string())
}

/// Drop the connection and connect again (fresh token, fresh session).
#[tauri::command]
pub fn qq_reconnect(core: St<'_>) -> CmdResult<QqStatus> {
    if !bound(&core) { return Err("先在设置里扫码接入 QQ 机器人".into()); }
    connect(core.inner());
    Ok(status(&core))
}

/// Why messages of `thread` cannot reach QQ although it is meant to sync there (None = fine).
pub(crate) fn blocked(core: &Core, thread: &str) -> Option<String> {
    let agent = thread.strip_prefix("dm:")?;
    if agent_sync(core, agent) != "qq" { return None; }
    if rest().is_none() { return Some("QQ 机器人没有连上，回复没发到 QQ：到设置 → QQ 看状态，或点「一键诊断」。".into()); }
    if owner(core).is_none() { return Some("还不知道你的 QQ，回复没发到 QQ：用手机 QQ 给机器人发一条私聊（第一个私聊的人会被认作你本人）。".into()); }
    None
}

fn ago(ms: u64) -> String {
    let s = now_ms().saturating_sub(ms) / 1000;
    if s < 60 { format!("{s} 秒前") } else if s < 3600 { format!("{} 分钟前", s / 60) } else { format!("{} 小时前", s / 3600) }
}

/// Settings → 一键诊断: the QQ rows (none when no bot is bound).
pub(crate) async fn diagnose(core: &Core) -> Vec<(String, Result<String, String>)> {
    let mut out = Vec::new();
    if !bound(core) { return out; }
    let creds = secret_get(VAULT).and_then(|j| serde_json::from_str::<Creds>(&j).ok());
    let Some(creds) = creds else {
        out.push(("QQ 机器人凭据".into(), Err("钥匙串里读不到：请到设置 → QQ 解绑后重新扫码".into())));
        return out;
    };
    out.push(("QQ 机器人凭据".into(), Ok(format!("AppID {}", creds.app_id))));
    let rest = Rest::new(creds, endpoints());
    let token = rest.token().await;
    out.push(("QQ access_token".into(), token.as_ref().map(|_| if rest.via_proxy() { "可以取到（经系统代理）".to_owned() } else { "可以取到".to_owned() }).map_err(Clone::clone)));
    let d = lock(&DIAG).clone();
    let live = if d.ready {
        Ok(format!("在线，{}连上{}", ago(d.ready_at), if d.proxy { "（经系统代理）" } else { "" }))
    } else if token.is_ok() {
        // The running connection is down: a separate one to READY shows where it stops.
        match disco_qq::gateway_check(&rest, Duration::from_secs(15)).await {
            Ok((name, took, proxy)) => Ok(format!("单独测试连上了（{name}，{} ms{}）；后台连接现在是「{}」{}", took.as_millis(), if proxy { "，经系统代理" } else { "" },
                if d.stage.is_empty() { "未启动" } else { d.stage.as_str() }, if d.error.is_empty() { String::new() } else { format!("，上次错误：{}", d.error) })),
            Err(e) => Err(format!("{e}{}", if d.error.is_empty() || d.error == e { String::new() } else { format!("（后台连接上次错误：{}）", d.error) })),
        }
    } else {
        Err(format!("没有 access_token，连不了网关{}", if d.error.is_empty() { String::new() } else { format!("（{}）", d.error) }))
    };
    out.push(("QQ 实时连接".into(), live));
    out.push(("QQ 收到的事件".into(), if d.events > 0 { Ok(format!("共 {} 个，最近 {}（{}）", d.events, d.event, ago(d.event_at))) }
        else { Err("这次启动后还没收到任何 QQ 事件：QQ 群里要 @机器人 才会推送；私聊直接发给机器人".into()) }));
    let own = owner(core);
    out.push(("QQ 私聊主人".into(), match (own, d.stranger.is_empty()) {
        (Some(o), true) => Ok(format!("已认定（openid …{}）", tail(&o))),
        (Some(o), false) => Err(format!("收到 …{} 的私聊，但主人是 …{}：如果那就是你，在设置 → QQ 点「认领私聊」", d.stranger, tail(&o))),
        (None, _) => Err("还不知道你的 QQ：用手机 QQ 给机器人发一条私聊".into()),
    }));
    out.push(("QQ 发送".into(), if d.send_error.is_empty() { Ok("没有失败记录".into()) } else { Err(format!("{}：{}", ago(d.send_error_at), d.send_error)) }));
    let summary: Vec<String> = out.iter().map(|(n, r)| format!("{n}={}", if r.is_ok() { "ok" } else { "fail" })).collect();
    let _ = core.db().audit("qq", None, "qq.diagnose", &summary.join(" "));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_relays_and_reply_slots() {
        assert_eq!(bind_code(" 绑定 1234 "), Some("1234"));
        assert_eq!(bind_code("/bind 0042"), Some("0042"));
        assert_eq!(bind_code("绑定 12345"), None);
        assert_eq!(bind_code("我想绑定 1234"), None);
        assert_eq!(relayed("**claude**：好的"), Some(("claude", "好的")));
        assert_eq!(relayed("**a\nb**：x"), None);
        assert_eq!(relayed("claude：x"), None);
        let g = To::Group("G-test".into());
        lock(&REPLY).get_or_insert_with(HashMap::new).insert(g.clone(), ("m1".into(), Instant::now(), 1));
        assert_eq!(reply_slot(&g, 2), Some(("m1".into(), 1)));
        assert_eq!(reply_slot(&g, 3), Some(("m1".into(), 3)));
        assert_eq!(reply_slot(&g, 1), None, "a sixth reply must be an active message");
        assert_eq!(reply_slot(&To::Group("nope".into()), 1), None);
        // Private chats: same five, a longer window; an old message gets none.
        let u = To::User("O-test".into());
        lock(&REPLY).get_or_insert_with(HashMap::new).insert(u.clone(), ("c1".into(), Instant::now() - Duration::from_secs(600), 1));
        assert_eq!(reply_slot(&u, 1), Some(("c1".into(), 1)), "10 minutes is still inside the private-chat window");
        lock(&REPLY).get_or_insert_with(HashMap::new).insert(u.clone(), ("c1".into(), Instant::now() - Duration::from_secs(3600), 1));
        assert_eq!(reply_slot(&u, 1), None);
        assert!(first_time("x-1") && !first_time("x-1") && first_time(""));
    }

    #[test]
    fn picks_the_named_agent() {
        let agents = vec!["claude".to_owned(), "claude-2".to_owned(), "写手".to_owned()];
        assert_eq!(pick("@claude 看下", &agents), Some(("claude".into(), "看下")));
        assert_eq!(pick("@claude-2：继续", &agents), Some(("claude-2".into(), "继续")));
        assert_eq!(pick("@Claude", &agents), Some(("claude".into(), "")));
        assert_eq!(pick("@写手帮我写", &agents), Some(("写手".into(), "帮我写")));
        assert_eq!(pick("@claudex hi", &agents), None);
        assert_eq!(pick("hi @claude", &agents), None);
        assert_eq!(pick("＠claude hi", &agents), Some(("claude".into(), "hi")));
    }
}
