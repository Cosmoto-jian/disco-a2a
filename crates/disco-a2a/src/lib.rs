//! Thin wrapper over the official a2a-rs client. Everything else in Disco talks to
//! [`RemoteAgent`] and [`AgentEvent`], so an SDK change touches only this file.
#![forbid(unsafe_code)]

use std::sync::Arc;

use a2a::event::StreamResponse;
use a2a::{AgentCard, Message, Part, PartContent, Role, SendMessageRequest, TaskState};
use a2a_client::agent_card::AgentCardResolver;
use a2a_client::middleware::CallInterceptor;
use a2a_client::{A2AClient, A2AClientFactory, ServiceParams, Transport};
use disco_types::{AgentEvent, FilePart, TaskPhase};
use futures::{Stream, StreamExt};

#[derive(Debug, thiserror::Error)]
pub enum A2aError {
    #[error("a2a: {0}")]
    Protocol(#[from] a2a::A2AError),
    #[error("{0}")]
    Insecure(String),
    #[error("{0}")]
    Unreachable(String),
}

/// Static credential injected on every call. OAuth tokens are refreshed upstream
/// by disco-auth and arrive here as `Header("authorization", "Bearer …")`.
#[derive(Clone)]
pub enum Credential {
    None,
    Header { name: String, value: String },
}

struct AuthInterceptor(Credential);

#[async_trait::async_trait]
impl CallInterceptor for AuthInterceptor {
    async fn before(&self, _method: &str, params: &mut ServiceParams) -> Result<(), a2a::A2AError> {
        if let Credential::Header { name, value } = &self.0 {
            params.insert(name.clone(), vec![value.clone()]);
        }
        Ok(())
    }
}

pub struct RemoteAgent {
    pub card: AgentCard,
    client: A2AClient<Box<dyn Transport>>,
    /// For fetching files the agent sends back by url: always from `base`'s origin, with `cred`.
    base: String,
    cred: Credential,
    http: reqwest::Client,
}

impl RemoteAgent {
    /// Resolve `/.well-known/agent-card.json` under `base_url`, negotiate a binding.
    pub async fn connect(base_url: &str, cred: Credential) -> Result<Self, A2aError> {
        // Loopback and tailnet peers are reached directly: a system proxy (a VPN app)
        // would forward 100.x to a remote exit that can't see the tailnet.
        let direct = origin(base_url).is_some_and(|(_, h, _)| is_loopback(&h));
        let http = direct.then(|| reqwest::Client::builder().no_proxy().build().ok()).flatten();
        let card = AgentCardResolver::new(http.clone()).resolve(base_url).await.map_err(|e| unreachable_hint(base_url, e))?;
        if matches!(cred, Credential::Header { .. }) {
            check_cred_target(base_url, &serde_json::to_value(&card).unwrap_or_default())
                .map_err(A2aError::Insecure)?;
        }
        let mut factory = A2AClientFactory::builder().with_interceptor(Arc::new(AuthInterceptor(cred.clone())));
        let fetcher = http.clone().unwrap_or_default();
        if let Some(c) = http {
            factory = factory
                .register(Arc::new(a2a_client::jsonrpc::JsonRpcTransportFactory::new(Some(c.clone()))))
                .register(Arc::new(a2a_client::rest::RestTransportFactory::new(Some(c))));
        }
        let factory = factory.build();
        let client = factory.create_from_card(&card).await?;
        Ok(Self { card, client, base: base_url.to_owned(), cred, http: fetcher })
    }

    /// Send a text prompt (with any files, as inline A2A file parts) in `context_id`
    /// and stream normalised events.
    pub async fn prompt(
        &self,
        context_id: &str,
        text: &str,
        files: &[FilePart],
    ) -> Result<impl Stream<Item = AgentEvent> + Send + 'static, A2aError> {
        let parts = std::iter::once(Part::text(text)).chain(files.iter().map(file_part)).collect();
        let mut message = Message::new(Role::User, parts);
        message.context_id = Some(context_id.to_owned());
        let req = SendMessageRequest { message, configuration: None, metadata: None, tenant: None };
        let raw = self.client.send_streaming_message(&req).await?;
        Ok(raw.flat_map(|ev| futures::stream::iter(map_event(ev))))
    }
}

impl RemoteAgent {
    /// Download a file the agent sent back by url. Only the url's path is used, joined to
    /// the agent's own origin, so the credential never goes anywhere else. At most `max` bytes.
    pub async fn fetch(&self, url: &str, max: u64) -> Result<Vec<u8>, String> {
        let base = url::Url::parse(&self.base).map_err(|e| e.to_string())?;
        let path = url::Url::parse(url).or_else(|_| base.join(url)).map_err(|e| e.to_string())?;
        let mut at = base.clone();
        at.set_path(path.path());
        at.set_query(None);
        let mut req = self.http.get(at);
        if let Credential::Header { name, value } = &self.cred { req = req.header(name.as_str(), value.as_str()); }
        let mut r = req.send().await.map_err(|e| e.to_string())?;
        if !r.status().is_success() { return Err(format!("HTTP {}", r.status())); }
        let mut out = Vec::new();
        while let Some(c) = r.chunk().await.map_err(|e| e.to_string())? {
            out.extend_from_slice(&c);
            if out.len() as u64 > max { return Err("文件太大".into()); }
        }
        Ok(out)
    }
}

/// A2A file part: bytes inline (or a url), with name, type, size and SHA-256 in metadata.
pub fn file_part(f: &FilePart) -> Part {
    let mut p = match &f.url { Some(u) if f.bytes.is_empty() => Part::url(u.clone()), _ => Part::raw(f.bytes.clone()) };
    p.filename = Some(f.name.clone());
    p.media_type = f.mime.clone();
    p.metadata = Some([("sha256".to_owned(), f.sha256.clone().into()), ("size".to_owned(), f.size.into())].into_iter().collect());
    p
}

/// The file in an A2A part, if it is one (raw bytes or a url).
pub fn part_file(p: &Part) -> Option<FilePart> {
    let (bytes, url) = match &p.content {
        PartContent::Raw(b) => (b.clone(), None),
        PartContent::Url(u) => (Vec::new(), Some(u.clone())),
        _ => return None,
    };
    let meta = |k: &str| p.metadata.as_ref().and_then(|m| m.get(k).cloned());
    Some(FilePart {
        name: p.filename.clone().unwrap_or_else(|| "file".into()),
        mime: p.media_type.clone(),
        // Protobuf-JSON hops may turn the number into a float.
        size: meta("size").and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f as u64))).unwrap_or(bytes.len() as u64),
        sha256: meta("sha256").and_then(|v| v.as_str().map(str::to_lowercase)).unwrap_or_default(),
        url,
        bytes,
    })
}

fn phase(s: &TaskState) -> TaskPhase {
    match s {
        TaskState::Submitted | TaskState::Unspecified => TaskPhase::Submitted,
        TaskState::Working => TaskPhase::Working,
        TaskState::InputRequired => TaskPhase::InputRequired,
        TaskState::AuthRequired => TaskPhase::AuthRequired,
        TaskState::Completed => TaskPhase::Completed,
        TaskState::Canceled => TaskPhase::Canceled,
        TaskState::Failed => TaskPhase::Failed,
        TaskState::Rejected => TaskPhase::Rejected,
    }
}

fn texts(parts: &[Part]) -> impl Iterator<Item = AgentEvent> + '_ {
    parts.iter().filter_map(|p| match &p.content {
        PartContent::Text(t) => Some(AgentEvent::TextChunk { text: t.clone() }),
        _ => part_file(p).map(AgentEvent::File),
    })
}

fn map_event(ev: Result<StreamResponse, a2a::A2AError>) -> Vec<AgentEvent> {
    match ev {
        Err(e) => vec![AgentEvent::Done { phase: TaskPhase::Failed, stop_reason: Some(e.to_string()) }],
        Ok(StreamResponse::Message(m)) => texts(&m.parts).collect(),
        Ok(StreamResponse::ArtifactUpdate(a)) => texts(&a.artifact.parts).collect(),
        Ok(StreamResponse::StatusUpdate(u)) => status(&u.status),
        Ok(StreamResponse::Task(t)) => status(&t.status),
    }
}

fn status(s: &a2a::TaskStatus) -> Vec<AgentEvent> {
    let p = phase(&s.state);
    let mut out: Vec<AgentEvent> = s.message.iter().flat_map(|m| texts(&m.parts)).collect();
    out.push(if p.is_terminal() {
        AgentEvent::Done { phase: p, stop_reason: None }
    } else {
        AgentEvent::Phase { phase: p }
    });
    out
}

/// A tailnet address that can't be reached gets a plain-language reason.
fn unreachable_hint(base_url: &str, e: a2a::A2AError) -> A2aError {
    let msg = e.to_string();
    let tailnet = origin(base_url).is_some_and(|(_, h, _)| is_loopback(&h) && !h.starts_with("127.") && h != "localhost" && h != "[::1]");
    if tailnet && msg.contains("error sending request") {
        return A2aError::Unreachable(format!("连不上对方（{}）。请确认：对方的 Disco 开着；两台都在 Tailscale 里显示在线；VPN 没开 TUN/增强模式，或已把 100.64.0.0/10 设为直连；对方电脑的防火墙允许 Disco 接收连接（更新后 macOS 可能再弹一次「允许传入连接」，要点允许）。", origin(base_url).map(|o| format!("{}:{}", o.1, o.2)).unwrap_or_default()));
    }
    A2aError::Protocol(e)
}

/// (scheme, host, port) as a WHATWG parser (and so reqwest) will see it; `None`
/// for anything unparsable or host-less. Never hand-parse: `a\@b` fools splitters.
fn origin(url: &str) -> Option<(String, String, u16)> {
    let u = url::Url::parse(url).ok()?;
    Some((u.scheme().to_owned(), u.host_str()?.to_owned(), u.port_or_known_default()?))
}

/// Hosts where plain http is acceptable: loopback, or a Tailscale peer
/// (WireGuard already encrypts and authenticates the link).
fn is_loopback(h: &str) -> bool {
    if matches!(h, "localhost" | "127.0.0.1" | "[::1]") || h.ends_with(".ts.net") { return true; }
    h.parse::<std::net::Ipv4Addr>().is_ok_and(|ip| { let o = ip.octets(); o[0] == 100 && (64..128).contains(&o[1]) })
        || h.strip_prefix('[').and_then(|h| h.strip_suffix(']')).and_then(|h| h.parse::<std::net::Ipv6Addr>().ok()).is_some_and(|ip| ip.segments()[..3] == [0xfd7a, 0x115c, 0xa1e0])
}

/// A credential may only travel over https (or to loopback), and only to the
/// host the user entered: a hostile Agent Card must not redirect it elsewhere.
pub fn check_cred_target(base_url: &str, card: &serde_json::Value) -> Result<(), String> {
    let base = origin(base_url).ok_or("无效 URL")?;
    let (scheme, host, _) = &base;
    if scheme != "https" && !is_loopback(host) {
        return Err("带凭证的 Agent 必须使用 https".into());
    }
    fn urls<'a>(v: &'a serde_json::Value, out: &mut Vec<&'a str>) {
        match v {
            serde_json::Value::Object(m) => for (k, v) in m {
                // provider.url is the vendor's homepage, not an endpoint.
                if k == "provider" { continue; }
                if k == "url" && let Some(s) = v.as_str() { out.push(s); } else { urls(v, out); }
            },
            serde_json::Value::Array(a) => a.iter().for_each(|v| urls(v, out)),
            _ => {}
        }
    }
    let mut found = Vec::new();
    urls(card, &mut found);
    for u in found {
        match origin(u) {
            Some(o) if o == base => {}
            _ => return Err(format!("Agent Card 指向其他地址（{u}），已拒绝发送凭证")),
        }
    }
    Ok(())
}

#[cfg(test)]
mod file_tests {
    use super::*;

    #[test]
    fn file_part_survives_the_wire() {
        let f = FilePart { name: "a b.txt".into(), mime: Some("text/plain".into()), size: 5, sha256: "ab".repeat(32), url: None, bytes: b"hello".to_vec() };
        let json = serde_json::to_string(&file_part(&f)).unwrap();
        assert!(json.contains("\"raw\":\"aGVsbG8=\""), "{json}");
        let back: Part = serde_json::from_str(&json).unwrap();
        assert_eq!(part_file(&back), Some(f.clone()));
        let mut p = file_part(&f);
        p.metadata.as_mut().unwrap().insert("size".into(), serde_json::json!(5.0));
        assert_eq!(part_file(&p).map(|f| f.size), Some(5));
        let by_url = FilePart { bytes: Vec::new(), url: Some("http://100.64.0.1:7480/a/x/f/k".into()), ..f };
        let back: Part = serde_json::from_str(&serde_json::to_string(&file_part(&by_url)).unwrap()).unwrap();
        assert_eq!(part_file(&back), Some(by_url));
        assert_eq!(part_file(&Part::text("hi")), None);
    }
}

#[cfg(test)]
mod cred_tests {
    use super::check_cred_target;
    use serde_json::json;

    #[test]
    fn cred_target_rules() {
        let ok = json!({"supportedInterfaces": [{"url": "https://a.example/rpc"}], "provider": {"url": "https://vendor.example"}});
        assert!(check_cred_target("https://a.example", &ok).is_ok());
        assert!(check_cred_target("http://a.example", &ok).is_err());
        let evil = json!({"supportedInterfaces": [{"url": "https://evil.example/rpc"}]});
        assert!(check_cred_target("https://a.example", &evil).is_err());
        let ts = json!({"url": "http://mac.tail1.ts.net:7480/a/x/jsonrpc"});
        assert!(check_cred_target("http://mac.tail1.ts.net:7480/a/x", &ts).is_ok());
        assert!(check_cred_target("http://100.70.1.2:7480/a/x", &json!({"url": "http://100.70.1.2:7480/a/x/jsonrpc"})).is_ok());
        assert!(check_cred_target("http://192.168.1.2:7480", &json!({})).is_err());
        assert!(check_cred_target("http://[fd7a:115c:a1e0::1]:7480/a/x", &json!({"url": "http://[fd7a:115c:a1e0::1]:7480/a/x/jsonrpc"})).is_ok());
        assert!(check_cred_target("http://[fd00::1]:7480", &json!({})).is_err());
        let local = json!({"url": "http://127.0.0.1:9000/"});
        assert!(check_cred_target("http://127.0.0.1:9000", &local).is_ok());
        let trick = json!({"url": "https://a.example@evil.example/"});
        assert!(check_cred_target("https://a.example", &trick).is_err());
    }

    #[test]
    fn origin_follows_whatwg() {
        use super::origin;
        let o = |h: &str, p| Some(("https".to_owned(), h.to_owned(), p));
        // `\` is a path separator for special schemes: this goes to evil.example.
        assert_eq!(origin(r"https://evil.example\@a.example/rpc"), o("evil.example", 443));
        assert_eq!(origin("https://user:pw@A.Example/x"), o("a.example", 443));
        assert_eq!(origin("https://a.example:443/"), o("a.example", 443));
        assert_eq!(origin("http://a.example/").map(|o| o.2), Some(80));
        assert_eq!(origin("http://[::1]:9000/").map(|o| o.1), Some("[::1]".into()));
        for bad in ["", "a.example/rpc", "https://", "https://a b.example/", "mailto:x@a.example"] { assert_eq!(origin(bad), None, "{bad}"); }
    }

    #[test]
    fn cred_target_tricks() {
        let base = "https://a.example";
        for u in [r"https://evil.example\@a.example/rpc", "https://a.example:8443/rpc", "http://a.example/rpc", "not a url", "https://a.example.evil.example/"] {
            assert!(check_cred_target(base, &json!({"url": u})).is_err(), "{u}");
        }
        for u in ["https://a.example:443/rpc", "https://u@a.example/rpc", "HTTPS://A.EXAMPLE/rpc"] {
            assert!(check_cred_target(base, &json!({"url": u})).is_ok(), "{u}");
        }
        assert!(check_cred_target(r"https://evil.example\@a.example", &json!({"url": "https://a.example/rpc"})).is_err());
        assert!(check_cred_target("http://127.0.0.1:9000", &json!({"url": "http://127.0.0.1:9001/"})).is_err());
    }
}
