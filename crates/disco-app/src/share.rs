//! Sharing a local agent with someone on the same Tailscale network.
//!
//! The owner's Disco serves each shared agent as an A2A endpoint at
//! `http://<tailscale-host>:<port>/a/<share-id>`. Every share has its own random
//! bearer token (kept in the vault) that the owner can revoke at any time. Only
//! loopback and Tailscale peers may connect, only the two "send message" methods
//! are accepted, and tool permissions are decided on the owner's machine.
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use a2a::event::StreamResponse;
use a2a::*;
use a2a_server::*;
use axum::body::{Body, to_bytes};
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use disco_types::{AgentEvent, PermissionDecision};
use tauri::Manager;
use futures::stream::{self, BoxStream};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::{CmdResult, Core, St, err, secret_del, secret_get, secret_set, tailnet};

pub const SHARE: &str = "share/";
const PORTS: std::ops::RangeInclusive<u16> = 7480..=7489;
const SHARE_HEADER: &str = "x-disco-share";
const PEER_HEADER: &str = "x-disco-peer";
const INVITE_PREFIX: &str = "disco1:";

/// What a share allows. Stored as JSON in settings under `share/<id>`.
#[derive(Serialize, Deserialize, Clone)]
pub struct Share {
    pub agent: String,
    /// Who it was shared with (shown in the owner's chat list).
    pub guest: String,
    /// "chat": anything that needs approval is refused; "ask": the owner decides.
    pub mode: String,
    /// Messages per day; 0 = unlimited.
    pub limit: u32,
}

#[derive(Serialize)]
pub struct ShareView { id: String, agent: String, guest: String, mode: String, limit: u32, used_today: u32 }

#[derive(Serialize)]
pub struct ShareStatus {
    /// e.g. "http://my-mac.tail1234.ts.net:7480" once the server runs and Tailscale is found.
    base: Option<String>,
    problem: Option<String>,
    shares: Vec<ShareView>,
}

/// The running server: bound port, or the reason it could not start.
#[derive(Default)]
pub struct Server { port: Option<u16>, error: Option<String> }

#[derive(Serialize, Deserialize)]
struct Invite { url: String, token: String, agent: String, owner: String }

fn today() -> String {
    let d = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() / 86_400).unwrap_or(0);
    d.to_string()
}

impl Core {
    pub(crate) fn shares(&self) -> Vec<(String, Share)> {
        self.db().settings_with_prefix(SHARE).unwrap_or_default().into_iter()
            .filter_map(|(id, v)| Some((id, serde_json::from_str(&v).ok()?))).collect()
    }

    fn share(&self, id: &str) -> Option<Share> {
        serde_json::from_str(&self.db().setting(&format!("{SHARE}{id}"))?).ok()
    }

    fn used_today(&self, id: &str) -> u32 {
        self.db().setting(&format!("share_used/{id}/{}", today())).and_then(|v| v.parse().ok()).unwrap_or(0)
    }

    /// Keep shares pointing at a renamed agent; drop them when it is removed.
    pub(crate) fn shares_follow(&self, old: &str, new: Option<&str>) {
        for (id, mut s) in self.shares().into_iter().filter(|(_, s)| s.agent == old) {
            match new {
                Some(n) => { s.agent = n.to_owned(); let _ = self.db().set_setting(&format!("{SHARE}{id}"), &serde_json::to_string(&s).unwrap_or_default()); }
                None => self.drop_share(&id),
            }
        }
    }

    fn drop_share(&self, id: &str) {
        let _ = self.db().set_setting(&format!("{SHARE}{id}"), "");
        secret_del(&format!("share/{id}"));
    }

    /// Start the share server once (idempotent). Binds all interfaces; the guard
    /// below refuses everyone who is not loopback or on the tailnet.
    pub(crate) fn serve_shares(self: &Arc<Self>) {
        let mut srv = self.server.lock().unwrap_or_else(|p| p.into_inner());
        if srv.port.is_some() { return; }
        let mut last = String::new();
        for port in PORTS {
            match std::net::TcpListener::bind(("0.0.0.0", port)) {
                Ok(l) => {
                    let _ = l.set_nonblocking(true);
                    let core = self.clone();
                    tauri::async_runtime::spawn(async move {
                        let Ok(listener) = tokio::net::TcpListener::from_std(l) else { return };
                        let app = router(core).into_make_service_with_connect_info::<SocketAddr>();
                        let _ = axum::serve(listener, app).await;
                    });
                    *srv = Server { port: Some(port), error: None };
                    return;
                }
                Err(e) => last = e.to_string(),
            }
        }
        srv.error = Some(format!("端口 7480–7489 都被占用：{last}"));
    }

    pub(crate) fn net_policy(&self) -> tailnet::Policy {
        self.db().setting(tailnet::POLICY).and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default()
    }

    fn share_base(&self) -> Result<String, String> {
        let port = {
            let s = self.server.lock().unwrap_or_else(|p| p.into_inner());
            s.port.ok_or_else(|| s.error.clone().unwrap_or_else(|| "共享服务未启动".into()))?
        };
        let me = crate::tailnet::me().ok_or("没检测到 Tailscale：请先安装 Tailscale 并登录，再回来生成邀请")?;
        Ok(format!("http://{}:{port}", me.node))
    }

    /// Run one guest message as a turn of the shared agent. The owner sees it live
    /// in the `peer:<id>` thread, where permission prompts also appear.
    async fn peer_turn(&self, id: &str, device: &str, guest_ctx: &str, text: &str, out: mpsc::UnboundedSender<AgentEvent>) -> Result<String, String> {
        let share = self.share(id).ok_or("这个共享已被撤销")?;
        if self.net_policy().quiet_now() { return Err("对方设置了免打扰，现在不处理消息，请稍后再试".into()); }
        let used = self.used_today(id);
        if share.limit > 0 && used >= share.limit {
            return Err(format!("今天的 {} 次额度已用完", share.limit));
        }
        let _ = self.db().set_setting(&format!("share_used/{id}/{}", today()), &(used + 1).to_string());
        let thread = format!("peer:{id}");
        self.post_message(&thread, &share.guest, text);
        let context = format!("peer:{id}|{guest_ctx}");
        let deny = share.mode != "ask";
        let core = self.app.state::<Arc<Core>>().inner().clone();
        let agent = share.agent.clone();
        // Returns false for events the owner's UI should not see (refused permission prompts).
        let res = self.turn_with(&thread, &share.agent, &context, text, move |ev| {
            if let AgentEvent::PermissionAsked { request_id, .. } = ev {
                if deny {
                    let (core, agent, rid) = (core.clone(), agent.clone(), request_id.clone());
                    tauri::async_runtime::spawn(async move {
                        core.hub.decide(&agent, &rid, PermissionDecision::Cancel).await;
                    });
                    return false;
                }
                return true;
            }
            let _ = out.send(ev.clone());
            true
        }).await;
        let _ = self.db().audit(&format!("peer:{}@{device}", share.guest), Some(&share.agent), "peer_prompt", if res.is_ok() { "ok" } else { "error" });
        match &res {
            Ok(a) => self.post_message(&thread, &share.agent, a.trim()),
            Err(e) => self.post_message(&thread, "system", &crate::fail_line(&share.agent, e)),
        }
        res
    }
}

fn eq_ct(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn router(core: Arc<Core>) -> axum::Router {
    let handler = Arc::new(DefaultRequestHandler::new(PeerExec(core.clone()), InMemoryTaskStore::new()));
    axum::Router::new()
        .route("/a/{id}/.well-known/agent-card.json", axum::routing::get(card))
        .with_state(core.clone())
        .nest("/a/{id}/jsonrpc", a2a_server::jsonrpc::jsonrpc_router(handler))
        .layer(middleware::from_fn_with_state(core, guard))
}

fn deny(code: StatusCode, msg: &str) -> Response { (code, msg.to_owned()).into_response() }

/// Tailnet/loopback only; the device must pass the access policy (when Tailscale
/// can say who it is); per-share bearer token; only message methods.
async fn guard(State(core): State<Arc<Core>>, ConnectInfo(peer): ConnectInfo<SocketAddr>, req: Request, next: Next) -> Response {
    let ip = peer.ip().to_canonical();
    if !(ip.is_loopback() || tailnet::is_tailnet(&ip)) { return deny(StatusCode::FORBIDDEN, "tailnet only"); }
    let mut who = String::from("local");
    if !ip.is_loopback() {
        let policy = core.net_policy();
        match tauri::async_runtime::spawn_blocking(move || tailnet::whois(&ip)).await.ok().flatten() {
            Some(id) => {
                if let Err(e) = policy.admit(&id) { return deny(StatusCode::FORBIDDEN, e); }
                who = id.node;
            }
            // Without the CLI we can't name the device: a restrictive policy then fails closed.
            None if policy.mode == "allowlist" || !policy.deny.is_empty() => return deny(StatusCode::FORBIDDEN, "无法确认 Tailscale 身份"),
            None => who = ip.to_string(),
        }
    }
    let path = req.uri().path().to_owned();
    let Some(rest) = path.strip_prefix("/a/") else { return deny(StatusCode::NOT_FOUND, "not found") };
    let (id, tail) = rest.split_once('/').unwrap_or((rest, ""));
    let id = id.to_owned();
    if core.share(&id).is_none() { return deny(StatusCode::NOT_FOUND, "no such share"); }
    if tail == ".well-known/agent-card.json" { return next.run(req).await; }
    let token = req.headers().get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).unwrap_or("");
    let ok = secret_get(&format!("share/{id}")).is_some_and(|t| eq_ct(t.as_bytes(), token.as_bytes()));
    if !ok { return deny(StatusCode::UNAUTHORIZED, "bad token"); }
    let (mut parts, body) = req.into_parts();
    let Ok(bytes) = to_bytes(body, 1 << 20).await else { return deny(StatusCode::PAYLOAD_TOO_LARGE, "too large") };
    let method = serde_json::from_slice::<serde_json::Value>(&bytes).ok().and_then(|v| v["method"].as_str().map(str::to_owned)).unwrap_or_default();
    if method != methods::SEND_MESSAGE && method != methods::SEND_STREAMING_MESSAGE {
        return deny(StatusCode::FORBIDDEN, "method not allowed");
    }
    parts.headers.insert(SHARE_HEADER, HeaderValue::from_str(&id).unwrap_or(HeaderValue::from_static("")));
    parts.headers.insert(PEER_HEADER, HeaderValue::from_str(&who).unwrap_or(HeaderValue::from_static("")));
    next.run(Request::from_parts(parts, Body::from(bytes))).await
}

async fn card(State(core): State<Arc<Core>>, axum::extract::Path(id): axum::extract::Path<String>, headers: axum::http::HeaderMap) -> Response {
    let Some(s) = core.share(&id) else { return deny(StatusCode::NOT_FOUND, "no such share") };
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("127.0.0.1");
    let card = AgentCard {
        name: s.agent.clone(), description: format!("{} 通过 Disco 共享的 {}", owner_name(), s.agent), version: a2a::VERSION.into(), provider: None,
        capabilities: AgentCapabilities { streaming: Some(true), push_notifications: Some(false), extensions: None, extended_agent_card: None },
        skills: vec![], default_input_modes: vec!["text/plain".into()], default_output_modes: vec!["text/plain".into()],
        supported_interfaces: vec![AgentInterface::new(format!("http://{host}/a/{id}/jsonrpc"), TRANSPORT_PROTOCOL_JSONRPC)],
        security_schemes: None, security_requirements: None, documentation_url: None, icon_url: None, signatures: None,
    };
    axum::Json(card).into_response()
}

/// The person using this computer, as others see them.
pub(crate) fn owner_name() -> String {
    std::env::var(if cfg!(windows) { "USERNAME" } else { "USER" }).ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "friend".into())
}

struct PeerExec(Arc<Core>);

fn status(task_id: &TaskId, context_id: &str, state: TaskState, message: Option<String>) -> StreamResponse {
    StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
        task_id: task_id.clone(), context_id: context_id.to_owned(),
        status: TaskStatus { state, message: message.map(|m| Message::new(Role::Agent, vec![Part::text(m)])), timestamp: Some(chrono::Utc::now()) },
        metadata: None,
    })
}

impl AgentExecutor for PeerExec {
    fn execute(&self, ctx: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let id = ctx.service_params.get(SHARE_HEADER).and_then(|v| v.first()).cloned().unwrap_or_default();
        let device = ctx.service_params.get(PEER_HEADER).and_then(|v| v.first()).cloned().unwrap_or_default();
        let text: String = ctx.message.as_ref().map(|m| m.parts.iter().filter_map(|p| match &p.content {
            PartContent::Text(t) => Some(t.as_str()),
            _ => None,
        }).collect::<Vec<_>>().join("\n")).unwrap_or_default();
        let (task_id, context_id) = ctx.task_info();
        let (tx, rx) = mpsc::unbounded_channel::<Result<StreamResponse, A2AError>>();
        let core = self.0.clone();
        let _ = tx.send(Ok(status(&task_id, &context_id, TaskState::Working, None)));
        tauri::async_runtime::spawn(async move {
            let (etx, mut erx) = mpsc::unbounded_channel::<AgentEvent>();
            let (fwd_tx, tid, cid) = (tx.clone(), task_id.clone(), context_id.clone());
            let fwd = tauri::async_runtime::spawn(async move {
                // The first chunk creates the artifact; later ones append to it.
                let mut append = false;
                while let Some(ev) = erx.recv().await {
                    if let AgentEvent::TextChunk { text } = ev {
                        let _ = fwd_tx.send(Ok(StreamResponse::ArtifactUpdate(TaskArtifactUpdateEvent {
                            task_id: tid.clone(), context_id: cid.clone(),
                            artifact: Artifact { artifact_id: "answer".into(), name: None, description: None, parts: vec![Part::text(text)], metadata: None, extensions: None },
                            append: Some(append), last_chunk: Some(false), metadata: None,
                        })));
                        append = true;
                    }
                }
            });
            let res = core.peer_turn(&id, &device, &context_id, &text, etx).await;
            let _ = fwd.await;
            let _ = tx.send(Ok(match res {
                Ok(_) => status(&task_id, &context_id, TaskState::Completed, None),
                Err(e) => status(&task_id, &context_id, TaskState::Failed, Some(e)),
            }));
        });
        Box::pin(stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|x| (x, rx)) }))
    }

    fn cancel(&self, _ctx: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        Box::pin(stream::empty())
    }
}

// ---------- commands

#[tauri::command]
pub fn share_status(core: St<'_>) -> ShareStatus {
    let (base, problem) = match core.share_base() { Ok(b) => (Some(b), None), Err(e) => (None, Some(e)) };
    let shares = core.shares().into_iter().map(|(id, s)| ShareView { used_today: core.used_today(&id), id, agent: s.agent, guest: s.guest, mode: s.mode, limit: s.limit }).collect();
    ShareStatus { base, problem, shares }
}

/// Share `agent` with `guest`; returns the invite code to send them.
#[tauri::command]
pub fn create_share(core: St<'_>, agent: String, guest: String, mode: String, limit: u32) -> CmdResult<String> {
    core.agent(&agent)?;
    let guest = guest.trim();
    if guest.is_empty() { return Err("写一下对方的名字".into()); }
    core.inner().serve_shares();
    let base = core.share_base()?;
    let id = uuid::Uuid::new_v4().simple().to_string()[..12].to_owned();
    let token = format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
    secret_set(&format!("share/{id}"), &token)?;
    let share = Share { agent: agent.clone(), guest: guest.to_owned(), mode: if mode == "ask" { mode } else { "chat".into() }, limit };
    core.db().set_setting(&format!("{SHARE}{id}"), &serde_json::to_string(&share).map_err(err)?).map_err(err)?;
    let _ = core.db().audit("user", Some(&agent), "share", guest);
    invite(&base, &id, &token, &agent)
}

/// The invite code of an existing share (to send it again).
#[tauri::command]
pub fn share_invite(core: St<'_>, id: String) -> CmdResult<String> {
    let s = core.share(&id).ok_or("这个共享已被撤销")?;
    let token = secret_get(&format!("share/{id}")).ok_or("找不到共享密钥")?;
    invite(&core.share_base()?, &id, &token, &s.agent)
}

#[tauri::command]
pub fn revoke_share(core: St<'_>, id: String) {
    core.drop_share(&id);
    let _ = core.db().audit("user", None, "revoke_share", &id);
}

fn invite(base: &str, id: &str, token: &str, agent: &str) -> CmdResult<String> {
    let inv = Invite { url: format!("{base}/a/{id}"), token: token.to_owned(), agent: agent.to_owned(), owner: owner_name() };
    Ok(format!("{INVITE_PREFIX}{}", b64(serde_json::to_string(&inv).map_err(err)?.as_bytes())))
}

/// Guest side: add the agent an invite code points at.
#[tauri::command]
pub async fn accept_invite(core: St<'_>, code: String, name: Option<String>) -> CmdResult<String> {
    let raw = code.trim().strip_prefix(INVITE_PREFIX).ok_or("这不是 Disco 邀请码（应以 disco1: 开头）")?;
    let inv: Invite = unb64(raw).and_then(|b| serde_json::from_slice(&b).ok()).ok_or("邀请码不完整，请让对方重新复制")?;
    let slug = |s: &str| s.chars().filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_').collect::<String>();
    let name = name.filter(|n| !n.trim().is_empty()).unwrap_or_else(|| {
        let n = format!("{}-{}", slug(&inv.agent), slug(&inv.owner));
        n.chars().take(32).collect()
    });
    let spec = crate::NewAgent::remote(name, inv.url, inv.token);
    let names = crate::add_agent_rows(&core, spec).await?;
    core.inner().warm(Some(names.clone()));
    Ok(names.into_iter().next().unwrap_or_default())
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn b64(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = c.iter().enumerate().fold(0u32, |acc, (i, b)| acc | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..=c.len() { out.push(B64[(n >> (18 - 6 * i)) as usize & 63] as char); }
    }
    out
}

fn unb64(s: &str) -> Option<Vec<u8>> {
    let mut lut: HashMap<u8, u32> = HashMap::new();
    for (i, c) in B64.iter().enumerate() { lut.insert(*c, i as u32); }
    let bytes: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace() && *b != b'=').collect();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for c in bytes.chunks(4) {
        if c.len() == 1 { return None; }
        let n = c.iter().enumerate().try_fold(0u32, |acc, (i, b)| Some(acc | (lut.get(b)? << (18 - 6 * i))))?;
        for i in 0..c.len() - 1 { out.push((n >> (16 - 8 * i)) as u8); }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn b64_roundtrip() {
        for s in ["", "a", "ab", "abc", "abcd", "{\"url\":\"http://x.ts.net:7480/a/1\"}"] {
            assert_eq!(unb64(&b64(s.as_bytes())).unwrap(), s.as_bytes());
        }
    }
}
