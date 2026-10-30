//! QQ as a second place for group chats, through the official QQ Bot API (crate disco-qq).
//!
//! Set-up is one scan: Settings shows q.qq.com's bind QR code, the user scans it with
//! mobile QQ and picks or creates a bot; the AppID and AppSecret come back encrypted and
//! go straight to the OS keychain. A Disco group is tied to a QQ group by sending the
//! code Disco shows (`绑定 1234`) in that group, which also tells Disco which group
//! member is its owner. From then on the group works like a Discord-synced one: one bot
//! speaks for every local agent as `**name**：text`, people @ the bot and name an agent.
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use disco_qq::{Bind, BindState, Creds, Endpoints, Incoming, Kind, Rest};
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
/// A bind code is valid this long.
const CODE_TTL: Duration = Duration::from_secs(600);
/// QQ accepts passive replies to a message for 5 minutes, at most 5 of them.
const REPLY_TTL: Duration = Duration::from_secs(290);
const REPLY_MAX: u32 = 5;

struct Conn { rest: Rest, stop: watch::Sender<bool> }
static CONN: Mutex<Option<Conn>> = Mutex::new(None);
static STATUS: Mutex<String> = Mutex::new(String::new());
/// The bind task on screen: (task, its QR code as SVG, generation).
static QR: Mutex<Option<(Bind, String, u64)>> = Mutex::new(None);
static QR_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Bind codes waiting to be sent in a QQ group: (code, Disco group id, issued).
static CODES: Mutex<Vec<(String, String, Instant)>> = Mutex::new(Vec::new());
/// Per QQ group: the newest message to reply to, when it came, the next sequence number.
type Replies = HashMap<String, (String, Instant, u32)>;
static REPLY: Mutex<Option<Replies>> = Mutex::new(None);
/// Message ids already handled (QQ may deliver one twice).
static SEEN: Mutex<VecDeque<String>> = Mutex::new(VecDeque::new());
/// Test endpoints (selftest only): API + token, and the bind host.
static AT: Mutex<Option<(Endpoints, String)>> = Mutex::new(None);

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> { m.lock().unwrap_or_else(|p| p.into_inner()) }

#[cfg(debug_assertions)]
pub(crate) fn use_endpoints(api: &str, bind_host: &str) {
    *lock(&AT) = Some((Endpoints { api: api.into(), token: format!("{api}/app/getAppAccessToken") }, bind_host.into()));
}

fn endpoints() -> Endpoints { lock(&AT).as_ref().map(|a| a.0.clone()).unwrap_or_default() }
fn bind_host() -> Option<String> { lock(&AT).as_ref().map(|a| a.1.clone()) }

pub(crate) fn rest() -> Option<Rest> { lock(&CONN).as_ref().map(|c| c.rest.clone()) }

/// The QQ group a Disco group is tied to.
pub(crate) fn openid_of(core: &Core, group: &str) -> Option<String> {
    core.db().setting(&format!("{GROUP}{group}")).filter(|s| !s.is_empty())
}

fn group_of(core: &Core, openid: &str) -> Option<String> {
    core.db().setting(&format!("{OF}{openid}")).filter(|s| !s.is_empty())
}

/// Where to post a thread's messages in QQ: the bot and the QQ group.
pub(crate) fn target(core: &Core, thread: &str) -> Option<(Rest, String)> {
    let openid = openid_of(core, thread.strip_prefix("group:")?)?;
    Some((rest()?, openid))
}

/// Reserve `n` passive replies to the group's newest message, if QQ still allows them.
fn reply_slot(openid: &str, n: u32) -> Option<(String, u32)> {
    let mut g = lock(&REPLY);
    let e = g.get_or_insert_with(HashMap::new).get_mut(openid)?;
    if e.1.elapsed() > REPLY_TTL || e.2 + n > REPLY_MAX + 1 { return None; }
    let seq = e.2;
    e.2 += n;
    Some((e.0.clone(), seq))
}

/// Post into a group's QQ group as `**who**：text` (or just `text` when `who` is empty).
pub(crate) async fn post(rest: &Rest, openid: &str, who: &str, text: &str) -> Result<(), String> {
    let body = if who.is_empty() { text.to_owned() } else { format!("**{who}**：{text}") };
    let n = disco_qq::chunk(&body).len() as u32;
    let slot = reply_slot(openid, n);
    rest.send_group(openid, &body, slot.as_ref().map(|(id, s)| (id.as_str(), *s)), &[]).await.map(|_| ())
}

/// A file in the group's QQ group.
pub(crate) async fn post_file(rest: &Rest, openid: &str, name: &str, bytes: &[u8]) -> Result<(), String> {
    let slot = reply_slot(openid, 1);
    rest.send_group_file(openid, name, bytes, slot.as_ref().map(|(id, s)| (id.as_str(), *s))).await.map(|_| ())
}

/// A card with buttons (an inbox item) in the group's QQ group.
pub(crate) async fn post_card(rest: &Rest, openid: &str, text: &str, buttons: &[disco_qq::Button]) -> Result<(), String> {
    let slot = reply_slot(openid, disco_qq::chunk(text).len() as u32);
    rest.send_group(openid, text, slot.as_ref().map(|(id, s)| (id.as_str(), *s)), buttons).await.map(|_| ())
}

fn set_status(core: &Core, s: String) {
    *lock(&STATUS) = s.clone();
    let _ = core.app.emit("qq", s);
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
    tauri::async_runtime::spawn(disco_qq::run_gateway(rest, tx, stop_rx, move |s| set_status(&c, s)));
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
            _ => {}
        }
    }

    async fn qq_group_message(self: &Arc<Self>, m: Incoming) {
        lock(&REPLY).get_or_insert_with(HashMap::new).insert(m.group.clone(), (m.id.clone(), Instant::now(), 1));
        let Some(gid) = group_of(self, &m.group) else {
            // Not tied to a Disco group yet: only a bind code means anything here.
            let Some(code) = bind_code(&m.content) else { return };
            let found = {
                let mut codes = lock(&CODES);
                codes.retain(|c| c.2.elapsed() < CODE_TTL);
                codes.iter().position(|c| c.0 == code).map(|i| codes.remove(i).1)
            };
            let Some(gid) = found else { return };
            let name = self.db().list_groups().ok().and_then(|gs| gs.into_iter().find(|g| g.id == gid)).map(|g| g.name).unwrap_or_default();
            let _ = self.db().set_setting(&format!("{GROUP}{gid}"), &m.group);
            let _ = self.db().set_setting(&format!("{OF}{}", m.group), &gid);
            let _ = self.db().set_setting(&format!("{ME}{}", m.group), &m.author_id);
            self.post_message(&format!("group:{gid}"), "system", &format!("已和 QQ 群连上，绑定人 {} 算作你本人。在 QQ 群里 @机器人 并写 @agent 名字来叫 agent。", m.author));
            self.agents_changed();
            if let Some(rest) = rest() {
                let _ = post(&rest, &m.group, "", &format!("已和 Disco 群聊「{name}」连上 ✅\n@我 说话，组长会回答；想叫某个 agent，就在话里写 @它的名字。\n/help 查看用法")).await;
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

    /// A button on an inbox card: only the group's owner may answer.
    async fn qq_press(&self, m: Incoming) {
        let Some(rest) = rest() else { return };
        let _ = rest.ack(&m.id).await;
        let press = m.press.unwrap_or_default();
        let mut parts = press.splitn(3, ':');
        let (Some("disco"), Some(id), Some(pick)) = (parts.next(), parts.next(), parts.next()) else { return };
        if self.db().setting(&format!("{ME}{}", m.group)).as_deref() != Some(m.author_id.as_str()) { return; }
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
}

fn status(core: &Core) -> QqStatus {
    let qr = lock(&QR).as_ref().map(|(b, svg, _)| (svg.clone(), b.url()));
    // One db() lock per statement: two in one struct literal would deadlock.
    let bound = core.db().setting(SET_ON).as_deref() == Some("1");
    let name = core.db().setting(SET_NAME).unwrap_or_default();
    let status = lock(&STATUS).clone();
    QqStatus { bound, status, name, url: qr.as_ref().map(|q| q.1.clone()), qr: qr.map(|q| q.0) }
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
                Ok(BindState::Done { creds, owner: _ }) => {
                    let r = save_bot(&core2, creds).await;
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
async fn save_bot(core: &Core, creds: Creds) -> Result<(), String> {
    let name = Rest::new(creds.clone(), endpoints()).me().await?;
    secret_set(VAULT, &serde_json::to_string(&creds).map_err(err)?)?;
    core.db().set_setting(SET_NAME, &name).map_err(err)?;
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
    *lock(&STATUS) = String::new();
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
        lock(&REPLY).get_or_insert_with(HashMap::new).insert("G-test".into(), ("m1".into(), Instant::now(), 1));
        assert_eq!(reply_slot("G-test", 2), Some(("m1".into(), 1)));
        assert_eq!(reply_slot("G-test", 3), Some(("m1".into(), 3)));
        assert_eq!(reply_slot("G-test", 1), None, "a sixth reply must be an active message");
        assert_eq!(reply_slot("nope", 1), None);
        assert!(first_time("x-1") && !first_time("x-1") && first_time(""));
    }
}
