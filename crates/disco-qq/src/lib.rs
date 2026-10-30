//! QQ adapter for the official QQ Bot API (q.qq.com open platform): one-scan bot
//! binding, REST over reqwest and a small WebSocket gateway client.
//!
//! Binding is the open platform's own "快捷绑定" (the flow behind the official
//! `@tencent-connect/qqbot-connector` SDK): Disco asks q.qq.com for a bind task with a
//! random AES key, shows the task's page as a QR code, the user scans it with mobile
//! QQ and picks (or creates) a bot, and the poll returns the bot's AppID and its
//! AppSecret encrypted with that key. Nothing to copy or paste.
#![forbid(unsafe_code)]

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use futures::{SinkExt as _, StreamExt as _};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::Message;

const BIND_HOST: &str = "https://q.qq.com";
const API: &str = "https://api.sgroup.qq.com";
const TOKEN_URL: &str = "https://bots.qq.com/app/getAppAccessToken";
/// GROUP_AND_C2C_EVENT | INTERACTION
const INTENTS: u64 = (1 << 25) | (1 << 26);
/// Longest text we put in one QQ message.
const CHUNK: usize = 1800;

fn b64() -> base64::engine::GeneralPurpose { base64::engine::general_purpose::STANDARD }

/// A bot's credentials.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Creds { pub app_id: String, pub secret: String }

// ---------------------------------------------------------------- one-scan binding

/// A bind task on q.qq.com: show [`Bind::url`] as a QR code and poll it.
#[derive(Clone, Debug)]
pub struct Bind { pub task_id: String, key: String }

#[derive(Debug, PartialEq)]
pub enum BindState {
    Waiting,
    /// The code timed out: start a new task.
    Expired,
    /// Scanned and confirmed: the bot, and the scanner's QQ openid (the owner).
    Done { creds: Creds, owner: Option<String> },
}

impl Bind {
    /// The page mobile QQ opens from the QR code.
    pub fn url(&self) -> String {
        format!("{BIND_HOST}/qqbot/openclaw/connect.html?task_id={}&source=&_wv=2", urlencode(&self.task_id))
    }
}

fn urlencode(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

async fn post_json(http: &reqwest::Client, url: &str, body: &Value) -> Result<Value, String> {
    let r = http.post(url).json(body).timeout(Duration::from_secs(15)).send().await.map_err(|e| format!("连不上 QQ 开放平台：{e}"))?;
    if !r.status().is_success() { return Err(format!("QQ 开放平台返回 HTTP {}", r.status())); }
    r.json().await.map_err(|e| e.to_string())
}

/// Start a bind task (`host` is q.qq.com outside tests).
pub async fn bind_start(http: &reqwest::Client, host: Option<&str>) -> Result<Bind, String> {
    let key = b64().encode(ring::rand::generate::<[u8; 32]>(&ring::rand::SystemRandom::new()).map_err(|_| "随机数失败")?.expose());
    let v = post_json(http, &format!("{}/lite/create_bind_task", host.unwrap_or(BIND_HOST)), &json!({ "key": key })).await?;
    if v["retcode"].as_i64() != Some(0) { return Err(v["msg"].as_str().unwrap_or("创建绑定任务失败").to_owned()); }
    let task_id = v["data"]["task_id"].as_str().filter(|t| !t.is_empty()).ok_or("创建绑定任务失败：没有 task_id")?.to_owned();
    Ok(Bind { task_id, key })
}

/// Poll a bind task once.
pub async fn bind_poll(http: &reqwest::Client, host: Option<&str>, bind: &Bind) -> Result<BindState, String> {
    let v = post_json(http, &format!("{}/lite/poll_bind_result", host.unwrap_or(BIND_HOST)), &json!({ "task_id": bind.task_id })).await?;
    if v["retcode"].as_i64() != Some(0) { return Err(v["msg"].as_str().unwrap_or("查询绑定结果失败").to_owned()); }
    let d = &v["data"];
    Ok(match d["status"].as_i64().unwrap_or(0) {
        2 => {
            let app_id = match &d["bot_appid"] { Value::String(s) => s.clone(), n => n.to_string() };
            let secret = decrypt(d["bot_encrypt_secret"].as_str().unwrap_or_default(), &bind.key)?;
            if app_id.is_empty() || app_id == "0" || secret.is_empty() { return Err("绑定结果不完整".into()); }
            BindState::Done { creds: Creds { app_id, secret }, owner: d["user_openid"].as_str().filter(|s| !s.is_empty()).map(str::to_owned) }
        }
        3 => BindState::Expired,
        _ => BindState::Waiting,
    })
}

/// AES-256-GCM, base64 of IV(12) + ciphertext + tag(16), key = base64 of 32 bytes.
pub fn decrypt(enc_b64: &str, key_b64: &str) -> Result<String, String> {
    use ring::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
    let bad = || "AppSecret 解密失败".to_owned();
    let key = b64().decode(key_b64).map_err(|_| bad())?;
    let data = b64().decode(enc_b64).map_err(|_| bad())?;
    if data.len() < 12 + 16 { return Err(bad()); }
    let key = LessSafeKey::new(UnboundKey::new(&AES_256_GCM, &key).map_err(|_| bad())?);
    let nonce = Nonce::try_assume_unique_for_key(&data[..12]).map_err(|_| bad())?;
    let mut buf = data[12..].to_vec();
    let plain = key.open_in_place(nonce, Aad::empty(), &mut buf).map_err(|_| bad())?;
    String::from_utf8(plain.to_vec()).map_err(|_| bad())
}

/// The other half of [`decrypt`], as q.qq.com does it (for mock servers in tests).
pub fn encrypt(plain: &str, key_b64: &str) -> Result<String, String> {
    use ring::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
    let key = b64().decode(key_b64).map_err(|e| e.to_string())?;
    let key = LessSafeKey::new(UnboundKey::new(&AES_256_GCM, &key).map_err(|_| "bad key")?);
    let iv = ring::rand::generate::<[u8; 12]>(&ring::rand::SystemRandom::new()).map_err(|_| "rand")?.expose();
    let mut buf = plain.as_bytes().to_vec();
    key.seal_in_place_append_tag(Nonce::assume_unique_for_key(iv), Aad::empty(), &mut buf).map_err(|_| "seal")?;
    Ok(b64().encode([iv.as_slice(), &buf].concat()))
}

/// The QR code of `text` as an SVG document (dark modules on white).
pub fn qr_svg(text: &str) -> Result<String, String> {
    let code = qrcode::QrCode::new(text.as_bytes()).map_err(|e| e.to_string())?;
    Ok(code.render::<qrcode::render::svg::Color>().min_dimensions(200, 200).quiet_zone(true).build())
}

// ---------------------------------------------------------------- REST

/// Where the API lives (overridden only by tests).
#[derive(Clone, Debug)]
pub struct Endpoints { pub api: String, pub token: String }

impl Default for Endpoints {
    fn default() -> Self { Self { api: API.into(), token: TOKEN_URL.into() } }
}

#[derive(Clone)]
pub struct Rest {
    http: reqwest::Client,
    creds: Creds,
    at: Endpoints,
    token: Arc<Mutex<Option<(String, Instant)>>>,
}

/// One button of an inline keyboard: (button data, label, primary).
pub type Button = (String, String, bool);

impl Rest {
    pub fn new(creds: Creds, at: Endpoints) -> Self {
        Self { http: reqwest::Client::builder().connect_timeout(Duration::from_secs(15)).build().unwrap_or_default(), creds, at, token: Arc::default() }
    }

    pub fn app_id(&self) -> &str { &self.creds.app_id }

    async fn token(&self) -> Result<String, String> {
        if let Some((t, until)) = self.token.lock().unwrap_or_else(|p| p.into_inner()).clone() && Instant::now() < until { return Ok(t); }
        let v = post_json(&self.http, &self.at.token, &json!({ "appId": self.creds.app_id, "clientSecret": self.creds.secret })).await?;
        let t = v["access_token"].as_str().filter(|t| !t.is_empty()).ok_or_else(|| format!("AppSecret 无效：{}", v["message"].as_str().unwrap_or("拿不到 access_token")))?.to_owned();
        let secs = match &v["expires_in"] { Value::String(s) => s.parse().unwrap_or(7200), n => n.as_u64().unwrap_or(7200) };
        // Refresh a minute early.
        *self.token.lock().unwrap_or_else(|p| p.into_inner()) = Some((t.clone(), Instant::now() + Duration::from_secs(secs.saturating_sub(60).max(30))));
        Ok(t)
    }

    async fn call(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Result<Value, String> {
        let token = self.token().await?;
        let mut req = self.http.request(method, format!("{}{path}", self.at.api)).header("Authorization", format!("QQBot {token}")).timeout(Duration::from_secs(60));
        if let Some(b) = body { req = req.json(&b); }
        let r = req.send().await.map_err(|e| format!("连不上 QQ：{e}"))?;
        let status = r.status();
        let text = r.text().await.unwrap_or_default();
        let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if !status.is_success() || v.get("code").and_then(Value::as_i64).is_some_and(|c| c != 0) {
            let msg = v["message"].as_str().map(str::to_owned).unwrap_or_else(|| text.chars().take(200).collect());
            return Err(format!("QQ 返回 {}：{msg}", v["code"].as_i64().map_or(status.as_u16().to_string(), |c| c.to_string())));
        }
        Ok(v)
    }

    /// The bot's name.
    pub async fn me(&self) -> Result<String, String> {
        Ok(self.call(reqwest::Method::GET, "/users/@me", None).await?["username"].as_str().unwrap_or("QQ 机器人").to_owned())
    }

    async fn gateway_url(&self) -> Result<String, String> {
        self.call(reqwest::Method::GET, "/gateway", None).await?["url"].as_str().map(str::to_owned).ok_or("拿不到 QQ 网关地址".into())
    }

    /// Post `text` (markdown) to a group, in pieces when long. `reply` = the message
    /// being answered and the first sequence number to use (a passive reply); none = an
    /// active message. Buttons go with the last piece. Falls back to plain text when the
    /// bot may not send markdown.
    pub async fn send_group(&self, group: &str, text: &str, reply: Option<(&str, u32)>, buttons: &[Button]) -> Result<u32, String> {
        let mut seq = reply.map_or(1, |r| r.1);
        let parts = chunk(text);
        let n = parts.len();
        for (i, part) in parts.into_iter().enumerate() {
            let mut body = json!({ "msg_type": 2, "markdown": { "content": part }, "msg_seq": seq });
            if let Some((id, _)) = reply { body["msg_id"] = json!(id); }
            if i + 1 == n && !buttons.is_empty() { body["keyboard"] = keyboard(buttons); }
            let path = format!("/v2/groups/{group}/messages");
            if let Err(e) = self.call(reqwest::Method::POST, &path, Some(body.clone())).await {
                // No markdown permission: plain text (buttons need markdown, so they go).
                let mut plain = json!({ "msg_type": 0, "content": part, "msg_seq": seq + 1 });
                if let Some((id, _)) = reply { plain["msg_id"] = json!(id); }
                seq += 1;
                self.call(reqwest::Method::POST, &path, Some(plain)).await.map_err(|e2| format!("{e2}（markdown：{e}）"))?;
            }
            seq += 1;
        }
        Ok(seq)
    }

    /// Upload a file (≤ the caller's limit) to a group and post it. `kind`: 1 image, 2 video, 3 voice, 4 file.
    pub async fn send_group_file(&self, group: &str, name: &str, bytes: &[u8], reply: Option<(&str, u32)>) -> Result<u32, String> {
        let lower = name.to_ascii_lowercase();
        let kind = if [".png", ".jpg", ".jpeg", ".gif", ".webp"].iter().any(|e| lower.ends_with(e)) { 1 } else if [".mp4", ".mov"].iter().any(|e| lower.ends_with(e)) { 2 } else { 4 };
        let up = self.call(reqwest::Method::POST, &format!("/v2/groups/{group}/files"),
            Some(json!({ "file_type": kind, "file_data": b64().encode(bytes), "file_name": name, "srv_send_msg": false }))).await?;
        let info = up["file_info"].as_str().ok_or("上传文件失败：没有 file_info")?;
        let seq = reply.map_or(1, |r| r.1);
        let mut body = json!({ "msg_type": 7, "media": { "file_info": info }, "msg_seq": seq });
        if let Some((id, _)) = reply { body["msg_id"] = json!(id); }
        self.call(reqwest::Method::POST, &format!("/v2/groups/{group}/messages"), Some(body)).await?;
        Ok(seq + 1)
    }

    /// Acknowledge a button press (QQ shows a failure if it is not answered).
    pub async fn ack(&self, interaction: &str) -> Result<(), String> {
        self.call(reqwest::Method::PUT, &format!("/interactions/{interaction}"), Some(json!({ "code": 0 }))).await.map(|_| ())
    }

    /// Download an attachment (at most `max` bytes).
    pub async fn download(&self, url: &str, max: u64) -> Result<Vec<u8>, String> {
        let mut r = self.http.get(url).timeout(Duration::from_secs(120)).send().await.map_err(|e| e.to_string())?;
        if !r.status().is_success() { return Err(format!("HTTP {}", r.status())); }
        let mut out = Vec::new();
        while let Some(c) = r.chunk().await.map_err(|e| e.to_string())? {
            out.extend_from_slice(&c);
            if out.len() as u64 > max { return Err("文件太大".into()); }
        }
        Ok(out)
    }
}

/// QQ inline keyboard: one row per three buttons; callback buttons (INTERACTION_CREATE
/// carries the data), one click each, all in one group so the others grey out.
fn keyboard(buttons: &[Button]) -> Value {
    let rows: Vec<Value> = buttons.chunks(3).map(|row| json!({ "buttons": row.iter().enumerate().map(|(i, (data, label, primary))| json!({
        "id": format!("{i}-{data}"),
        "render_data": { "label": label, "visited_label": format!("已选：{label}"), "style": if *primary { 1 } else { 0 } },
        "action": { "type": 1, "data": data, "permission": { "type": 2 }, "click_limit": 1 },
        "group_id": "disco",
    })).collect::<Vec<_>>() })).collect();
    json!({ "content": { "rows": rows } })
}

/// Split at line ends where possible, never inside a UTF-8 character.
pub fn chunk(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for line in text.split_inclusive('\n') {
        if cur.chars().count() + line.chars().count() > CHUNK && !cur.is_empty() { out.push(std::mem::take(&mut cur)); }
        if line.chars().count() > CHUNK {
            let cs: Vec<char> = line.chars().collect();
            for piece in cs.chunks(CHUNK) { out.push(piece.iter().collect()); }
        } else {
            cur.push_str(line);
        }
    }
    if !cur.trim().is_empty() || out.is_empty() { out.push(cur); }
    out
}

// ---------------------------------------------------------------- gateway events

#[derive(Debug, Clone, PartialEq)]
pub enum Kind { Group, Direct, Press, Added, Removed }

#[derive(Debug, Clone, PartialEq)]
pub struct Attachment { pub url: String, pub name: String, pub size: u64 }

#[derive(Debug, Clone, PartialEq)]
pub struct Incoming {
    pub kind: Kind,
    /// Group openid (or the user's openid in a direct chat).
    pub group: String,
    /// The sender's openid in that group (`member_openid`), or the presser's.
    pub author_id: String,
    pub author: String,
    /// Sent by a bot (another Disco).
    pub bot: bool,
    /// Text with mentions written as `@Name` and our own @ removed.
    pub content: String,
    /// The message id (for passive replies) or the interaction id.
    pub id: String,
    /// This bot was @'d.
    pub at_me: bool,
    pub attachments: Vec<Attachment>,
    /// A button press: its data.
    pub press: Option<String>,
}

fn s(v: &Value) -> String { v.as_str().unwrap_or_default().to_owned() }

/// One gateway dispatch (`t`, `d`) as Disco needs it; None for anything else.
pub fn parse(t: &str, d: &Value) -> Option<Incoming> {
    let base = |kind| Incoming { kind, group: s(&d["group_openid"]), author_id: String::new(), author: String::new(), bot: false,
        content: String::new(), id: s(&d["id"]), at_me: false, attachments: Vec::new(), press: None };
    match t {
        "GROUP_AT_MESSAGE_CREATE" | "GROUP_MESSAGE_CREATE" | "C2C_MESSAGE_CREATE" => {
            let a = &d["author"];
            let mentions = d["mentions"].as_array().cloned().unwrap_or_default();
            let at_me = t == "GROUP_AT_MESSAGE_CREATE" || mentions.iter().any(|m| m["is_you"].as_bool() == Some(true));
            let mut content = s(&d["content"]);
            for m in &mentions {
                let id = [&m["member_openid"], &m["id"], &m["user_openid"]].into_iter().find_map(|v| v.as_str().filter(|s| !s.is_empty())).unwrap_or_default();
                if id.is_empty() { continue; }
                let with = if m["is_you"].as_bool() == Some(true) { String::new() } else {
                    let name = [&m["nickname"], &m["username"]].into_iter().find_map(|v| v.as_str().filter(|s| !s.is_empty())).unwrap_or(id);
                    format!("@{name}")
                };
                content = content.replace(&format!("<@!{id}>"), &with).replace(&format!("<@{id}>"), &with);
            }
            let attachments = d["attachments"].as_array().map(|a| a.iter().filter_map(|x| {
                let url = s(&x["url"]);
                if url.is_empty() { return None; }
                let url = if url.starts_with("//") { format!("https:{url}") } else if url.starts_with("http") { url } else { format!("https://{url}") };
                Some(Attachment { url, name: x["filename"].as_str().filter(|n| !n.is_empty()).unwrap_or("file").to_owned(), size: x["size"].as_u64().unwrap_or(0) })
            }).collect()).unwrap_or_default();
            let (kind, group) = if t == "C2C_MESSAGE_CREATE" { (Kind::Direct, s(&a["user_openid"])) } else { (Kind::Group, s(&d["group_openid"])) };
            let author_id = [&a["member_openid"], &a["user_openid"], &a["id"]].into_iter().find_map(|v| v.as_str().filter(|s| !s.is_empty())).unwrap_or_default().to_owned();
            let tail: String = author_id.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
            let author = [&a["username"], &a["nickname"]].into_iter().find_map(|v| v.as_str().filter(|s| !s.is_empty())).map_or_else(|| format!("QQ 用户{tail}"), str::to_owned);
            Some(Incoming { kind, group, author_id, author, bot: a["bot"].as_bool() == Some(true), content: content.trim().to_owned(), at_me, attachments, ..base(Kind::Group) })
        }
        "INTERACTION_CREATE" => {
            let data = d["data"]["resolved"]["button_data"].as_str().map(str::to_owned)?;
            let who = [&d["group_member_openid"], &d["user_openid"]].into_iter().find_map(|v| v.as_str().filter(|s| !s.is_empty())).unwrap_or_default().to_owned();
            Some(Incoming { author_id: who, press: Some(data), ..base(Kind::Press) })
        }
        "GROUP_ADD_ROBOT" => Some(Incoming { author_id: s(&d["op_member_openid"]), ..base(Kind::Added) }),
        "GROUP_DEL_ROBOT" => Some(Incoming { author_id: s(&d["op_member_openid"]), ..base(Kind::Removed) }),
        _ => None,
    }
}

// ---------------------------------------------------------------- gateway client

enum End { Stopped, Fatal(String), Retry(String) }

#[derive(Default)]
struct Resume { session: String, seq: Option<u64> }

/// Connect, identify (or resume), keep the heartbeat, hand every event to `tx`;
/// reconnect with backoff until `stop` fires. `on_status` gets a short Chinese line.
pub async fn run_gateway(rest: Rest, tx: mpsc::Sender<Incoming>, mut stop: watch::Receiver<bool>, on_status: impl Fn(String) + Send + 'static) {
    let mut backoff = 2u64;
    let mut resume = Resume::default();
    loop {
        on_status("连接中…".into());
        let mut ready = false;
        let end = session(&rest, &tx, &mut stop, &mut resume, &mut ready, &on_status).await;
        match end {
            End::Stopped => { on_status("已断开".into()); return; }
            End::Fatal(e) => { on_status(e); return; }
            End::Retry(e) => {
                if ready { backoff = 2; }
                on_status(format!("{e}，{backoff} 秒后重试"));
                tokio::select! {
                    _ = stop.changed() => { on_status("已断开".into()); return; }
                    _ = tokio::time::sleep(Duration::from_secs(backoff)) => {}
                }
                backoff = (backoff * 2).min(60);
            }
        }
    }
}

async fn session(rest: &Rest, tx: &mpsc::Sender<Incoming>, stop: &mut watch::Receiver<bool>, resume: &mut Resume, ready: &mut bool, on_status: &(impl Fn(String) + Send)) -> End {
    let url = match rest.gateway_url().await {
        Ok(u) => u,
        Err(e) if e.contains("AppSecret") || e.contains("401") => return End::Fatal(format!("QQ 机器人凭据无效，请重新扫码绑定（{e}）")),
        Err(e) => return End::Retry(e),
    };
    let token = match rest.token().await { Ok(t) => t, Err(e) => return End::Retry(e) };
    let (ws, _) = match tokio::time::timeout(Duration::from_secs(30), tokio_tungstenite::connect_async(url.as_str())).await {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => return End::Retry(format!("连不上 QQ 网关：{e}")),
        Err(_) => return End::Retry("连接 QQ 网关超时".into()),
    };
    let (mut w, mut r) = ws.split();
    let mut beat = tokio::time::interval(Duration::from_secs(3600));
    beat.tick().await;
    loop {
        let msg = tokio::select! {
            _ = stop.changed() => { let _ = w.close().await; return End::Stopped; }
            _ = beat.tick() => {
                let _ = w.send(Message::text(json!({ "op": 1, "d": resume.seq }).to_string())).await;
                continue;
            }
            m = r.next() => m,
        };
        let text = match msg {
            Some(Ok(Message::Text(t))) => t.to_string(),
            Some(Ok(Message::Close(f))) => {
                let code = f.as_ref().map(|f| u16::from(f.code)).unwrap_or(0);
                return match code {
                    4004 => End::Fatal("QQ 机器人凭据无效，请重新扫码绑定".into()),
                    4914 => End::Fatal("QQ 机器人已下架，只能连沙箱环境".into()),
                    4915 => End::Fatal("QQ 机器人已被封禁".into()),
                    4006 | 4007 | 4009 => { *resume = Resume::default(); End::Retry(format!("QQ 会话失效（{code}）")) }
                    _ => End::Retry(format!("QQ 网关断开（{code}）")),
                };
            }
            Some(Ok(_)) => continue,
            Some(Err(e)) => return End::Retry(format!("QQ 网关出错：{e}")),
            None => return End::Retry("QQ 网关断开".into()),
        };
        let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
        if let Some(n) = v["s"].as_u64() { resume.seq = Some(n); }
        match v["op"].as_u64() {
            Some(10) => {
                let ms = v["d"]["heartbeat_interval"].as_u64().unwrap_or(41_250).max(5_000);
                beat = tokio::time::interval(Duration::from_millis(ms));
                beat.tick().await;
                let hello = if resume.session.is_empty() {
                    json!({ "op": 2, "d": { "token": format!("QQBot {token}"), "intents": INTENTS, "shard": [0, 1], "properties": {} } })
                } else {
                    json!({ "op": 6, "d": { "token": format!("QQBot {token}"), "session_id": resume.session, "seq": resume.seq } })
                };
                if w.send(Message::text(hello.to_string())).await.is_err() { return End::Retry("QQ 网关断开".into()); }
            }
            Some(7) => return End::Retry("QQ 要求重连".into()),
            Some(9) => { *resume = Resume::default(); return End::Retry("QQ 会话失效".into()); }
            Some(0) => {
                let t = v["t"].as_str().unwrap_or_default();
                if t == "READY" {
                    resume.session = s(&v["d"]["session_id"]);
                    *ready = true;
                    on_status(format!("在线 · {}", v["d"]["user"]["username"].as_str().unwrap_or("QQ 机器人")));
                } else if t == "RESUMED" {
                    *ready = true;
                    on_status("在线".into());
                } else if let Some(m) = parse(t, &v["d"]) && tx.send(m).await.is_err() {
                    return End::Stopped;
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decrypts_what_the_platform_encrypts() {
        let key = [7u8; 32];
        let enc = encrypt("s3cretAppSecret", &b64().encode(key)).unwrap();
        assert_eq!(b64().decode(&enc).unwrap().len(), 12 + 15 + 16);
        assert_eq!(decrypt(&enc, &b64().encode(key)).unwrap(), "s3cretAppSecret");
        assert!(decrypt(&enc, &b64().encode([8u8; 32])).is_err());
        assert!(decrypt("AAAA", &b64().encode(key)).is_err());
    }

    #[test]
    fn bind_url_and_qr() {
        let b = Bind { task_id: "e66b0d41-4c3a 42f7".into(), key: String::new() };
        assert_eq!(b.url(), "https://q.qq.com/qqbot/openclaw/connect.html?task_id=e66b0d41-4c3a%2042f7&source=&_wv=2");
        let svg = qr_svg(&b.url()).unwrap();
        assert!(svg.starts_with("<?xml") && svg.contains("<svg") && svg.contains("<path"), "{}", &svg[..80]);
    }

    #[test]
    fn parses_group_messages() {
        let d = json!({ "id": "m1", "group_openid": "G1", "content": " <@BOT> @claude 看下 <@U2>",
            "author": { "member_openid": "U1", "username": "小王" },
            "mentions": [{ "member_openid": "BOT", "is_you": true }, { "member_openid": "U2", "username": "阿杰" }],
            "attachments": [{ "url": "//multimedia.nt.qq.com/a.png", "filename": "a.png", "size": 12 }] });
        let m = parse("GROUP_MESSAGE_CREATE", &d).unwrap();
        assert_eq!((m.kind.clone(), m.group.as_str(), m.author_id.as_str(), m.author.as_str(), m.at_me, m.bot), (Kind::Group, "G1", "U1", "小王", true, false));
        assert_eq!(m.content, "@claude 看下 @阿杰");
        assert_eq!(m.attachments, vec![Attachment { url: "https://multimedia.nt.qq.com/a.png".into(), name: "a.png".into(), size: 12 }]);
        let plain = parse("GROUP_MESSAGE_CREATE", &json!({ "id": "m2", "group_openid": "G1", "content": "hi", "author": { "member_openid": "ABCDEF12", "bot": true } })).unwrap();
        assert!(!plain.at_me && plain.bot && plain.author == "QQ 用户EF12");
        assert!(parse("GROUP_AT_MESSAGE_CREATE", &json!({ "id": "m3", "group_openid": "G1", "content": "x", "author": {} })).unwrap().at_me);
        let press = parse("INTERACTION_CREATE", &json!({ "id": "i1", "group_openid": "G1", "group_member_openid": "U1", "data": { "resolved": { "button_data": "disco:a:0" } } })).unwrap();
        assert_eq!((press.kind, press.id.as_str(), press.author_id.as_str(), press.press.as_deref()), (Kind::Press, "i1", "U1", Some("disco:a:0")));
        let dm = parse("C2C_MESSAGE_CREATE", &json!({ "id": "m4", "content": "hi", "author": { "user_openid": "O1" } })).unwrap();
        assert_eq!((dm.kind, dm.group.as_str()), (Kind::Direct, "O1"));
        assert_eq!(parse("GROUP_ADD_ROBOT", &json!({ "group_openid": "G2", "op_member_openid": "U9" })).map(|m| (m.kind, m.group, m.author_id)), Some((Kind::Added, "G2".into(), "U9".into())));
        assert_eq!(parse("MESSAGE_REACTION_ADD", &json!({})), None);
    }

    #[test]
    fn chunks_and_keyboard() {
        assert_eq!(chunk("hi"), vec!["hi".to_owned()]);
        let long = format!("{}\n{}", "中".repeat(1000), "文".repeat(2000));
        let parts = chunk(&long);
        assert!(parts.len() == 3 && parts.iter().all(|p| p.chars().count() <= CHUNK), "{:?}", parts.iter().map(|p| p.chars().count()).collect::<Vec<_>>());
        assert_eq!(parts.concat(), long);
        let k = keyboard(&[("disco:a:0".into(), "允许一次".into(), true), ("disco:a:x".into(), "拒绝".into(), false)]);
        let b = &k["content"]["rows"][0]["buttons"];
        assert_eq!((b[0]["action"]["data"].as_str(), b[0]["render_data"]["style"].as_i64(), b[1]["render_data"]["label"].as_str()), (Some("disco:a:0"), Some(1), Some("拒绝")));
    }
}
