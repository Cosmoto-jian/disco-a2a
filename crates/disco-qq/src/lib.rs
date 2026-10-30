//! QQ adapter for the official QQ Bot API (q.qq.com open platform): one-scan bot
//! binding, REST over reqwest and a small WebSocket gateway client.
//!
//! Binding is the open platform's own "快捷绑定" (the flow behind the official
//! `@tencent-connect/qqbot-connector` SDK): Disco asks q.qq.com for a bind task with a
//! random AES key, shows the task's page as a QR code, the user scans it with mobile
//! QQ and picks (or creates) a bot, and the poll returns the bot's AppID and its
//! AppSecret encrypted with that key. Nothing to copy or paste.
#![forbid(unsafe_code)]

use std::sync::atomic::{AtomicBool, Ordering};
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
/// GROUP_AND_C2C_EVENT (group @ messages, private chats, bot added/removed) | INTERACTION
/// (button presses). The official SDK also asks for guild intents; Disco has no use for them.
pub const INTENTS: u64 = (1 << 25) | (1 << 26);
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

/// Where the API lives (overridden only by tests), and the system proxy to fall back to.
#[derive(Clone, Debug)]
pub struct Endpoints {
    pub api: String,
    pub token: String,
    /// `http://host:port` or `socks5://host:port`. QQ is tried directly first (it is a
    /// mainland service, a VPN usually only slows it down); the proxy is the fallback
    /// when the direct route cannot connect.
    pub proxy: Option<String>,
}

impl Default for Endpoints {
    fn default() -> Self { Self { api: API.into(), token: TOKEN_URL.into(), proxy: None } }
}

#[derive(Clone)]
pub struct Rest {
    /// Direct (never through a proxy).
    http: reqwest::Client,
    /// Through the system proxy, when there is one.
    via: Option<reqwest::Client>,
    /// The direct route failed and the proxy worked: use the proxy first.
    proxied: Arc<AtomicBool>,
    creds: Creds,
    at: Endpoints,
    token: Arc<Mutex<Option<(String, Instant)>>>,
}

/// One button of an inline keyboard: (button data, label, primary).
pub type Button = (String, String, bool);

/// Where a message goes: a QQ group (its openid) or one user's private chat with the bot (C2C, the user's openid).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum To { Group(String), User(String) }

impl To {
    fn path(&self, what: &str) -> String {
        match self { To::Group(g) => format!("/v2/groups/{g}/{what}"), To::User(u) => format!("/v2/users/{u}/{what}") }
    }
    /// The passive-reply window QQ gives: 5 minutes in groups, 60 in private chats (kept a little shorter).
    pub fn reply_window(&self) -> Duration {
        match self { To::Group(_) => Duration::from_secs(290), To::User(_) => Duration::from_secs(3500) }
    }
}

/// A fresh message sequence number (1..=65535). QQ drops a message whose (msg_id,
/// msg_seq) it has seen before ("消息被去重"), so every message gets its own, like the
/// official SDK's `getNextMsgSeq`.
fn next_seq() -> u32 {
    let r = ring::rand::generate::<[u8; 2]>(&ring::rand::SystemRandom::new()).map(|r| r.expose()).unwrap_or_default();
    u32::from(u16::from_be_bytes(r)).max(1)
}

const UA: &str = concat!("disco-a2a/", env!("CARGO_PKG_VERSION"), " (qqbot)");

impl Rest {
    pub fn new(creds: Creds, at: Endpoints) -> Self {
        let base = || reqwest::Client::builder().connect_timeout(Duration::from_secs(10)).user_agent(UA);
        let http = base().no_proxy().build().unwrap_or_default();
        let via = at.proxy.as_deref().and_then(|p| reqwest::Proxy::all(p).ok()).and_then(|p| base().proxy(p).build().ok());
        Self { http, via, proxied: Arc::default(), creds, at, token: Arc::default() }
    }

    pub fn app_id(&self) -> &str { &self.creds.app_id }

    /// Whether requests currently go through the system proxy.
    pub fn via_proxy(&self) -> bool { self.via.is_some() && self.proxied.load(Ordering::Relaxed) }

    /// Send on the preferred route; when it cannot connect, try the other one once and
    /// remember the one that worked.
    async fn route(&self, build: impl Fn(&reqwest::Client) -> reqwest::RequestBuilder) -> Result<reqwest::Response, String> {
        let first_proxy = self.via_proxy();
        let first = if first_proxy { self.via.as_ref().unwrap_or(&self.http) } else { &self.http };
        match build(first).send().await {
            Ok(r) => Ok(r),
            Err(e) if e.is_connect() || e.is_timeout() => {
                let other = if first_proxy { Some(&self.http) } else { self.via.as_ref() };
                let Some(other) = other else { return Err(format!("连不上 QQ（{}）：{e}", if first_proxy { "经代理" } else { "直连" })) };
                let r = build(other).send().await.map_err(|e2| format!("连不上 QQ：直连和经系统代理都失败（{e} / {e2}）"))?;
                self.proxied.store(!first_proxy, Ordering::Relaxed);
                Ok(r)
            }
            Err(e) => Err(format!("连不上 QQ：{e}")),
        }
    }

    /// An access token (cached; fetched again a minute before it runs out).
    pub async fn token(&self) -> Result<String, String> {
        if let Some((t, until)) = self.token.lock().unwrap_or_else(|p| p.into_inner()).clone() && Instant::now() < until { return Ok(t); }
        let body = json!({ "appId": self.creds.app_id, "clientSecret": self.creds.secret });
        let r = self.route(|c| c.post(&self.at.token).json(&body).timeout(Duration::from_secs(15))).await?;
        let status = r.status();
        let text = r.text().await.unwrap_or_default();
        let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let Some(t) = v["access_token"].as_str().filter(|t| !t.is_empty()) else {
            // e.g. {"code":10004,"message":"机器人不存在"} / 100016 invalid secret.
            let why = v["message"].as_str().map(str::to_owned).unwrap_or_else(|| format!("HTTP {status} {}", text.chars().take(120).collect::<String>()));
            return Err(format!("取 access_token 失败（AppID {}）：{why}{}", self.creds.app_id, v["code"].as_i64().map(|c| format!("（{c}）")).unwrap_or_default()));
        };
        let secs = match &v["expires_in"] { Value::String(s) => s.parse().unwrap_or(7200), n => n.as_u64().unwrap_or(7200) };
        *self.token.lock().unwrap_or_else(|p| p.into_inner()) = Some((t.to_owned(), Instant::now() + Duration::from_secs(secs.saturating_sub(60).max(30))));
        Ok(t.to_owned())
    }

    /// Drop the cached token (QQ rejected it).
    pub fn forget_token(&self) { *self.token.lock().unwrap_or_else(|p| p.into_inner()) = None; }

    async fn call(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Result<Value, String> {
        for attempt in 0..2 {
            let token = self.token().await?;
            let url = format!("{}{path}", self.at.api);
            let r = self.route(|c| {
                let mut req = c.request(method.clone(), &url).header("Authorization", format!("QQBot {token}"))
                    .header("X-Union-Appid", &self.creds.app_id).timeout(Duration::from_secs(60));
                if let Some(b) = &body { req = req.json(b); }
                req
            }).await?;
            let status = r.status();
            let text = r.text().await.unwrap_or_default();
            let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            // An expired / revoked token: fetch a new one and try once more.
            if status.as_u16() == 401 && attempt == 0 { self.forget_token(); continue; }
            if !status.is_success() || v.get("code").and_then(Value::as_i64).is_some_and(|c| c != 0) {
                let msg = v["message"].as_str().map(str::to_owned).unwrap_or_else(|| text.chars().take(200).collect());
                let code = v["code"].as_i64().or_else(|| v["err_code"].as_i64()).map_or(status.as_u16().to_string(), |c| c.to_string());
                return Err(format!("QQ 返回 {code}：{msg}"));
            }
            return Ok(v);
        }
        Err("QQ 拒绝了 access_token（401）".into())
    }

    /// The bot's name (`/users/@me`; not every bot type may call it, so callers treat a failure as "no name").
    pub async fn me(&self) -> Result<String, String> {
        Ok(self.call(reqwest::Method::GET, "/users/@me", None).await?["username"].as_str().unwrap_or("QQ 机器人").to_owned())
    }

    pub async fn gateway_url(&self) -> Result<String, String> {
        self.call(reqwest::Method::GET, "/gateway", None).await?["url"].as_str().filter(|u| u.starts_with("ws")).map(str::to_owned).ok_or("拿不到 QQ 网关地址".into())
    }

    /// Post `text` (markdown) to a group, in pieces when long. `reply` = the message being
    /// answered (a passive reply); none = an active message. Buttons go with the last
    /// piece. Falls back to plain text when the bot may not send markdown.
    pub async fn send_group(&self, group: &str, text: &str, reply: Option<&str>, buttons: &[Button]) -> Result<(), String> {
        self.send(&To::Group(group.to_owned()), text, reply, buttons).await
    }

    /// Like [`Rest::send_group`], to a group or to one user's private (C2C) chat with the bot.
    pub async fn send(&self, to: &To, text: &str, reply: Option<&str>, buttons: &[Button]) -> Result<(), String> {
        let parts = chunk(text);
        let n = parts.len();
        let path = to.path("messages");
        for (i, part) in parts.into_iter().enumerate() {
            let mut body = json!({ "msg_type": 2, "markdown": { "content": part }, "msg_seq": next_seq() });
            if let Some(id) = reply { body["msg_id"] = json!(id); }
            if i + 1 == n && !buttons.is_empty() { body["keyboard"] = keyboard(buttons); }
            if let Err(e) = self.call(reqwest::Method::POST, &path, Some(body)).await {
                // No markdown permission: plain text (buttons need markdown, so they go).
                let mut plain = json!({ "msg_type": 0, "content": part, "msg_seq": next_seq() });
                if let Some(id) = reply { plain["msg_id"] = json!(id); }
                self.call(reqwest::Method::POST, &path, Some(plain)).await.map_err(|e2| format!("{e2}（markdown：{e}）"))?;
            }
        }
        Ok(())
    }

    /// Upload a file (≤ the caller's limit) to a group and post it.
    pub async fn send_group_file(&self, group: &str, name: &str, bytes: &[u8], reply: Option<&str>) -> Result<(), String> {
        self.send_file(&To::Group(group.to_owned()), name, bytes, reply).await
    }

    /// Like [`Rest::send_group_file`], to a group or a private chat. `file_type`: 1 image, 2 video, 3 voice, 4 file.
    pub async fn send_file(&self, to: &To, name: &str, bytes: &[u8], reply: Option<&str>) -> Result<(), String> {
        let lower = name.to_ascii_lowercase();
        let kind = if [".png", ".jpg", ".jpeg", ".gif", ".webp"].iter().any(|e| lower.ends_with(e)) { 1 } else if [".mp4", ".mov"].iter().any(|e| lower.ends_with(e)) { 2 } else { 4 };
        let up = self.call(reqwest::Method::POST, &to.path("files"),
            Some(json!({ "file_type": kind, "file_data": b64().encode(bytes), "file_name": name, "srv_send_msg": false }))).await?;
        let info = up["file_info"].as_str().ok_or("上传文件失败：没有 file_info")?;
        let mut body = json!({ "msg_type": 7, "media": { "file_info": info }, "msg_seq": next_seq() });
        if let Some(id) = reply { body["msg_id"] = json!(id); }
        self.call(reqwest::Method::POST, &to.path("messages"), Some(body)).await?;
        Ok(())
    }

    /// Acknowledge a button press (QQ shows a failure if it is not answered).
    pub async fn ack(&self, interaction: &str) -> Result<(), String> {
        self.call(reqwest::Method::PUT, &format!("/interactions/{interaction}"), Some(json!({ "code": 0 }))).await.map(|_| ())
    }

    /// Download an attachment (at most `max` bytes).
    pub async fn download(&self, url: &str, max: u64) -> Result<Vec<u8>, String> {
        let mut r = self.route(|c| c.get(url).timeout(Duration::from_secs(120))).await?;
        if !r.status().is_success() { return Err(format!("HTTP {}", r.status())); }
        let mut out = Vec::new();
        while let Some(c) = r.chunk().await.map_err(|e| e.to_string())? {
            out.extend_from_slice(&c);
            if out.len() as u64 > max { return Err("文件太大".into()); }
        }
        Ok(out)
    }

    /// Open the gateway WebSocket: directly, or through the system proxy (first when the
    /// REST side already found the direct route dead, else as the fallback).
    async fn open_ws(&self, url: &str) -> Result<(Ws, bool), String> {
        let direct = || async { tokio_tungstenite::connect_async(url).await.map(|(w, _)| w).map_err(|e| e.to_string()) };
        let proxied = |p: String| async move {
            let (host, port) = ws_host(url).ok_or("网关地址无效")?;
            let tcp = tunnel(&host, port, &p).await?;
            tokio_tungstenite::client_async_tls_with_config(url, tcp, None, None).await.map(|(w, _)| w).map_err(|e| e.to_string())
        };
        let proxy = self.at.proxy.clone();
        if self.via_proxy() && let Some(p) = proxy.clone() && let Ok(w) = proxied(p).await { return Ok((w, true)); }
        match direct().await {
            Ok(w) => Ok((w, false)),
            Err(e) => match proxy {
                Some(p) => proxied(p).await.map(|w| (w, true)).map_err(|e2| format!("连不上 QQ 网关：直连 {e}；经系统代理 {e2}")),
                None => Err(format!("连不上 QQ 网关：{e}")),
            },
        }
    }
}

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// `wss://host[:port]/path` → (host, port).
fn ws_host(url: &str) -> Option<(String, u16)> {
    let (scheme, rest) = url.split_once("://")?;
    let auth = rest.split('/').next()?;
    let port = if scheme == "wss" { 443 } else { 80 };
    Some(match auth.rsplit_once(':') { Some((h, p)) => (h.to_owned(), p.parse().ok()?), None => (auth.to_owned(), port) })
}

/// A TCP tunnel to `host:port` through an HTTP (CONNECT) or SOCKS5 proxy.
async fn tunnel(host: &str, port: u16, proxy: &str) -> Result<tokio::net::TcpStream, String> {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let (scheme, rest) = proxy.split_once("://").unwrap_or(("http", proxy));
    let rest = rest.trim_end_matches('/');
    let (auth, addr) = match rest.rsplit_once('@') { Some((a, h)) => (Some(a), h), None => (None, rest) };
    let mut s = tokio::time::timeout(Duration::from_secs(10), tokio::net::TcpStream::connect(addr)).await
        .map_err(|_| format!("连接代理 {addr} 超时"))?.map_err(|e| format!("连接代理 {addr} 失败：{e}"))?;
    if scheme.starts_with("socks5") {
        s.write_all(&[5, 1, 0]).await.map_err(|e| e.to_string())?;
        let mut r = [0u8; 2];
        s.read_exact(&mut r).await.map_err(|e| e.to_string())?;
        if r != [5, 0] { return Err("SOCKS5 代理拒绝了无认证连接".into()); }
        let mut req = vec![5, 1, 0, 3, host.len() as u8];
        req.extend_from_slice(host.as_bytes());
        req.extend_from_slice(&port.to_be_bytes());
        s.write_all(&req).await.map_err(|e| e.to_string())?;
        let mut head = [0u8; 4];
        s.read_exact(&mut head).await.map_err(|e| e.to_string())?;
        if head[1] != 0 { return Err(format!("SOCKS5 CONNECT 失败（{}）", head[1])); }
        let skip = match head[3] { 1 => 4, 4 => 16, _ => { let mut l = [0u8; 1]; s.read_exact(&mut l).await.map_err(|e| e.to_string())?; l[0] as usize } };
        let mut tail = vec![0u8; skip + 2];
        s.read_exact(&mut tail).await.map_err(|e| e.to_string())?;
    } else {
        let auth = auth.map(|a| format!("Proxy-Authorization: Basic {}\r\n", b64().encode(a))).unwrap_or_default();
        s.write_all(format!("CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\n{auth}\r\n").as_bytes()).await.map_err(|e| e.to_string())?;
        let mut buf = Vec::with_capacity(256);
        let mut b = [0u8; 1];
        while !buf.ends_with(b"\r\n\r\n") && buf.len() < 8192 {
            s.read_exact(&mut b).await.map_err(|e| format!("代理无响应：{e}"))?;
            buf.push(b[0]);
        }
        let status = String::from_utf8_lossy(&buf);
        if !status.split_whitespace().nth(1).is_some_and(|c| c == "200") {
            return Err(format!("代理 CONNECT 失败：{}", status.lines().next().unwrap_or("")));
        }
    }
    Ok(s)
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
            // QQ also writes the bot's own @ as `<@!appid>` / `<@appid>` without listing it in
            // `mentions` (the official SDK strips any `<@!?…>` left over): drop those.
            let content = strip_markers(&content);
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

/// Remove leftover `<@…>` / `<@!…>` mention markers (and the space after each).
fn strip_markers(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find("<@") {
        let Some(j) = rest[i..].find('>') else { break };
        let id = rest[i + 2..i + j].trim_start_matches('!');
        if id.is_empty() || id.contains(char::is_whitespace) { out.push_str(&rest[..i + 2]); rest = &rest[i + 2..]; continue; }
        out.push_str(&rest[..i]);
        rest = rest[i + j + 1..].trim_start_matches(' ');
    }
    out.push_str(rest);
    out
}

// ---------------------------------------------------------------- gateway client

/// What the gateway client is doing, for the status line and diagnostics.
#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    /// A step on the way in: 取 token / 取网关地址 / 连网关 / 鉴权.
    Step(&'static str),
    /// READY (with the bot's name) or RESUMED (None); `bool` = through the system proxy.
    Ready(Option<String>, bool),
    /// A dispatch arrived (its type), whether Disco uses it or not.
    Event(String),
    /// Lost or refused; trying again in so many seconds.
    Retry(String, u64),
    /// QQ will not let this bot connect (offline / banned): no retry.
    Fatal(String),
    Stopped,
}

enum End { Stopped, Fatal(String), Retry(String, Option<u64>) }

#[derive(Default)]
struct Resume { session: String, seq: Option<u64>, rejected: u32 }

impl Resume {
    fn reset(&mut self) { self.session.clear(); self.seq = None; }
}

/// The official SDK's reconnect delays (seconds).
const DELAYS: [u64; 6] = [1, 2, 5, 10, 30, 60];

/// Connect, identify (or resume), keep the heartbeat, hand every event to `tx`;
/// reconnect with backoff until `stop` fires. `on_status` follows every step.
pub async fn run_gateway(rest: Rest, tx: mpsc::Sender<Incoming>, mut stop: watch::Receiver<bool>, on_status: impl Fn(Status) + Send + 'static) {
    let mut attempt = 0usize;
    let mut resume = Resume::default();
    loop {
        let mut ready = false;
        let end = session(&rest, Some(&tx), &mut stop, &mut resume, &mut ready, &on_status, INTENTS).await;
        let (why, wait) = match end {
            End::Stopped => { on_status(Status::Stopped); return; }
            End::Fatal(e) => { on_status(Status::Fatal(e)); return; }
            End::Retry(e, custom) => {
                if ready { attempt = 0; }
                let wait = custom.unwrap_or(DELAYS[attempt.min(DELAYS.len() - 1)]);
                attempt += 1;
                (e, wait)
            }
        };
        on_status(Status::Retry(why, wait));
        tokio::select! {
            _ = stop.changed() => { on_status(Status::Stopped); return; }
            _ = tokio::time::sleep(Duration::from_secs(wait)) => {}
        }
    }
}

/// One-off check for diagnostics: token → gateway → HELLO → IDENTIFY → READY, then
/// close. Returns the bot's name and how long it took.
pub async fn gateway_check(rest: &Rest, limit: Duration) -> Result<(String, Duration, bool), String> {
    let started = Instant::now();
    let (_tx, mut stop) = watch::channel(false);
    let mut resume = Resume::default();
    let mut ready = false;
    let name = Arc::new(Mutex::new(None::<(String, bool)>));
    let n2 = name.clone();
    let on = move |s: Status| if let Status::Ready(n, p) = s { *n2.lock().unwrap_or_else(|e| e.into_inner()) = Some((n.unwrap_or_default(), p)); };
    // Only run while the real connection is down: two sessions of one bot share its events.
    let end = tokio::time::timeout(limit, session(rest, None, &mut stop, &mut resume, &mut ready, &on, INTENTS)).await;
    let got = name.lock().unwrap_or_else(|e| e.into_inner()).clone();
    match (got, end) {
        (Some((n, p)), _) => Ok((n, started.elapsed(), p)),
        (None, Ok(End::Fatal(e) | End::Retry(e, _))) => Err(e),
        (None, Ok(End::Stopped)) => Err("已停止".into()),
        (None, Err(_)) => Err(format!("{} 秒内没等到 READY", limit.as_secs())),
    }
}

/// `tx` None = a check: stop as soon as READY arrives.
async fn session(rest: &Rest, tx: Option<&mpsc::Sender<Incoming>>, stop: &mut watch::Receiver<bool>, resume: &mut Resume, ready: &mut bool,
    on_status: &(impl Fn(Status) + Send), intents: u64) -> End {
    on_status(Status::Step("取 token"));
    let token = match rest.token().await { Ok(t) => t, Err(e) => return End::Retry(e, None) };
    on_status(Status::Step("取网关地址"));
    let url = match rest.gateway_url().await { Ok(u) => u, Err(e) => return End::Retry(e, None) };
    on_status(Status::Step("连网关"));
    let (ws, via_proxy) = tokio::select! {
        _ = stop.changed() => return End::Stopped,
        r = tokio::time::timeout(Duration::from_secs(30), rest.open_ws(&url)) => match r {
            Ok(Ok(w)) => w,
            Ok(Err(e)) => return End::Retry(e, None),
            Err(_) => return End::Retry("连接 QQ 网关超时".into(), None),
        },
    };
    let (mut w, mut r) = ws.split();
    // No heartbeat until HELLO says how often.
    let mut beat = tokio::time::interval_at(tokio::time::Instant::now() + Duration::from_secs(3600), Duration::from_secs(3600));
    let mut acked = true;
    let mut identified = false;
    loop {
        let msg = tokio::select! {
            _ = stop.changed() => { let _ = w.close().await; return End::Stopped; }
            _ = beat.tick() => {
                if !acked { return End::Retry("QQ 网关心跳无响应".into(), None); }
                acked = false;
                if w.send(Message::text(json!({ "op": 1, "d": resume.seq }).to_string())).await.is_err() { return End::Retry("发送心跳失败".into(), None); }
                continue;
            }
            m = r.next() => m,
        };
        let text = match msg {
            Some(Ok(Message::Text(t))) => t.to_string(),
            Some(Ok(Message::Binary(b))) => String::from_utf8_lossy(&b).into_owned(),
            Some(Ok(Message::Close(f))) => {
                let code = f.as_ref().map(|f| u16::from(f.code)).unwrap_or(0);
                let reason = f.as_ref().map(|f| f.reason.to_string()).unwrap_or_default();
                return closed(rest, resume, code, &reason);
            }
            Some(Ok(_)) => continue,
            Some(Err(e)) => return End::Retry(format!("QQ 网关出错：{e}"), None),
            None => return End::Retry("QQ 网关断开".into(), None),
        };
        let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
        if let Some(n) = v["s"].as_u64() { resume.seq = Some(n); }
        match v["op"].as_u64() {
            Some(10) => {
                let ms = v["d"]["heartbeat_interval"].as_u64().unwrap_or(41_250).max(5_000);
                beat = tokio::time::interval_at(tokio::time::Instant::now() + Duration::from_millis(ms), Duration::from_millis(ms));
                on_status(Status::Step("鉴权"));
                let hello = if !resume.session.is_empty() && resume.seq.is_some() && tx.is_some() {
                    json!({ "op": 6, "d": { "token": format!("QQBot {token}"), "session_id": resume.session, "seq": resume.seq } })
                } else {
                    identified = true;
                    json!({ "op": 2, "d": { "token": format!("QQBot {token}"), "intents": intents, "shard": [0, 1] } })
                };
                if w.send(Message::text(hello.to_string())).await.is_err() { return End::Retry("QQ 网关断开".into(), None); }
            }
            Some(11) => acked = true,
            Some(7) => return End::Retry("QQ 要求重连".into(), Some(1)),
            Some(9) => {
                // d = true: the session may be resumed; false: identify again with a fresh token.
                if v["d"].as_bool() != Some(true) { resume.reset(); rest.forget_token(); }
                if identified { resume.rejected += 1; }
                return End::Retry(rejected(resume.rejected, "QQ 拒绝了会话（op 9）"), Some(if resume.rejected >= 3 { 60 } else { 3 }));
            }
            Some(0) => {
                let t = v["t"].as_str().unwrap_or_default();
                if t == "READY" {
                    resume.session = s(&v["d"]["session_id"]);
                    resume.rejected = 0;
                    *ready = true;
                    on_status(Status::Ready(Some(v["d"]["user"]["username"].as_str().unwrap_or("QQ 机器人").to_owned()), via_proxy));
                    if tx.is_none() { let _ = w.close().await; return End::Stopped; }
                } else if t == "RESUMED" {
                    resume.rejected = 0;
                    *ready = true;
                    on_status(Status::Ready(None, via_proxy));
                } else {
                    on_status(Status::Event(t.to_owned()));
                    if let (Some(tx), Some(m)) = (tx, parse(t, &v["d"])) && tx.send(m).await.is_err() { return End::Stopped; }
                }
            }
            _ => {}
        }
    }
}

/// The retry note; after a few refusals in a row, say what usually causes them.
fn rejected(n: u32, what: &str) -> String {
    if n >= 3 { format!("{what}，已连续 {n} 次：AppSecret 可能已在 q.qq.com 重置，请到设置 → QQ 解绑后重新扫码") } else { what.to_owned() }
}

/// What a close code means (as the official SDK reads them).
fn closed(rest: &Rest, resume: &mut Resume, code: u16, reason: &str) -> End {
    let why = |what: &str| if reason.is_empty() { format!("{what}（{code}）") } else { format!("{what}（{code} {reason}）") };
    match code {
        4004 => { rest.forget_token(); resume.rejected += 1; End::Retry(rejected(resume.rejected, &why("QQ 鉴权失败")), Some(if resume.rejected >= 3 { 60 } else { 2 })) }
        4006 | 4007 | 4009 => { resume.reset(); rest.forget_token(); End::Retry(why("QQ 会话失效"), None) }
        4008 => End::Retry(why("QQ 限流"), Some(60)),
        4900..=4913 => { resume.reset(); rest.forget_token(); End::Retry(why("QQ 网关内部错误"), None) }
        4914 => End::Fatal(why("机器人已下架或只能连沙箱环境：到 q.qq.com 机器人管理里上线机器人")),
        4915 => End::Fatal(why("机器人已被封禁")),
        _ => End::Retry(why("QQ 网关断开"), None),
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
        let c2c_press = parse("INTERACTION_CREATE", &json!({ "id": "i2", "chat_type": 2, "user_openid": "O1", "data": { "resolved": { "button_data": "disco:b:1" } } })).unwrap();
        assert_eq!((c2c_press.group.as_str(), c2c_press.author_id.as_str()), ("", "O1"));
        assert_eq!(To::User("O1".into()).path("files"), "/v2/users/O1/files");
        assert_eq!(To::Group("G1".into()).path("messages"), "/v2/groups/G1/messages");
        assert!(To::User(String::new()).reply_window() > To::Group(String::new()).reply_window());
        let dm = parse("C2C_MESSAGE_CREATE", &json!({ "id": "m4", "content": "hi", "author": { "user_openid": "O1" } })).unwrap();
        assert_eq!((dm.kind, dm.group.as_str()), (Kind::Direct, "O1"));
        assert_eq!(parse("GROUP_ADD_ROBOT", &json!({ "group_openid": "G2", "op_member_openid": "U9" })).map(|m| (m.kind, m.group, m.author_id)), Some((Kind::Added, "G2".into(), "U9".into())));
        assert_eq!(parse("MESSAGE_REACTION_ADD", &json!({})), None);
        // Real group @ messages may carry the bot's own marker without a `mentions` entry.
        let raw = parse("GROUP_AT_MESSAGE_CREATE", &json!({ "id": "m5", "group_openid": "G1", "content": "<@!1020304> 绑定 1234", "author": { "member_openid": "U1" } })).unwrap();
        assert_eq!(raw.content, "绑定 1234");
        assert_eq!(strip_markers("<@1020304>/help"), "/help");
        assert_eq!(strip_markers("a <@ b> c"), "a <@ b> c");
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

    /// A tiny QQ open platform for gateway tests: tokens tok1, tok2, … (one per fetch),
    /// `/gateway` (only with a valid `QQBot tokN` + X-Union-Appid) pointing at `ws`.
    async fn mock_platform(ws: String) -> (String, Arc<Mutex<Vec<String>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let log = Arc::new(Mutex::new(Vec::<String>::new()));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        let lg = log.clone();
        tokio::spawn(async move {
            let mut n = 0;
            loop {
                let Ok((mut s, _)) = l.accept().await else { return };
                let mut buf = vec![0u8; 8192];
                let k = s.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..k]).to_string();
                let line = req.lines().next().unwrap_or_default().to_owned();
                let header = |h: &str| req.lines().find_map(|l| l.split_once(':').filter(|(k, _)| k.eq_ignore_ascii_case(h)).map(|(_, v)| v.trim().to_owned())).unwrap_or_default();
                let (code, body) = if line.starts_with("POST /app/getAppAccessToken") {
                    n += 1;
                    lg.lock().unwrap().push(format!("token{n} {}", req.split("\r\n\r\n").nth(1).unwrap_or_default()));
                    (200, format!(r#"{{"access_token":"tok{n}","expires_in":"7200"}}"#))
                } else if line.starts_with("GET /gateway") {
                    lg.lock().unwrap().push(format!("gateway {} {}", header("authorization"), header("x-union-appid")));
                    if header("authorization").starts_with("QQBot tok") { (200, format!(r#"{{"url":"{ws}"}}"#)) } else { (401, r#"{"code":11244,"message":"AccessToken无效或过期"}"#.to_owned()) }
                } else { (404, "{}".to_owned()) };
                let _ = s.write_all(format!("HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await;
            }
        });
        (format!("http://127.0.0.1:{port}"), log)
    }

    /// The gateway as QQ runs it: HELLO, IDENTIFY with `QQBot <token>` + intents + shard,
    /// 4004 for a stale token, READY, then a private message. Before v1.11.1 a 4004 ended
    /// the connection for good and the token was never fetched again.
    #[tokio::test]
    async fn gateway_refreshes_the_token_after_4004_and_delivers_events() {
        use tokio_tungstenite::tungstenite::protocol::{CloseFrame, frame::coding::CloseCode};
        let _ = rustls::crypto::ring::default_provider().install_default();
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ws = format!("ws://127.0.0.1:{}/websocket/", l.local_addr().unwrap().port());
        let idents = Arc::new(Mutex::new(Vec::<Value>::new()));
        let id2 = idents.clone();
        tokio::spawn(async move {
            let mut conn = 0;
            loop {
                let Ok((s, _)) = l.accept().await else { return };
                conn += 1;
                let mut ws = tokio_tungstenite::accept_async(s).await.unwrap();
                ws.send(Message::text(r#"{"op":10,"d":{"heartbeat_interval":41250}}"#)).await.unwrap();
                let Some(Ok(Message::Text(t))) = ws.next().await else { continue };
                let v: Value = serde_json::from_str(&t).unwrap();
                id2.lock().unwrap().push(v.clone());
                if conn == 1 {
                    let _ = ws.send(Message::text(r#"{"op":9,"d":false}"#)).await;
                    let _ = ws.close(Some(CloseFrame { code: CloseCode::from(4004), reason: "Authentication fail".into() })).await;
                    continue;
                }
                ws.send(Message::text(r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"S1","user":{"username":"DiscoBot"}}}"#)).await.unwrap();
                ws.send(Message::Binary(br#"{"op":0,"s":2,"t":"C2C_MESSAGE_CREATE","d":{"id":"c1","content":"hi","author":{"id":"x","user_openid":"OWNER","union_openid":"u"}}}"#.to_vec().into())).await.unwrap();
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
        });
        let (api, log) = mock_platform(ws).await;
        let rest = Rest::new(Creds { app_id: "102".into(), secret: "sec".into() }, Endpoints { api: api.clone(), token: format!("{api}/app/getAppAccessToken"), proxy: None });
        let (tx, mut rx) = mpsc::channel(8);
        let (stop_tx, stop_rx) = watch::channel(false);
        let seen = Arc::new(Mutex::new(Vec::<Status>::new()));
        let s2 = seen.clone();
        tokio::spawn(run_gateway(rest, tx, stop_rx, move |s| s2.lock().unwrap().push(s)));
        let m = tokio::time::timeout(Duration::from_secs(15), rx.recv()).await.expect("no event within 15 s").unwrap();
        let _ = stop_tx.send(true);
        assert_eq!((m.kind, m.group.as_str(), m.author_id.as_str(), m.content.as_str()), (Kind::Direct, "OWNER", "OWNER", "hi"));
        let ids = idents.lock().unwrap().clone();
        assert_eq!(ids.len(), 2, "{ids:?}");
        assert_eq!(ids[0]["op"], 2);
        assert_eq!(ids[0]["d"]["token"], "QQBot tok1");
        assert_eq!(ids[0]["d"]["intents"], json!(INTENTS));
        assert_eq!(ids[0]["d"]["shard"], json!([0, 1]));
        assert_eq!(ids[1]["d"]["token"], "QQBot tok2", "the rejected token must be fetched again");
        let log = log.lock().unwrap().clone();
        assert!(log[0].contains(r#""appId":"102""#) && log[0].contains(r#""clientSecret":"sec""#), "{log:?}");
        assert!(log.iter().any(|l| l == "gateway QQBot tok1 102"), "{log:?}");
        let seen = seen.lock().unwrap().clone();
        assert!(seen.contains(&Status::Step("鉴权")) && seen.contains(&Status::Ready(Some("DiscoBot".into()), false)), "{seen:?}");
        assert!(seen.iter().any(|s| matches!(s, Status::Retry(e, _) if e.contains("op 9") || e.contains("4004"))), "{seen:?}");
        assert!(seen.contains(&Status::Event("C2C_MESSAGE_CREATE".into())), "{seen:?}");
        assert!(!seen.iter().any(|s| matches!(s, Status::Fatal(_))), "{seen:?}");
    }

    #[test]
    fn seqs_hosts_and_close_codes() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let a: Vec<u32> = (0..50).map(|_| next_seq()).collect();
        assert!(a.iter().all(|s| (1..=65535).contains(s)));
        assert!(a.windows(2).any(|w| w[0] != w[1]), "msg_seq must vary");
        assert_eq!(ws_host("wss://api.sgroup.qq.com/websocket/"), Some(("api.sgroup.qq.com".into(), 443)));
        assert_eq!(ws_host("ws://127.0.0.1:9/x"), Some(("127.0.0.1".into(), 9)));
        let rest = Rest::new(Creds { app_id: "1".into(), secret: "x".into() }, Endpoints::default());
        let mut r = Resume { session: "S".into(), seq: Some(3), rejected: 0 };
        assert!(matches!(closed(&rest, &mut r, 4004, ""), End::Retry(_, Some(2))));
        assert!(matches!(closed(&rest, &mut r, 4009, ""), End::Retry(_, None)) && r.session.is_empty());
        assert!(matches!(closed(&rest, &mut r, 4008, ""), End::Retry(_, Some(60))));
        assert!(matches!(closed(&rest, &mut r, 4914, ""), End::Fatal(_)));
        assert!(matches!(closed(&rest, &mut r, 4915, ""), End::Fatal(_)));
        r.rejected = 2;
        assert!(matches!(closed(&rest, &mut r, 4004, ""), End::Retry(e, Some(60)) if e.contains("重新扫码")));
    }

    /// Live check against the real QQ gateway (network; `cargo test -p disco-qq -- --ignored`):
    /// a fake token gets HELLO, then INVALID_SESSION / 4004, and never hangs.
    #[tokio::test]
    #[ignore]
    async fn live_gateway_rejects_a_fake_token() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let (api, _log) = mock_platform("wss://api.sgroup.qq.com/websocket/".into()).await;
        let rest = Rest::new(Creds { app_id: "1".into(), secret: "x".into() }, Endpoints { api: api.clone(), token: format!("{api}/app/getAppAccessToken"), proxy: None });
        let (tx, _rx) = mpsc::channel(8);
        let (stop_tx, stop_rx) = watch::channel(false);
        let seen = Arc::new(Mutex::new(Vec::<Status>::new()));
        let s2 = seen.clone();
        let h = tokio::spawn(run_gateway(rest.clone(), tx, stop_rx, move |s| { println!("status: {s:?}"); s2.lock().unwrap().push(s); }));
        tokio::time::sleep(Duration::from_secs(6)).await;
        let _ = stop_tx.send(true);
        let _ = tokio::time::timeout(Duration::from_secs(3), h).await;
        assert!(seen.lock().unwrap().iter().any(|s| matches!(s, Status::Retry(e, _) if e.contains("op 9") || e.contains("4004"))));
        let check = gateway_check(&rest, Duration::from_secs(10)).await;
        println!("check: {check:?}");
        assert!(check.is_err());
    }
}
