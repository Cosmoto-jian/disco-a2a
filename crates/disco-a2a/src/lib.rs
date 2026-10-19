//! Thin wrapper over the official a2a-rs client. Everything else in Disco talks to
//! [`RemoteAgent`] and [`AgentEvent`], so an SDK change touches only this file.
#![forbid(unsafe_code)]

use std::sync::Arc;

use a2a::event::StreamResponse;
use a2a::{AgentCard, Message, Part, PartContent, Role, SendMessageRequest, TaskState};
use a2a_client::agent_card::AgentCardResolver;
use a2a_client::middleware::CallInterceptor;
use a2a_client::{A2AClient, A2AClientFactory, ServiceParams, Transport};
use disco_types::{AgentEvent, TaskPhase};
use futures::{Stream, StreamExt};

#[derive(Debug, thiserror::Error)]
pub enum A2aError {
    #[error("a2a: {0}")]
    Protocol(#[from] a2a::A2AError),
    #[error("{0}")]
    Insecure(String),
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
}

impl RemoteAgent {
    /// Resolve `/.well-known/agent-card.json` under `base_url`, negotiate a binding.
    pub async fn connect(base_url: &str, cred: Credential) -> Result<Self, A2aError> {
        let card = AgentCardResolver::new(None).resolve(base_url).await?;
        if matches!(cred, Credential::Header { .. }) {
            check_cred_target(base_url, &serde_json::to_value(&card).unwrap_or_default())
                .map_err(A2aError::Insecure)?;
        }
        let factory = A2AClientFactory::builder()
            .with_interceptor(Arc::new(AuthInterceptor(cred)))
            .build();
        let client = factory.create_from_card(&card).await?;
        Ok(Self { card, client })
    }

    /// Send a text prompt in `context_id` and stream normalised events.
    pub async fn prompt(
        &self,
        context_id: &str,
        text: &str,
    ) -> Result<impl Stream<Item = AgentEvent> + Send + 'static, A2aError> {
        let mut message = Message::new(Role::User, vec![Part::text(text)]);
        message.context_id = Some(context_id.to_owned());
        let req = SendMessageRequest { message, configuration: None, metadata: None, tenant: None };
        let raw = self.client.send_streaming_message(&req).await?;
        Ok(raw.flat_map(|ev| futures::stream::iter(map_event(ev))))
    }
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
        _ => None,
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
