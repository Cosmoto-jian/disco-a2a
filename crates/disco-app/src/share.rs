//! Sharing a local agent with someone on the same Tailscale network.
//!
//! The owner's Disco serves each shared agent as an A2A endpoint at
//! `http://<tailscale-host>:<port>/a/<share-id>`. Every share has its own random
//! bearer token (kept in the vault) that the owner can revoke at any time. Only
//! loopback and Tailscale peers may connect (the server listens only on
//! 127.0.0.1 and this machine's own Tailscale addresses), only the two "send
//! message" methods are accepted, and tool permissions are decided on the owner's machine.
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use a2a::event::StreamResponse;
use a2a::*;
use a2a_server::*;
use axum::body::{Body, to_bytes};
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use disco_store::Store;
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
/// One answer for a bad token, an unknown share and a revoked one: nobody learns which ids exist.
const GONE: &str = "邀请码无效或共享已被撤销，请让对方重新发一个";

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
    /// e.g. "http://100.101.2.3:7480" (the Tailscale IP) once the server runs and Tailscale is found.
    base: Option<String>,
    problem: Option<String>,
    shares: Vec<ShareView>,
}

/// The running server: bound port, the Tailscale addresses it listens on
/// (besides 127.0.0.1), or the reason it could not start / listen.
#[derive(Default)]
pub struct Server { port: Option<u16>, error: Option<String> }

#[derive(Serialize, Deserialize)]
struct Invite { url: String, token: String, agent: String, owner: String }

/// Today in local time, like quiet hours: the daily limit resets at the owner's midnight.
fn today() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

fn used_key(id: &str) -> String { format!("share_used/{id}/{}", today()) }

/// Count one message under `key` unless `limit` (0 = none) is reached. Read and write
/// happen under the one `&Store` borrow (one db lock), so two turns can't both pass.
fn take_quota(db: &Store, key: &str, limit: u32) -> bool {
    let used: u32 = db.setting(key).and_then(|v| v.parse().ok()).unwrap_or(0);
    if limit > 0 && used >= limit { return false; }
    db.set_setting(key, &(used + 1).to_string()).is_ok()
}

/// Give back a message that never reached the agent.
fn refund_quota(db: &Store, key: &str) {
    let used: u32 = db.setting(key).and_then(|v| v.parse().ok()).unwrap_or(0);
    let left = used.saturating_sub(1);
    let _ = db.set_setting(key, &if left == 0 { String::new() } else { left.to_string() });
}

/// Peer turns in flight: (ticket, share id, context). Revoking a share takes its
/// entries out, which both cancels those turns and tells them they were revoked.
static RUNNING: Mutex<Vec<(u64, String, String)>> = Mutex::new(Vec::new());

/// A registered peer turn; unregisters on drop.
struct Ticket(u64);

impl Ticket {
    fn new(id: &str, context: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let t = NEXT.fetch_add(1, Ordering::Relaxed);
        RUNNING.lock().unwrap_or_else(|p| p.into_inner()).push((t, id.to_owned(), context.to_owned()));
        Self(t)
    }
}

/// Still registered, i.e. its share was not revoked meanwhile.
fn live(ticket: u64) -> bool {
    RUNNING.lock().unwrap_or_else(|p| p.into_inner()).iter().any(|e| e.0 == ticket)
}

impl Drop for Ticket {
    fn drop(&mut self) { RUNNING.lock().unwrap_or_else(|p| p.into_inner()).retain(|e| e.0 != self.0); }
}

/// Unregister every running turn of share `id`; returns their contexts.
fn take_running(id: &str) -> Vec<String> {
    let mut r = RUNNING.lock().unwrap_or_else(|p| p.into_inner());
    let (gone, keep): (Vec<_>, Vec<_>) = r.drain(..).partition(|e| e.1 == id);
    *r = keep;
    gone.into_iter().map(|e| e.2).collect()
}

impl Core {
    pub(crate) fn shares(&self) -> Vec<(String, Share)> {
        self.db().settings_with_prefix(SHARE).unwrap_or_default().into_iter()
            .filter_map(|(id, v)| Some((id, serde_json::from_str(&v).ok()?))).collect()
    }

    fn share(&self, id: &str) -> Option<Share> {
        serde_json::from_str(&self.db().setting(&format!("{SHARE}{id}"))?).ok()
    }

    pub(crate) fn used_today(&self, id: &str) -> u32 {
        self.db().setting(&used_key(id)).and_then(|v| v.parse().ok()).unwrap_or(0)
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

    /// Start the share server (idempotent) on all interfaces, as v0.6–v1.5.12 did
    /// (binding only the detected Tailscale address broke sharing on real machines).
    /// `guard` admits loopback and Tailscale peers only, and every call needs the
    /// guest's token, so a LAN or carrier-NAT neighbour gets nothing.
    pub(crate) fn serve_shares(self: &Arc<Self>) {
        let mut srv = self.server.lock().unwrap_or_else(|p| p.into_inner());
        if srv.port.is_some() { return; }
        let mut last = String::new();
        for port in PORTS {
            match std::net::TcpListener::bind((Ipv4Addr::UNSPECIFIED, port)) {
                Ok(l) => {
                    self.listen(l);
                    // Tailscale's IPv6 address, where the OS keeps v6 separate; harmless if it fails.
                    if let Ok(l6) = std::net::TcpListener::bind((std::net::Ipv6Addr::UNSPECIFIED, port)) { self.listen(l6); }
                    *srv = Server { port: Some(port), error: None };
                    return;
                }
                Err(e) => last = e.to_string(),
            }
        }
        srv.error = Some(format!("端口 7480–7489 都被占用：{last}"));
    }

    fn listen(self: &Arc<Self>, l: std::net::TcpListener) {
        let _ = l.set_nonblocking(true);
        let core = self.clone();
        tauri::async_runtime::spawn(async move {
            let Ok(listener) = tokio::net::TcpListener::from_std(l) else { return };
            let app = router(core).into_make_service_with_connect_info::<SocketAddr>();
            let _ = axum::serve(listener, app).await;
        });
    }

    pub(crate) fn net_policy(&self) -> tailnet::Policy {
        self.db().setting(tailnet::POLICY).and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default()
    }

    pub(crate) fn share_base(&self) -> Result<String, String> {
        let (port, error) = {
            let s = self.server.lock().unwrap_or_else(|p| p.into_inner());
            (s.port, s.error.clone())
        };
        let port = port.ok_or_else(|| error.unwrap_or_else(|| "共享服务未启动".into()))?;
        // The IP, not the MagicDNS name: a guest may have Tailscale DNS off (e.g. under another VPN).
        let host = match tailnet::best_ip(&tailnet::my_ips()) {
            Some(IpAddr::V6(v)) => format!("[{v}]"),
            Some(v4) => v4.to_string(),
            None if crate::selftest_mode() => "127.0.0.1".into(),
            None => return Err(match tailnet::diagnose() { Err(why) => format!("{why}，再回来生成邀请"), Ok(_) => "Tailscale 刚连上，请再试一次".into() }),
        };
        Ok(format!("http://{host}:{port}"))
    }

    /// Run one guest message as a turn of the shared agent. The owner sees it live
    /// in the `peer:<id>` thread, where permission prompts also appear.
    pub(crate) async fn peer_turn(&self, id: &str, device: &str, guest_ctx: &str, text: &str, out: mpsc::UnboundedSender<AgentEvent>) -> Result<String, String> {
        let context = format!("peer:{id}|{guest_ctx}");
        // Register before reading the share: a revoke either comes first (no share) or finds the ticket.
        let ticket = Ticket::new(id, &context);
        let share = self.share(id).ok_or("这个共享已被撤销")?;
        if self.net_policy().quiet_now() { return Err("对方设置了免打扰，现在不处理消息，请稍后再试".into()); }
        let key = used_key(id);
        let counted = take_quota(&self.db(), &key, share.limit);
        if !counted { return Err(format!("今天的 {} 次额度已用完", share.limit)); }
        let thread = format!("peer:{id}");
        // The guest's Discord-synced group the agent was last used in lets the owner talk back.
        // Only a group is remembered: a later DM turn must not erase it. The guest names the
        // channel, so it is only a candidate: `peer_check` opens the chat once Discord confirms
        // the owner's bot can see that channel and the owner's own account is in its server.
        if let Some(ch) = discord_group(guest_ctx) {
            let fresh = format!("group:{ch}");
            if self.peer_channel(id).as_deref() != Some(ch) {
                let _ = self.db().set_setting(&format!("{PEER_CTX}{id}"), &fresh);
                let _ = self.db().set_setting(&format!("{PEER_OK}{id}"), CHECKING);
                self.agents_changed();
            }
            let (core, id) = (self.app.state::<Arc<Core>>().inner().clone(), id.to_owned());
            tauri::async_runtime::spawn(async move {
                let before = core.db().setting(&format!("{PEER_OK}{id}"));
                if core.peer_check(&id).await != before { core.agents_changed(); }
            });
        }
        self.post_message(&thread, &share.guest, text);
        let deny = share.mode != "ask";
        let core = self.app.state::<Arc<Core>>().inner().clone();
        let agent = share.agent.clone();
        let (tk, ctx) = (ticket.0, context.clone());
        // `started`: the agent produced something, so the message counts. `stopping`: revoked mid-turn.
        let (started, stopping) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
        let seen = started.clone();
        // Returns false for events the owner's UI should not see (refused permission prompts).
        let res = self.turn_with(&thread, &share.agent, &context, text, move |ev| {
            seen.store(true, Ordering::Relaxed);
            if !live(tk) {
                // Revoked after the turn began: revoke's own cancel may have come too early.
                if !stopping.swap(true, Ordering::Relaxed) {
                    let (core, agent, ctx) = (core.clone(), agent.clone(), ctx.clone());
                    tauri::async_runtime::spawn(async move { core.hub.cancel(&agent, &ctx).await; });
                }
                return false;
            }
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
        let res = if live(ticket.0) { res } else { Err("这个共享已被撤销".into()) };
        drop(ticket);
        if res.is_err() && !started.load(Ordering::Relaxed) { refund_quota(&self.db(), &key); }
        let _ = self.db().audit(&format!("peer:{}@{device}", share.guest), Some(&share.agent), "peer_prompt", if res.is_ok() { "ok" } else { "error" });
        match &res {
            Ok(a) => self.post_message(&thread, &share.agent, a.trim()),
            Err(e) => self.post_message(&thread, "system", &crate::fail_line(&share.agent, e)),
        }
        res
    }
}

pub(crate) const PEER_CTX: &str = "peer_ctx/";
/// Result of the last talk check per share: "" = ready, else what is still missing.
pub(crate) const PEER_OK: &str = "peer_ok/";
const CHECKING: &str = "正在确认你和你的 Bot 是否都在对方的服务器里…";

impl Core {
    /// The Discord channel of the guest's group the shared agent was last used in, if any.
    pub(crate) fn peer_channel(&self, id: &str) -> Option<String> {
        discord_group(&self.db().setting(&format!("{PEER_CTX}{id}"))?).map(str::to_owned)
    }

    /// What still keeps the owner from talking in the guest's group ("" = nothing).
    pub(crate) fn peer_missing(&self, id: &str) -> Option<String> {
        self.peer_channel(id)?;
        Some(self.db().setting(&format!("{PEER_OK}{id}")).unwrap_or_else(|| CHECKING.into()))
    }

    /// Check (and remember) whether both the owner's Discord account and bot are in the
    /// server of the guest's group: only then does the spectator view turn into a chat.
    pub(crate) async fn peer_check(&self, id: &str) -> Option<String> {
        let ch = self.peer_channel(id)?;
        let mut owners = self.owners.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let rest = self.discord_rest();
        // Owners are learned when the gateway connects; ask Discord if that has not happened yet.
        if owners.is_empty() && let Some(d) = &rest { owners = d.owner_ids().await.unwrap_or_default(); }
        let missing = match rest {
            None => "先在设置里连上 Discord，才能加入对方的群聊".to_owned(),
            Some(d) => match d.channel_guild(&ch).await {
                Err(_) => "你的 Bot 还不在对方的服务器里：把设置里的「邀请 Bot」链接发给对方，请他授权".into(),
                Ok(guild) => {
                    let mut me = false;
                    for o in &owners { if d.is_member(&guild, o).await { me = true; break; } }
                    if me { String::new() } else { "你的 Discord 账号还没进对方的服务器：请对方发一个邀请链接给你".into() }
                }
            },
        };
        let _ = self.db().set_setting(&format!("{PEER_OK}{id}"), &missing);
        Some(missing)
    }

    /// The owner speaks in the guest's group: post to its Discord channel as the owner,
    /// exactly like typing there. The guest's Disco routes it (and any @mentions) as usual.
    pub(crate) async fn peer_say(&self, id: &str, text: &str) -> Result<(), String> {
        let ch = self.peer_channel(id).ok_or("对方最近不是在 Discord 同步的群里用你的 agent，没有可以回话的群")?;
        if let Some(m) = self.peer_missing(id).filter(|m| !m.is_empty()) {
            self.post_message(&format!("peer:{id}"), "system", &m);
            return Err(m);
        }
        let d = self.discord_rest().ok_or("先在设置里连上 Discord，才能在对方的群里说话")?;
        let thread = format!("peer:{id}");
        self.post_message(&thread, "me", text);
        let r = d.post(&ch, &format!("**{}**：{}", owner_name(), self.with_pings(text))).await;
        if let Err(e) = &r { self.post_message(&thread, "system", &format!("Discord 发送失败：{e}（你的 Bot 需要在对方的 Discord 服务器里）")); }
        r
    }
}

/// The Discord channel id in an A2A context `group:<channel>` (local groups have none).
/// The Discord channel in a guest's group context: `group:<ch>`, or the team and
/// delegated forms `team:group:<ch>:<agent>` and `ask:<agent>:group:<ch>`.
fn discord_group(ctx: &str) -> Option<&str> {
    let rest = ctx.strip_prefix("team:").or_else(|| ctx.strip_prefix("ask:").and_then(|r| r.split_once(':').map(|(_, t)| t))).unwrap_or(ctx);
    let ch = rest.strip_prefix("group:")?;
    let ch = ch.split_once(':').map_or(ch, |(c, _)| c);
    (!ch.is_empty() && ch.bytes().all(|b| b.is_ascii_digit())).then_some(ch)
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

/// A refusal the guest's A2A client can read: a JSON-RPC error, not plain text.
fn deny(code: StatusCode, msg: &str) -> Response {
    let body = serde_json::json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32001, "message": msg } });
    (code, [(header::CONTENT_TYPE, "application/json")], body.to_string()).into_response()
}

/// Tailnet/loopback only; per-share bearer token (checked before anything else
/// looks at the share or asks Tailscale); then the device must pass the access
/// policy (when Tailscale can say who it is); only message methods. The agent card
/// is public (A2A clients fetch it without credentials) and names only the agent.
async fn guard(State(core): State<Arc<Core>>, ConnectInfo(peer): ConnectInfo<SocketAddr>, req: Request, next: Next) -> Response {
    let ip = peer.ip().to_canonical();
    if !(ip.is_loopback() || tailnet::is_tailnet(&ip)) { return deny(StatusCode::FORBIDDEN, "tailnet only"); }
    let path = req.uri().path().to_owned();
    let Some(rest) = path.strip_prefix("/a/") else { return deny(StatusCode::NOT_FOUND, "not found") };
    let (id, tail) = rest.split_once('/').unwrap_or((rest, ""));
    let id = id.to_owned();
    if tail == ".well-known/agent-card.json" { return next.run(req).await; }
    let token = req.headers().get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).unwrap_or("");
    let secret = secret_get(&format!("share/{id}"));
    let ok = !token.is_empty() && secret.is_some_and(|t| eq_ct(t.as_bytes(), token.as_bytes()));
    if !ok || core.share(&id).is_none() { return deny(StatusCode::UNAUTHORIZED, GONE); }
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
    let Some(s) = core.share(&id) else { return deny(StatusCode::NOT_FOUND, GONE) };
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("127.0.0.1");
    let card = AgentCard {
        name: s.agent.clone(), description: format!("通过 Disco 共享的 {}", s.agent), version: a2a::VERSION.into(), provider: None,
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

/// Re-check whether the owner can talk in the guest's group; returns what is missing ("" = ready).
#[tauri::command]
pub async fn peer_check(core: St<'_>, id: String) -> CmdResult<Option<String>> {
    Ok(core.peer_check(&id).await)
}

#[tauri::command]
pub fn share_status(core: St<'_>) -> ShareStatus {
    // Picks up Tailscale addresses that appeared since the server started.
    if !core.shares().is_empty() { core.inner().serve_shares(); }
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
    let agent = core.share(&id).map(|s| s.agent);
    core.drop_share(&id);
    // Stop the guest's turns that are already running (a turn starting later finds no share).
    let running = take_running(&id);
    if let Some(agent) = agent && !running.is_empty() {
        let core = core.inner().clone();
        tauri::async_runtime::spawn(async move {
            for ctx in running { core.hub.cancel(&agent, &ctx).await; }
        });
    }
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
    core.agents_changed();
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
    fn quota_is_counted_once_and_refunded() {
        let db = Store::open_in_memory().unwrap();
        assert!(take_quota(&db, "k", 2) && take_quota(&db, "k", 2) && !take_quota(&db, "k", 2));
        refund_quota(&db, "k");
        assert!(take_quota(&db, "k", 2) && !take_quota(&db, "k", 2));
        refund_quota(&db, "k"); refund_quota(&db, "k"); refund_quota(&db, "k");
        assert_eq!(db.setting("k"), None);
        assert!((0..5).all(|_| take_quota(&db, "u", 0)));
        let d = today();
        assert!(d.len() == 10 && chrono::NaiveDate::parse_from_str(&d, "%Y-%m-%d").is_ok(), "{d}");
    }

    #[test]
    fn revoke_takes_only_that_shares_turns() {
        let (a, b) = (Ticket::new("rv-a", "peer:rv-a|x"), Ticket::new("rv-b", "peer:rv-b|y"));
        assert!(live(a.0) && live(b.0));
        assert_eq!(take_running("rv-a"), vec!["peer:rv-a|x".to_owned()]);
        assert!(!live(a.0) && live(b.0));
        let bt = b.0;
        drop(b);
        assert!(!live(bt) && take_running("rv-b").is_empty());
        drop(a);
    }

    #[test]
    fn b64_roundtrip() {
        for s in ["", "a", "ab", "abc", "abcd", "{\"url\":\"http://x.ts.net:7480/a/1\"}"] {
            assert_eq!(unb64(&b64(s.as_bytes())).unwrap(), s.as_bytes());
        }
    }
}
