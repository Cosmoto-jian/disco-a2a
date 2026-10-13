//! Who is on the other end of a tailnet connection, and whether they may talk to us.
//!
//! The share server asks Tailscale itself who connected (`tailscale whois`) and
//! checks the answer against one local access policy: a rule is a hostname, a
//! MagicDNS name, `tag:NAME` or `user:LOGIN`, and deny always wins. Quiet hours
//! keep guests from waking your agents at night.
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Settings key of the policy (JSON).
pub const POLICY: &str = "tailnet_policy";

/// A connecting machine, as Tailscale vouches for it.
#[derive(Clone, Debug, Default, Serialize, PartialEq)]
pub struct Ident { pub node: String, pub host: String, pub user: String, pub tags: Vec<String> }

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Policy {
    /// `tailnet`: every authenticated device the tailnet lets through; `allowlist`: only `allow`.
    pub mode: String,
    pub allow: Vec<String>,
    pub deny: Vec<String>,
    /// "HH:MM" local time; guests are turned away in between.
    pub quiet_start: String,
    pub quiet_end: String,
}

impl Default for Policy {
    fn default() -> Self {
        Self { mode: "tailnet".into(), allow: vec![], deny: vec![], quiet_start: String::new(), quiet_end: String::new() }
    }
}

pub fn valid_rule(rule: &str) -> bool {
    let r = rule.trim().to_lowercase();
    if r.is_empty() || r.len() > 320 || r.chars().any(|c| c.is_whitespace() || c.is_control()) { return false; }
    if let Some(u) = r.strip_prefix("user:") { return !u.is_empty() && !u.contains([':', '/', '*', '\\']); }
    if let Some(t) = r.strip_prefix("tag:") {
        return t.chars().next().is_some_and(|c| c.is_ascii_alphanumeric()) && t.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    }
    let r = r.trim_end_matches('.');
    r.len() <= 253 && r.split('.').all(|l| !l.is_empty() && l.len() <= 63 && !l.starts_with('-') && !l.ends_with('-') && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
}

pub fn matches(rule: &str, who: &Ident) -> bool {
    let r = rule.trim().to_lowercase();
    if r.starts_with("tag:") { return who.tags.iter().any(|t| t.to_lowercase() == r); }
    if let Some(u) = r.strip_prefix("user:") { return !who.user.is_empty() && who.user.to_lowercase() == u; }
    let r = r.trim_end_matches('.');
    let node = who.node.to_lowercase();
    !r.is_empty() && (r == node || r == node.split('.').next().unwrap_or("") || r == who.host.to_lowercase())
}

impl Policy {
    /// Normalise and check what the user typed; an invalid rule is an error, never ignored.
    pub fn cleaned(mut self) -> Result<Self, String> {
        for list in [&mut self.allow, &mut self.deny] {
            let mut out: Vec<String> = Vec::new();
            for r in list.iter().map(|r| r.trim().to_lowercase()).filter(|r| !r.is_empty()) {
                if !valid_rule(&r) { return Err(format!("无效的规则：{r}（可以写设备名、MagicDNS 名、tag:名称 或 user:登录名）")); }
                if !out.contains(&r) { out.push(r); }
            }
            *list = out;
        }
        if self.mode != "allowlist" { self.mode = "tailnet".into(); }
        for t in [&self.quiet_start, &self.quiet_end] {
            if !t.is_empty() && hm(t).is_none() { return Err(format!("时间格式应为 HH:MM：{t}")); }
        }
        Ok(self)
    }

    /// May this machine reach us at all? `Err` says why not.
    pub fn admit(&self, who: &Ident) -> Result<(), &'static str> {
        if self.deny.iter().any(|r| matches(r, who)) { return Err("该设备已被拒绝"); }
        if self.mode == "allowlist" && !self.allow.iter().any(|r| matches(r, who)) { return Err("该设备不在允许名单里"); }
        Ok(())
    }

    /// Whether it is quiet hours right now.
    pub fn quiet_now(&self) -> bool {
        use chrono::Timelike;
        let t = chrono::Local::now();
        self.quiet_at(t.hour() * 60 + t.minute())
    }

    /// Whether `minutes` (local, since midnight) falls in quiet hours.
    pub fn quiet_at(&self, minutes: u32) -> bool {
        let (Some(s), Some(e)) = (hm(&self.quiet_start), hm(&self.quiet_end)) else { return false };
        if s == e { return false; }
        if s < e { (s..e).contains(&minutes) } else { minutes >= s || minutes < e }
    }
}

fn hm(t: &str) -> Option<u32> {
    let (h, m) = t.trim().split_once(':')?;
    let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60).then_some(h * 60 + m)
}

pub fn is_tailnet(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => { let o = v.octets(); o[0] == 100 && (64..128).contains(&o[1]) }
        IpAddr::V6(v) => v.segments()[..3] == [0xfd7a, 0x115c, 0xa1e0] || v.to_ipv4_mapped().is_some_and(|m| is_tailnet(&IpAddr::V4(m))),
    }
}

// ───────────────────────── talking to Tailscale ─────────────────────────

const CLIS: [&str; 4] = ["/Applications/Tailscale.app/Contents/MacOS/Tailscale", "/opt/homebrew/bin/tailscale", "/usr/local/bin/tailscale", "tailscale"];

fn run(args: &[&str]) -> Option<Value> {
    static WORKS: OnceLock<Mutex<Option<&'static str>>> = OnceLock::new();
    let known = *WORKS.get_or_init(Mutex::default).lock().unwrap_or_else(|p| p.into_inner());
    for c in known.into_iter().chain(CLIS.into_iter().filter(|c| Some(*c) != known)) {
        let Ok(o) = std::process::Command::new(c).args(args).output() else { continue };
        if !o.status.success() { continue; }
        *WORKS.get_or_init(Mutex::default).lock().unwrap_or_else(|p| p.into_inner()) = Some(c);
        return serde_json::from_slice(&o.stdout).ok();
    }
    None
}

/// `tailscale status --json`, cached for a few seconds.
pub fn status() -> Option<Value> {
    static CACHE: Mutex<Option<(Instant, Value)>> = Mutex::new(None);
    let mut c = CACHE.lock().unwrap_or_else(|p| p.into_inner());
    if let Some((t, v)) = c.as_ref() && t.elapsed() < Duration::from_secs(5) { return Some(v.clone()); }
    let v = run(&["status", "--json"])?;
    *c = Some((Instant::now(), v.clone()));
    Some(v)
}

/// A machine on the tailnet.
#[derive(Clone, Debug, Serialize)]
pub struct Node { pub node: String, pub host: String, pub ip: String, pub online: bool, pub me: bool }

fn node_of(n: &Value, me: bool, running: bool) -> Option<Node> {
    let node = n["DNSName"].as_str()?.trim_end_matches('.').to_lowercase();
    let ips: Vec<&str> = n["TailscaleIPs"].as_array()?.iter().filter_map(Value::as_str).collect();
    let ip = ips.iter().find(|i| !i.contains(':')).or(ips.first())?.to_string();
    if node.is_empty() { return None; }
    Some(Node { node, host: n["HostName"].as_str().unwrap_or("").into(), ip, online: if me { running } else { n["Online"].as_bool() == Some(true) }, me })
}

pub fn nodes() -> Vec<Node> {
    let Some(st) = status() else { return vec![] };
    let running = st["BackendState"].as_str() == Some("Running");
    let mut v: Vec<Node> = node_of(&st["Self"], true, running).into_iter().collect();
    if let Some(peers) = st["Peer"].as_object() { v.extend(peers.values().filter_map(|n| node_of(n, false, running))); }
    v
}

/// This Mac on the tailnet.
pub fn me() -> Option<Node> {
    if let Some(n) = nodes().into_iter().find(|n| n.me) { return Some(n); }
    // The CLI may be missing (App Store build): find a 100.64/10 address on an interface.
    let o = std::process::Command::new("ifconfig").output().ok()?;
    String::from_utf8_lossy(&o.stdout).split_whitespace().collect::<Vec<_>>().windows(2)
        .find_map(|w| (w[0] == "inet" && w[1].parse::<IpAddr>().is_ok_and(|ip| is_tailnet(&ip))).then(|| w[1].to_owned()))
        .map(|ip| Node { node: ip.clone(), host: String::new(), ip, online: true, me: true })
}

/// Who `ip` is, according to Tailscale (cached for a minute). None if it can't say.
pub fn whois(ip: &IpAddr) -> Option<Ident> {
    type Cache = Mutex<HashMap<IpAddr, (Instant, Option<Ident>)>>;
    static CACHE: OnceLock<Cache> = OnceLock::new();
    let cache = CACHE.get_or_init(Mutex::default);
    if let Some((t, v)) = cache.lock().unwrap_or_else(|p| p.into_inner()).get(ip) && t.elapsed() < Duration::from_secs(60) { return v.clone(); }
    let ip_s = ip.to_canonical().to_string();
    let v = run(&["whois", "--json", &ip_s]).and_then(|d| {
        let node = d["Node"]["Name"].as_str()?.trim_end_matches('.').to_lowercase();
        (!node.is_empty()).then(|| Ident {
            node,
            host: d["Node"]["Hostinfo"]["Hostname"].as_str().unwrap_or("").to_lowercase(),
            user: d["UserProfile"]["LoginName"].as_str().unwrap_or("").into(),
            tags: d["Node"]["Tags"].as_array().map(|a| a.iter().filter_map(|t| t.as_str().map(str::to_owned)).collect()).unwrap_or_default(),
        })
    });
    cache.lock().unwrap_or_else(|p| p.into_inner()).insert(*ip, (Instant::now(), v.clone()));
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn who() -> Ident { Ident { node: "box.tail1.ts.net".into(), host: "Box".into(), user: "me@x.com".into(), tags: vec!["tag:agent".into()] } }

    #[test]
    fn rules_and_policy() {
        for r in ["box", "box.tail1.ts.net", "tag:agent", "user:me@x.com"] { assert!(valid_rule(r) && matches(r, &who()), "{r}"); }
        for r in ["", "a b", "tag:", "user:a/b", "-x"] { assert!(!valid_rule(r), "{r}"); }
        assert!(!matches("other", &who()));
        let mut p = Policy::default();
        assert!(p.admit(&who()).is_ok());
        p.mode = "allowlist".into();
        assert!(p.admit(&who()).is_err());
        p.allow = vec!["tag:agent".into()];
        assert!(p.admit(&who()).is_ok());
        p.deny = vec!["box".into()];
        assert!(p.admit(&who()).is_err());
        assert!(Policy { allow: vec!["bad rule".into()], ..Default::default() }.cleaned().is_err());
    }

    #[test]
    fn quiet_hours() {
        let p = Policy { quiet_start: "23:00".into(), quiet_end: "08:00".into(), ..Default::default() };
        assert!(p.quiet_at(23 * 60 + 5) && p.quiet_at(60) && !p.quiet_at(12 * 60));
        assert!(!Policy::default().quiet_at(60));
        assert!(Policy { quiet_start: "25:00".into(), ..Default::default() }.cleaned().is_err());
    }

    #[test]
    fn tailnet_ips() {
        assert!(is_tailnet(&"100.101.2.3".parse().unwrap()) && !is_tailnet(&"100.128.0.1".parse().unwrap()));
        assert!(is_tailnet(&"fd7a:115c:a1e0::1".parse().unwrap()));
    }
}
