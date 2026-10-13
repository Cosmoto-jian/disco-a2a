//! Discord adapter: REST over reqwest and a small hand-rolled Gateway client.
//!
//! Both honour the macOS system proxy (and HTTPS_PROXY / ALL_PROXY), so Disco
//! reaches Discord through a VPN running in "system proxy" mode, not only TUN.
#![forbid(unsafe_code)]

use std::time::{Duration, Instant};

use futures::{SinkExt as _, StreamExt as _};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::Message;

const API: &str = "https://discord.com/api/v10";
const GATEWAY_HOST: &str = "gateway.discord.gg";
/// GUILDS | GUILD_MESSAGES | MESSAGE_CONTENT
const INTENTS: u64 = 1 | (1 << 9) | (1 << 15);
/// Permissions Disco needs: view, send, history, manage channels, manage webhooks, threads.
pub const BOT_PERMISSIONS: u64 = 395_673_979_968 | (1 << 4);

/// Several dependencies enable different rustls backends; pin one process-wide.
pub fn install_crypto() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

// ---------------------------------------------------------------- proxy

/// The proxy to use for HTTPS, as a URL (`http://host:port` or `socks5://host:port`):
/// environment first, then the system setting (macOS `scutil --proxy`, Windows Internet Options).
pub fn proxy_url() -> Option<String> {
    for k in ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"] {
        if let Ok(v) = std::env::var(k) && !v.trim().is_empty() {
            return Some(v.trim().to_owned());
        }
    }
    system_proxy()
}

#[cfg(target_os = "macos")]
fn system_proxy() -> Option<String> {
    let out = std::process::Command::new("/usr/sbin/scutil").arg("--proxy").output().ok()?;
    parse_scutil(&String::from_utf8_lossy(&out.stdout))
}

/// Windows: the "Internet Options" proxy (what Clash and most VPN apps set).
#[cfg(windows)]
fn system_proxy() -> Option<String> {
    use std::os::windows::process::CommandExt;
    let out = std::process::Command::new("reg")
        .args(["query", r"HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings"])
        .creation_flags(0x0800_0000).output().ok()?;
    parse_reg(&String::from_utf8_lossy(&out.stdout))
}

#[cfg(not(any(target_os = "macos", windows)))]
fn system_proxy() -> Option<String> {
    None
}

/// `ProxyEnable REG_DWORD 0x1` + `ProxyServer REG_SZ host:port` (or `http=..;https=..`).
#[cfg_attr(not(windows), allow(dead_code))]
fn parse_reg(text: &str) -> Option<String> {
    let get = |k: &str| text.lines().find_map(|l| {
        let mut w = l.split_whitespace();
        (w.next()? == k).then(|| w.nth(1).map(str::to_owned)).flatten()
    });
    if get("ProxyEnable").as_deref() != Some("0x1") { return None; }
    let server = get("ProxyServer")?;
    let pick = server.split(';').find_map(|p| p.strip_prefix("https=")).or_else(|| server.split(';').find_map(|p| p.strip_prefix("http=")))
        .or_else(|| server.split(';').find_map(|p| p.strip_prefix("socks=").map(|_| p)))
        .unwrap_or(&server);
    if let Some(sock) = pick.strip_prefix("socks=") { return Some(format!("socks5://{sock}")); }
    Some(if pick.contains("://") { pick.to_owned() } else { format!("http://{pick}") })
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn parse_scutil(text: &str) -> Option<String> {
    let get = |k: &str| {
        text.lines().find_map(|l| {
            let (a, b) = l.split_once(" : ")?;
            (a.trim() == k).then(|| b.trim().to_owned())
        })
    };
    let on = |k: &str| get(k).as_deref() == Some("1");
    if on("HTTPSEnable") { return Some(format!("http://{}:{}", get("HTTPSProxy")?, get("HTTPSPort")?)); }
    if on("SOCKSEnable") { return Some(format!("socks5://{}:{}", get("SOCKSProxy")?, get("SOCKSPort")?)); }
    if on("HTTPEnable") { return Some(format!("http://{}:{}", get("HTTPProxy")?, get("HTTPPort")?)); }
    None
}

/// Open a TCP tunnel to `host:443`, through `proxy` when given.
async fn tunnel(host: &str, proxy: Option<&str>) -> Result<TcpStream, String> {
    let Some(p) = proxy else {
        return TcpStream::connect((host, 443)).await.map_err(|e| format!("连接 {host} 失败：{e}"));
    };
    let (scheme, rest) = p.split_once("://").unwrap_or(("http", p));
    let rest = rest.trim_end_matches('/');
    let (auth, addr) = match rest.rsplit_once('@') { Some((a, h)) => (Some(a), h), None => (None, rest) };
    let mut s = TcpStream::connect(addr).await.map_err(|e| format!("连接代理 {addr} 失败：{e}"))?;
    if scheme.starts_with("socks5") {
        // RFC 1928, no auth, domain-name CONNECT.
        s.write_all(&[5, 1, 0]).await.map_err(|e| e.to_string())?;
        let mut r = [0u8; 2];
        s.read_exact(&mut r).await.map_err(|e| e.to_string())?;
        if r != [5, 0] { return Err("SOCKS5 代理拒绝了无认证连接".into()); }
        let mut req = vec![5, 1, 0, 3, host.len() as u8];
        req.extend_from_slice(host.as_bytes());
        req.extend_from_slice(&443u16.to_be_bytes());
        s.write_all(&req).await.map_err(|e| e.to_string())?;
        let mut head = [0u8; 4];
        s.read_exact(&mut head).await.map_err(|e| e.to_string())?;
        if head[1] != 0 { return Err(format!("SOCKS5 CONNECT 失败（{}）", head[1])); }
        let skip = match head[3] { 1 => 4, 4 => 16, _ => { let mut l = [0u8; 1]; s.read_exact(&mut l).await.map_err(|e| e.to_string())?; l[0] as usize } };
        let mut rest = vec![0u8; skip + 2];
        s.read_exact(&mut rest).await.map_err(|e| e.to_string())?;
    } else {
        let auth = auth.map(|a| format!("Proxy-Authorization: Basic {}\r\n", base64(a.as_bytes()))).unwrap_or_default();
        s.write_all(format!("CONNECT {host}:443 HTTP/1.1\r\nHost: {host}:443\r\n{auth}\r\n").as_bytes()).await.map_err(|e| e.to_string())?;
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

fn base64(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for c in input.chunks(3) {
        let n = (u32::from(c[0]) << 16) | (u32::from(*c.get(1).unwrap_or(&0)) << 8) | u32::from(*c.get(2).unwrap_or(&0));
        for i in 0..4 {
            out.push(if i <= c.len() { T[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
        }
    }
    out
}

// ---------------------------------------------------------------- REST

#[derive(Debug, Clone)]
pub struct ChannelInfo {
    pub guild_id: String,
    pub guild_name: String,
    pub channel_id: String,
    pub channel_name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Guild {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
struct RawChannel {
    id: String,
    #[serde(rename = "type")]
    kind: u8,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    parent_id: Option<String>,
}

/// REST side, cheap to clone.
#[derive(Clone)]
pub struct Rest {
    http: reqwest::Client,
    auth: String,
}

impl Rest {
    pub fn new(token: String) -> Self {
        let mut b = reqwest::Client::builder().timeout(Duration::from_secs(20)).user_agent("DiscordBot (https://github.com/Cosmoto-jian/disco-a2a, 0.4)");
        if let Some(p) = proxy_url() && let Ok(px) = reqwest::Proxy::all(&p) {
            b = b.proxy(px);
        }
        Self { http: b.build().unwrap_or_default(), auth: format!("Bot {}", token.trim()) }
    }

    async fn call(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Result<Value, String> {
        for _ in 0..3 {
            let mut req = self.http.request(method.clone(), format!("{API}{path}"));
            if !path.starts_with("/webhooks/") { req = req.header("Authorization", &self.auth); }
            if let Some(b) = &body { req = req.json(b); }
            let res = req.send().await.map_err(|e| format!("无法连接 Discord：{e}"))?;
            let status = res.status();
            let v: Value = res.json().await.unwrap_or(Value::Null);
            if status.as_u16() == 429 {
                let wait = v["retry_after"].as_f64().unwrap_or(1.0).min(10.0);
                tokio::time::sleep(Duration::from_secs_f64(wait)).await;
                continue;
            }
            if status.is_success() { return Ok(v); }
            let msg = v["message"].as_str().unwrap_or("").to_owned();
            return Err(match status.as_u16() {
                401 => "Bot token 无效（401）".into(),
                403 => format!("Bot 缺少权限（403 {msg}）：请在设置里重新授权 Bot"),
                c => format!("Discord 返回 {c} {msg}"),
            });
        }
        Err("Discord 限流，请稍后再试".into())
    }

    async fn get(&self, path: &str) -> Result<Value, String> {
        self.call(reqwest::Method::GET, path, None).await
    }

    async fn post_json(&self, path: &str, body: Value) -> Result<Value, String> {
        self.call(reqwest::Method::POST, path, Some(body)).await
    }

    /// `(bot user id, bot name)`.
    pub async fn me(&self) -> Result<(String, String), String> {
        let v = self.get("/users/@me").await?;
        Ok((v["id"].as_str().unwrap_or_default().into(), v["username"].as_str().unwrap_or_default().into()))
    }

    /// Discord user ids that own the bot application (the owner, or every team member).
    pub async fn owner_ids(&self) -> Result<Vec<String>, String> {
        let v = self.get("/oauth2/applications/@me").await?;
        let mut ids: Vec<String> = v["owner"]["id"].as_str().map(str::to_owned).into_iter().collect();
        if let Some(members) = v["team"]["members"].as_array() {
            ids.extend(members.iter().filter_map(|m| m["user"]["id"].as_str().map(str::to_owned)));
        }
        Ok(ids)
    }

    pub async fn guilds(&self) -> Result<Vec<Guild>, String> {
        serde_json::from_value(self.get("/users/@me/guilds").await?).map_err(|e| e.to_string())
    }

    async fn channels(&self, guild: &str) -> Result<Vec<RawChannel>, String> {
        serde_json::from_value(self.get(&format!("/guilds/{guild}/channels")).await?).map_err(|e| e.to_string())
    }

    /// Every text channel in every guild the bot is in.
    pub async fn text_channels(&self) -> Result<Vec<ChannelInfo>, String> {
        let mut out = Vec::new();
        for g in self.guilds().await? {
            out.extend(self.channels(&g.id).await?.into_iter().filter(|c| c.kind == 0).map(|c| ChannelInfo {
                guild_id: g.id.clone(),
                guild_name: g.name.clone(),
                channel_id: c.id,
                channel_name: c.name.unwrap_or_default(),
            }));
        }
        Ok(out)
    }

    /// Find or create the text channel `name` under category `category` in `guild`.
    /// Returns the channel id and whether it was just created.
    pub async fn ensure_channel(&self, guild: &str, category: &str, name: &str, topic: &str) -> Result<(String, bool), String> {
        let chans = self.channels(guild).await?;
        let cat = match chans.iter().find(|c| c.kind == 4 && c.name.as_deref() == Some(category)) {
            Some(c) => c.id.clone(),
            None => self.post_json(&format!("/guilds/{guild}/channels"), json!({"name": category, "type": 4})).await?["id"]
                .as_str().unwrap_or_default().to_owned(),
        };
        let slug = channel_slug(name);
        if let Some(c) = chans.iter().find(|c| c.kind == 0 && c.parent_id.as_deref() == Some(&cat) && c.name.as_deref() == Some(&slug)) {
            return Ok((c.id.clone(), false));
        }
        let v = self.post_json(&format!("/guilds/{guild}/channels"), json!({"name": slug, "type": 0, "parent_id": cat, "topic": topic})).await?;
        Ok((v["id"].as_str().unwrap_or_default().to_owned(), true))
    }

    /// The bot's effective guild-level permission bits in `guild`.
    pub async fn bot_permissions(&self, guild: &str, bot_id: &str) -> Result<u64, String> {
        let g = self.get(&format!("/guilds/{guild}")).await?;
        let m = self.get(&format!("/guilds/{guild}/members/{bot_id}")).await?;
        let mine: Vec<&str> = m["roles"].as_array().map(|r| r.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
        let mut bits = 0u64;
        for r in g["roles"].as_array().into_iter().flatten() {
            let id = r["id"].as_str().unwrap_or_default();
            if id == guild || mine.contains(&id) {
                bits |= r["permissions"].as_str().and_then(|p| p.parse::<u64>().ok()).unwrap_or(0);
            }
        }
        Ok(if bits & 8 != 0 { u64::MAX } else { bits })
    }

    pub async fn rename_channel(&self, channel: &str, name: &str) -> Result<(), String> {
        self.call(reqwest::Method::PATCH, &format!("/channels/{channel}"), Some(json!({"name": channel_slug(name)}))).await.map(|_| ())
    }

    pub async fn delete_channel(&self, channel: &str) -> Result<(), String> {
        self.call(reqwest::Method::DELETE, &format!("/channels/{channel}"), None).await.map(|_| ())
    }

    /// Create the channel webhook used for agent personas. Returns "id\ntoken".
    pub async fn create_webhook(&self, channel: &str) -> Result<String, String> {
        let v = self.post_json(&format!("/channels/{channel}/webhooks"), json!({"name": "Disco"})).await?;
        match (v["id"].as_str(), v["token"].as_str()) {
            (Some(id), Some(t)) => Ok(format!("{id}\n{t}")),
            _ => Err("webhook 没有返回 token".into()),
        }
    }

    /// Post as an agent persona through the channel webhook. Never pings anyone.
    pub async fn post_as(&self, webhook: &str, username: &str, avatar: Option<&str>, text: &str) -> Result<(), String> {
        let (wid, token) = webhook.split_once('\n').ok_or("bad webhook ref")?;
        for part in chunk(text) {
            let mut body = json!({"content": part, "username": persona_name(username), "allowed_mentions": {"parse": []}});
            if let Some(a) = avatar { body["avatar_url"] = json!(a); }
            self.post_json(&format!("/webhooks/{wid}/{token}"), body).await?;
        }
        Ok(())
    }

    /// Post a message with buttons `(custom_id, label, allow)`; returns the message id.
    /// Used for approvals and questions, so they can be answered from Discord too.
    pub async fn post_buttons(&self, channel: &str, text: &str, buttons: &[(String, String, bool)]) -> Result<String, String> {
        let rows: Vec<Value> = buttons.chunks(5).take(5).map(|row| json!({"type": 1, "components": row.iter().map(|(id, label, allow)| json!({
            "type": 2, "style": if *allow { 3 } else { 2 }, "custom_id": id, "label": label.chars().take(80).collect::<String>(),
        })).collect::<Vec<_>>()})).collect();
        let content: String = text.chars().take(1900).collect();
        let v = self.post_json(&format!("/channels/{channel}/messages"), json!({"content": content, "components": rows, "allowed_mentions": {"parse": []}})).await?;
        v["id"].as_str().map(str::to_owned).ok_or_else(|| "Discord 没有返回消息 id".into())
    }

    /// Replace a message's text and remove its buttons.
    pub async fn settle_message(&self, channel: &str, message: &str, text: &str) -> Result<(), String> {
        let content: String = text.chars().take(1900).collect();
        self.call(reqwest::Method::PATCH, &format!("/channels/{channel}/messages/{message}"), Some(json!({"content": content, "components": []}))).await.map(|_| ())
    }

    /// Answer a button press: `update` rewrites the pressed message (and drops its
    /// buttons); otherwise only the presser sees `text`.
    pub async fn interaction_reply(&self, id: &str, token: &str, update: bool, text: &str) -> Result<(), String> {
        let content: String = text.chars().take(1900).collect();
        let body = if update { json!({"type": 7, "data": {"content": content, "components": []}}) }
            else { json!({"type": 4, "data": {"content": content, "flags": 64}}) };
        self.post_json(&format!("/interactions/{id}/{token}/callback"), body).await.map(|_| ())
    }

    /// Post as the bot itself (used for messages typed in the app). Never pings anyone.
    pub async fn post(&self, channel: &str, text: &str) -> Result<(), String> {
        for part in chunk(text) {
            self.post_json(&format!("/channels/{channel}/messages"), json!({"content": part, "allowed_mentions": {"parse": []}})).await?;
        }
        Ok(())
    }
}

/// Discord channel names: lowercase, no spaces, at most 100 chars.
pub fn channel_slug(name: &str) -> String {
    let s: String = name.trim().to_lowercase().chars().map(|c| if c.is_whitespace() { '-' } else { c }).take(100).collect();
    if s.is_empty() { "agent".into() } else { s }
}

/// Webhook usernames may not contain "discord" and must be 1–80 chars.
fn persona_name(name: &str) -> String {
    let n: String = name.replace("discord", "disc0rd").replace("Discord", "Disc0rd").chars().take(80).collect();
    if n.trim().is_empty() { "agent".into() } else { n }
}

/// Split at line boundaries into Discord-sized (≤ 2000 chars) messages.
pub fn chunk(text: &str) -> Vec<String> {
    const MAX: usize = 1900;
    let mut out = Vec::new();
    let mut cur = String::new();
    for line in text.split_inclusive('\n') {
        if cur.chars().count() + line.chars().count() > MAX && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        if line.chars().count() > MAX {
            let chars: Vec<char> = line.chars().collect();
            for piece in chars.chunks(MAX) { out.push(piece.iter().collect()); }
        } else {
            cur.push_str(line);
        }
    }
    if !cur.trim().is_empty() { out.push(cur); }
    out
}

// ---------------------------------------------------------------- Gateway

/// A human message seen in a guild text channel.
#[derive(Debug, Clone)]
pub struct Incoming {
    pub guild_id: String,
    pub channel_id: String,
    pub author_id: String,
    pub author: String,
    pub content: String,
    /// A button press on one of our messages: (interaction id, token, custom_id).
    pub press: Option<(String, String, String)>,
}

#[derive(Debug, Clone)]
pub struct GatewayReport {
    pub bot_name: String,
    pub guilds: usize,
    pub ready_after: Duration,
}

enum End {
    /// Fatal: wrong token or missing intent. Don't retry.
    Fatal(String),
    Retry(String),
    Stopped,
}

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>;

async fn open_ws() -> Result<Ws, String> {
    let proxy = proxy_url();
    let tcp = tunnel(GATEWAY_HOST, proxy.as_deref()).await?;
    let url = format!("wss://{GATEWAY_HOST}/?v=10&encoding=json");
    let (ws, _) = tokio::time::timeout(Duration::from_secs(20), tokio_tungstenite::client_async_tls_with_config(url, tcp, None, None))
        .await
        .map_err(|_| "Gateway 握手超时".to_owned())?
        .map_err(|e| format!("Gateway 握手失败：{e}"))?;
    Ok(ws)
}

/// One gateway session: HELLO → IDENTIFY → events, with heartbeats.
async fn session(
    token: &str,
    stop: &mut watch::Receiver<bool>,
    mut on_ready: impl FnMut(&Value),
    mut on_message: impl FnMut(&Value) -> bool,
) -> End {
    let mut ws = match open_ws().await { Ok(w) => w, Err(e) => return End::Retry(e) };
    let mut seq: Option<u64> = None;
    let mut beat = tokio::time::interval(Duration::from_secs(3600));
    let mut acked = true;
    loop {
        tokio::select! {
            _ = stop.changed() => { let _ = ws.close(None).await; return End::Stopped; }
            _ = beat.tick() => {
                if !acked { return End::Retry("心跳无响应".into()); }
                acked = false;
                if ws.send(Message::text(json!({"op": 1, "d": seq}).to_string())).await.is_err() { return End::Retry("发送心跳失败".into()); }
            }
            msg = ws.next() => {
                let text = match msg {
                    Some(Ok(Message::Text(t))) => t.to_string(),
                    Some(Ok(Message::Close(f))) => {
                        let (code, reason) = f.map(|f| (u16::from(f.code), f.reason.to_string())).unwrap_or((0, String::new()));
                        return match code {
                            4004 => End::Fatal("Bot token 无效（4004）".into()),
                            4013 | 4014 => End::Fatal("缺少特权 intent：请在 Discord 开发者后台打开 Message Content Intent（4014）".into()),
                            _ => End::Retry(format!("Gateway 关闭 {code} {reason}")),
                        };
                    }
                    Some(Ok(_)) => continue,
                    Some(Err(e)) => return End::Retry(format!("Gateway 错误：{e}")),
                    None => return End::Retry("Gateway 连接结束".into()),
                };
                let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
                if let Some(s) = v["s"].as_u64() { seq = Some(s); }
                match v["op"].as_u64() {
                    Some(10) => {
                        let ms = v["d"]["heartbeat_interval"].as_u64().unwrap_or(41_250);
                        beat = tokio::time::interval_at(tokio::time::Instant::now() + Duration::from_millis(ms / 2), Duration::from_millis(ms));
                        let identify = json!({"op": 2, "d": {"token": token, "intents": INTENTS,
                            "properties": {"os": std::env::consts::OS, "browser": "disco", "device": "disco"}}});
                        if ws.send(Message::text(identify.to_string())).await.is_err() { return End::Retry("IDENTIFY 发送失败".into()); }
                    }
                    Some(11) => acked = true,
                    Some(1) => { let _ = ws.send(Message::text(json!({"op": 1, "d": seq}).to_string())).await; }
                    Some(7) => return End::Retry("Discord 要求重连".into()),
                    Some(9) => return End::Retry("会话失效".into()),
                    Some(0) => match v["t"].as_str() {
                        Some("READY") => on_ready(&v["d"]),
                        Some("MESSAGE_CREATE") if !on_message(&v["d"]) => return End::Stopped,
                        Some("INTERACTION_CREATE") if v["d"]["type"].as_u64() == Some(3) => {
                            let mut d = v["d"].clone();
                            d["__press"] = json!(true);
                            if !on_message(&d) { return End::Stopped; }
                        }
                        _ => {}
                    },
                    _ => {}
                }
            }
        }
    }
}

/// Connect, wait for READY, then disconnect. Used by diagnostics.
pub async fn gateway_check(token: String, timeout: Duration) -> Result<GatewayReport, String> {
    let t0 = Instant::now();
    let (stop_tx, mut stop_rx) = watch::channel(false);
    let mut report = None;
    let end = tokio::time::timeout(timeout, session(&token, &mut stop_rx, |d| {
        report = Some(GatewayReport {
            bot_name: d["user"]["username"].as_str().unwrap_or_default().into(),
            guilds: d["guilds"].as_array().map_or(0, Vec::len),
            ready_after: t0.elapsed(),
        });
        let _ = stop_tx.send(true);
    }, |_| true)).await;
    if let Some(r) = report { return Ok(r); }
    match end {
        Err(_) => Err("等待 READY 超时".into()),
        Ok(End::Fatal(e) | End::Retry(e)) => Err(e),
        Ok(End::Stopped) => Err("未收到 READY".into()),
    }
}

/// Long-running gateway: forwards human guild messages to `tx`, reports status,
/// reconnects with backoff until `stop` flips.
pub async fn run_gateway(token: String, tx: mpsc::Sender<Incoming>, mut stop: watch::Receiver<bool>, on_status: impl Fn(String) + Send + 'static) {
    let mut backoff = 2u64;
    loop {
        let via = proxy_url().map(|p| format!("（经代理 {p}）")).unwrap_or_default();
        on_status(format!("连接中{via}…"));
        let mut was_ready = false;
        let end = session(&token, &mut stop, |d| {
            was_ready = true;
            let guilds = d["guilds"].as_array().map_or(0, Vec::len);
            on_status(format!("在线 · {} · {guilds} 个服务器", d["user"]["username"].as_str().unwrap_or("bot")));
        }, |m| {
            if m["__press"].as_bool() == Some(true) {
                // Component interaction: the presser is `member.user` in guilds, `user` in DMs.
                let u = if m["member"]["user"].is_object() { &m["member"]["user"] } else { &m["user"] };
                let msg = Incoming {
                    guild_id: m["guild_id"].as_str().unwrap_or_default().into(),
                    channel_id: m["channel_id"].as_str().unwrap_or_default().into(),
                    author_id: u["id"].as_str().unwrap_or_default().into(),
                    author: u["global_name"].as_str().or(u["username"].as_str()).unwrap_or("someone").into(),
                    content: String::new(),
                    press: Some((m["id"].as_str().unwrap_or_default().into(), m["token"].as_str().unwrap_or_default().into(),
                        m["data"]["custom_id"].as_str().unwrap_or_default().into())),
                };
                return tx.try_send(msg).is_ok() || !tx.is_closed();
            }
            if m["author"]["bot"].as_bool() == Some(true) || !m["webhook_id"].is_null() { return true; }
            let (Some(guild), Some(content)) = (m["guild_id"].as_str(), m["content"].as_str()) else { return true };
            if content.is_empty() { return true; }
            let a = &m["author"];
            let msg = Incoming {
                guild_id: guild.into(),
                channel_id: m["channel_id"].as_str().unwrap_or_default().into(),
                author_id: a["id"].as_str().unwrap_or_default().into(),
                author: a["global_name"].as_str().or(a["username"].as_str()).unwrap_or("someone").into(),
                content: content.into(),
                press: None,
            };
            tx.try_send(msg).is_ok() || !tx.is_closed()
        }).await;
        match end {
            End::Stopped => { on_status("已断开".into()); return; }
            End::Fatal(e) => { on_status(e); return; }
            End::Retry(e) => {
                if was_ready { backoff = 2; }
                let hint = if proxy_url().is_none() && !was_ready { "；若在用 VPN，请开启系统代理或 TUN 模式" } else { "" };
                on_status(format!("{e}{hint}，{backoff} 秒后重试"));
                tokio::select! {
                    _ = stop.changed() => { on_status("已断开".into()); return; }
                    _ = tokio::time::sleep(Duration::from_secs(backoff)) => {}
                }
                backoff = (backoff * 2).min(60);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_respect_limit_and_utf8() {
        let long = "中".repeat(5000);
        let parts = chunk(&long);
        assert!(parts.iter().all(|p| p.chars().count() <= 2000));
        assert_eq!(parts.concat(), long);
        assert_eq!(chunk("a\nb"), ["a\nb"]);
    }

    #[test]
    fn reg_parsing() {
        let t = "    ProxyEnable    REG_DWORD    0x1\n    ProxyServer    REG_SZ    127.0.0.1:7890\n";
        assert_eq!(parse_reg(t).as_deref(), Some("http://127.0.0.1:7890"));
        assert_eq!(parse_reg(&t.replace("0x1", "0x0")), None);
        assert_eq!(parse_reg("ProxyEnable REG_DWORD 0x1\nProxyServer REG_SZ http=a:1;https=b:2\n").as_deref(), Some("http://b:2"));
    }

    #[test]
    fn scutil_parsing() {
        let s = "<dictionary> {\n  HTTPEnable : 1\n  HTTPPort : 7890\n  HTTPProxy : 127.0.0.1\n  HTTPSEnable : 1\n  HTTPSPort : 7891\n  HTTPSProxy : 127.0.0.1\n}";
        assert_eq!(parse_scutil(s).as_deref(), Some("http://127.0.0.1:7891"));
        let socks = "  SOCKSEnable : 1\n  SOCKSPort : 1080\n  SOCKSProxy : 127.0.0.1\n";
        assert_eq!(parse_scutil(socks).as_deref(), Some("socks5://127.0.0.1:1080"));
        assert_eq!(parse_scutil("  HTTPEnable : 0\n"), None);
    }

    #[test]
    fn base64_basic_auth() {
        assert_eq!(base64(b"user:pass"), "dXNlcjpwYXNz");
        assert_eq!(base64(b"a"), "YQ==");
        assert_eq!(base64(b"ab"), "YWI=");
    }

    #[test]
    fn slugs() {
        assert_eq!(channel_slug("Claude Code"), "claude-code");
        assert_eq!(persona_name("discord-bot"), "disc0rd-bot");
    }
}

#[cfg(test)]
mod live {
    /// Network smoke test: a bogus token must reach IDENTIFY and be rejected with 4004.
    #[tokio::test]
    #[ignore = "network"]
    async fn bogus_token_is_rejected_by_gateway() {
        super::install_crypto();
        let e = super::gateway_check("x".repeat(70), std::time::Duration::from_secs(20)).await.unwrap_err();
        assert!(e.contains("4004"), "{e}");
    }

    /// Prints what the bot can do in its guilds (needs a real token, e.g. via an injecting proxy).
    #[tokio::test]
    #[ignore = "network + token"]
    async fn rest_probe() {
        super::install_crypto();
        let r = super::Rest::new(std::env::var("DISCORD_TOKEN").unwrap_or_default());
        let (id, name) = r.me().await.unwrap();
        for g in r.guilds().await.unwrap() {
            let bits = r.bot_permissions(&g.id, &id).await.unwrap();
            println!("{name} in {} ({}): perms {bits} manage_channels={}", g.name, g.id, bits & 16 != 0);
        }
    }

    /// Full agent-channel lifecycle in the first guild: category, channel, webhook persona, cleanup.
    #[tokio::test]
    #[ignore = "network + token"]
    async fn agent_channel_lifecycle() {
        super::install_crypto();
        let r = super::Rest::new(std::env::var("DISCORD_TOKEN").unwrap_or_default());
        let g = r.guilds().await.unwrap().remove(0);
        let (ch, created) = r.ensure_channel(&g.id, "Disco Agents", "Probe Agent", "disco test").await.unwrap();
        let (again, created2) = r.ensure_channel(&g.id, "Disco Agents", "Probe Agent", "disco test").await.unwrap();
        assert!(created && !created2 && ch == again);
        let hook = r.create_webhook(&ch).await.unwrap();
        r.post_as(&hook, "probe-agent", None, "hello @everyone (should not ping)").await.unwrap();
        r.delete_channel(&ch).await.unwrap();
        println!("lifecycle ok in {}", g.name);
    }
}
