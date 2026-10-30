//! Cloudflare named tunnel: the alternative to Tailscale for cross-device shares.
//!
//! The owner signs in with their own Cloudflare account (`cloudflared tunnel login`),
//! picks a hostname on one of their domains, and Disco creates a named tunnel routed to
//! it and runs `cloudflared` pointing at the share server's tunnel listener (share.rs
//! `serve_tunnel`). Invites then carry `https://<hostname>/a/<id>`; guests need no
//! Tailscale. Either way works per install (setting `share_via`); Tailscale invites
//! already sent keep working.
use std::io::BufRead;
use std::path::PathBuf;
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tauri::Manager;

use crate::{CmdResult, Core, St, err, hub, secret_del, secret_get, secret_set};

/// "cloudflare" = invites go through the tunnel; anything else = Tailscale.
pub const VIA: &str = "share_via";
/// The tunnel this install created (JSON `Conf`).
pub const CONF: &str = "cf_tunnel";
/// "1" once an Access service token is saved: tunnel requests must then carry Access's JWT.
const ACCESS_ON: &str = "cf_access";
/// Vault: the service token as `client id\nclient secret`.
const ACCESS_KEY: &str = "cf_access";

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct Conf { pub name: String, pub id: String, pub hostname: String }

/// The running `cloudflared` and what it last said. `state`: "" (off), "starting", "running", "error".
struct Proc { child: Option<Child>, state: &'static str, log: Vec<String>, login_url: Option<String> }
static PROC: Mutex<Proc> = Mutex::new(Proc { child: None, state: "", log: Vec::new(), login_url: None });
/// Bumped on every start/stop, so a dead process's reader thread can't overwrite a newer state.
static GEN: AtomicU64 = AtomicU64::new(0);

fn proc() -> std::sync::MutexGuard<'static, Proc> { PROC.lock().unwrap_or_else(|p| p.into_inner()) }

#[derive(Serialize)]
pub struct Status {
    /// cloudflared's path, when found.
    bin: Option<String>,
    logged_in: bool,
    hostname: String,
    via: String,
    /// "off" | "starting" | "running" | "error"
    state: String,
    error: Option<String>,
    /// The sign-in page, in case cloudflared could not open the browser itself.
    login_url: Option<String>,
    access: bool,
    /// The Access service token's Client ID (the secret never leaves the vault).
    access_id: Option<String>,
}

pub(crate) fn via_cloudflare(core: &Core) -> bool { core.db().setting(VIA).as_deref() == Some("cloudflare") }

pub(crate) fn conf(core: &Core) -> Option<Conf> {
    core.db().setting(CONF).and_then(|j| serde_json::from_str(&j).ok())
}

pub(crate) fn access_on(core: &Core) -> bool { core.db().setting(ACCESS_ON).as_deref() == Some("1") }

/// Headers a guest's Disco sends on every request to get past Cloudflare Access.
pub(crate) fn access_headers(core: &Core) -> Vec<(String, String)> {
    if !access_on(core) { return Vec::new(); }
    secret_get(ACCESS_KEY).and_then(|s| s.split_once('\n').map(|(id, sec)| vec![
        ("cf-access-client-id".to_owned(), id.to_owned()), ("cf-access-client-secret".to_owned(), sec.to_owned())])).unwrap_or_default()
}

fn appdata(core: &Core) -> PathBuf { core.app.path().app_data_dir().unwrap_or_else(|_| std::env::temp_dir()) }

const EXE: &str = if cfg!(windows) { "cloudflared.exe" } else { "cloudflared" };

/// Our own download first, then PATH, then where the official installers put it.
pub(crate) fn binary(core: &Core) -> Option<PathBuf> {
    let own = appdata(core).join("bin").join(EXE);
    if own.is_file() { return Some(own); }
    hub::which("cloudflared", &core.hub.user_path).or_else(|| {
        let spots: &[&str] = if cfg!(windows) {
            &[r"C:\Program Files (x86)\cloudflared\cloudflared.exe", r"C:\Program Files\cloudflared\cloudflared.exe"]
        } else { &["/opt/homebrew/bin/cloudflared", "/usr/local/bin/cloudflared"] };
        spots.iter().map(PathBuf::from).find(|p| p.is_file())
    })
}

/// Written by `cloudflared tunnel login`.
fn cert() -> PathBuf {
    std::env::var_os("TUNNEL_ORIGIN_CERT").map(PathBuf::from).unwrap_or_else(|| hub::dirs_home().join(".cloudflared").join("cert.pem"))
}

/// Run cloudflared to completion; Err = its last meaningful output line.
fn run(bin: &PathBuf, args: &[&str]) -> Result<String, String> {
    let o = hub::quiet_cmd(bin).args(args).stdin(Stdio::null()).output().map_err(|e| format!("运行 cloudflared 失败：{e}"))?;
    let (out, errs) = (String::from_utf8_lossy(&o.stdout).into_owned(), String::from_utf8_lossy(&o.stderr).into_owned());
    if o.status.success() { return Ok(format!("{out}\n{errs}")); }
    Err(last_error(&errs).or_else(|| last_error(&out)).unwrap_or_else(|| format!("cloudflared 退出：{}", o.status)))
}

/// The last error-looking line of cloudflared's log, without the timestamp and level.
fn last_error(log: &str) -> Option<String> {
    let lines: Vec<&str> = log.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let line = lines.iter().rev().find(|l| l.contains(" ERR ") || l.starts_with("error") || l.contains("failed")).or(lines.last())?;
    let msg = line.split_once(" ERR ").map_or(*line, |(_, m)| m).trim();
    Some(msg.chars().take(300).collect())
}

/// `sub.example.com`, from whatever the user pasted ("https://sub.example.com/").
pub(crate) fn clean_host(raw: &str) -> Result<String, String> {
    let h = raw.trim().trim_start_matches("https://").trim_start_matches("http://").trim_end_matches('/').to_ascii_lowercase();
    let ok = h.contains('.') && !h.starts_with('.') && !h.ends_with('.') && !h.contains("..")
        && h.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.');
    if ok { Ok(h) } else { Err("填一个你在 Cloudflare 上的域名下的子域名，如 disco.example.com".into()) }
}

/// The tunnel id in `cloudflared tunnel create -o json` output (or any UUID it printed).
pub(crate) fn tunnel_id(out: &str) -> Option<String> {
    let json = out.find('{').and_then(|i| serde_json::Deserializer::from_str(&out[i..]).into_iter::<serde_json::Value>().next()?.ok());
    if let Some(id) = json.as_ref().and_then(|v| v["id"].as_str()) { return Some(id.to_owned()); }
    out.as_bytes().windows(36).find(|w| w.iter().enumerate().all(|(j, c)| if [8, 13, 18, 23].contains(&j) { *c == b'-' } else { c.is_ascii_hexdigit() }))
        .map(|w| String::from_utf8_lossy(w).into_owned())
}

/// The config `cloudflared tunnel run` reads: everything for `hostname` to the tunnel listener.
pub(crate) fn config_yaml(c: &Conf, cred: &std::path::Path, port: u16) -> String {
    // Single-quoted YAML takes Windows backslashes literally.
    format!("tunnel: {}\ncredentials-file: '{}'\nno-autoupdate: true\ningress:\n  - hostname: {}\n    service: http://127.0.0.1:{port}\n  - service: http_status:404\n",
        c.id, cred.display().to_string().replace('\'', "''"), c.hostname)
}

fn cred_file(core: &Core, c: &Conf) -> PathBuf { appdata(core).join("cloudflared").join(format!("{}.json", c.id)) }

/// Start (or restart) `cloudflared` for the configured tunnel.
pub(crate) fn start(core: &Arc<Core>) -> Result<(), String> {
    let c = conf(core).filter(|c| !c.id.is_empty()).ok_or("还没有设置隧道")?;
    let bin = binary(core).ok_or("没找到 cloudflared")?;
    let port = core.serve_tunnel()?;
    let cred = cred_file(core, &c);
    if !cred.is_file() { return Err("隧道凭据文件不见了，请重新设置域名".into()); }
    let cfg = cred.with_file_name("config.yml");
    std::fs::write(&cfg, config_yaml(&c, &cred, port)).map_err(err)?;
    stop();
    let mut child = hub::quiet_cmd(&bin).arg("tunnel").arg("--config").arg(&cfg).arg("run")
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn().map_err(|e| format!("启动 cloudflared 失败：{e}"))?;
    let run_id = GEN.fetch_add(1, Ordering::SeqCst) + 1;
    let stderr = child.stderr.take();
    {
        let mut p = proc();
        *p = Proc { child: Some(child), state: "starting", log: Vec::new(), login_url: p.login_url.take() };
    }
    std::thread::spawn(move || {
        if let Some(e) = stderr {
            for line in std::io::BufReader::new(e).lines().map_while(Result::ok) {
                let mut p = proc();
                if GEN.load(Ordering::SeqCst) != run_id { return; }
                if line.contains("Registered tunnel connection") { p.state = "running"; }
                p.log.push(line);
                if p.log.len() > 8 { p.log.remove(0); }
            }
        }
        let mut p = proc();
        if GEN.load(Ordering::SeqCst) == run_id && !p.state.is_empty() { p.state = "error"; }
    });
    Ok(())
}

/// Stop `cloudflared` (also on quit).
pub(crate) fn stop() {
    GEN.fetch_add(1, Ordering::SeqCst);
    let child = { let mut p = proc(); p.state = ""; p.child.take() };
    if let Some(mut c) = child {
        let _ = c.kill();
        std::thread::spawn(move || { let _ = c.wait(); });
    }
}

/// At launch: bring the tunnel back if this install shares through it.
pub(crate) fn resume(core: &Arc<Core>) {
    if via_cloudflare(core) && conf(core).is_some_and(|c| !c.id.is_empty()) { let _ = start(core); }
}

fn status(core: &Core) -> Status {
    let c = conf(core).unwrap_or_default();
    let p = proc();
    let state = match p.state { "" => "off", s => s }.to_owned();
    let error = (state == "error").then(|| last_error(&p.log.join("\n")).unwrap_or_else(|| "cloudflared 已退出".into()));
    Status {
        bin: binary(core).map(|b| b.display().to_string()), logged_in: cert().is_file(), hostname: c.hostname,
        via: if via_cloudflare(core) { "cloudflare" } else { "tailscale" }.into(), state, error, login_url: p.login_url.clone(),
        access: access_on(core), access_id: secret_get(ACCESS_KEY).and_then(|s| s.split_once('\n').map(|(id, _)| id.to_owned())),
    }
}

// ---------- commands

#[tauri::command]
pub fn tunnel_status(core: St<'_>) -> Status { status(&core) }

/// Download the official cloudflared from Cloudflare's GitHub releases into Disco's own folder.
#[tauri::command]
pub async fn tunnel_install(core: St<'_>) -> CmdResult<Status> {
    let asset = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "cloudflared-darwin-arm64.tgz",
        ("macos", _) => "cloudflared-darwin-amd64.tgz",
        ("windows", _) => "cloudflared-windows-amd64.exe",
        ("linux", "aarch64") => "cloudflared-linux-arm64",
        _ => "cloudflared-linux-amd64",
    };
    let url = format!("https://github.com/cloudflare/cloudflared/releases/latest/download/{asset}");
    let dir = appdata(&core).join("bin");
    std::fs::create_dir_all(&dir).map_err(err)?;
    let r = crate::openai::client(&core.hub.user_env).get(&url).send().await.map_err(|e| format!("下载 cloudflared 失败：{e}"))?;
    if !r.status().is_success() { return Err(format!("下载 cloudflared 失败：HTTP {}", r.status())); }
    let bytes = r.bytes().await.map_err(|e| format!("下载 cloudflared 失败：{e}"))?;
    let dst = dir.join(EXE);
    if asset.ends_with(".tgz") {
        let tgz = dir.join("cloudflared.tgz");
        std::fs::write(&tgz, &bytes).map_err(err)?;
        let ok = hub::quiet_cmd("tar").arg("-xzf").arg(&tgz).arg("-C").arg(&dir).status().is_ok_and(|s| s.success());
        let _ = std::fs::remove_file(&tgz);
        if !ok { return Err("解压 cloudflared 失败".into()); }
    } else {
        std::fs::write(&dst, &bytes).map_err(err)?;
    }
    #[cfg(unix)]
    { use std::os::unix::fs::PermissionsExt; let _ = std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(0o755)); }
    run(&dst, &["--version"]).map_err(|e| format!("cloudflared 装好了但运行不了：{e}"))?;
    Ok(status(&core))
}

/// `cloudflared tunnel login`: it opens the Cloudflare sign-in page; the user picks the
/// domain there. Returns once the page's address is known; poll `tunnel_status` for
/// `logged_in`.
#[tauri::command]
pub async fn tunnel_login(core: St<'_>) -> CmdResult<Status> {
    if cert().is_file() { return Ok(status(&core)); }
    let bin = binary(&core).ok_or("先安装 cloudflared")?;
    let mut child = hub::quiet_cmd(&bin).args(["tunnel", "login"]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped())
        .spawn().map_err(|e| format!("运行 cloudflared 失败：{e}"))?;
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    for out in [child.stdout.take().map(|o| Box::new(o) as Box<dyn std::io::Read + Send>), child.stderr.take().map(|e| Box::new(e) as Box<dyn std::io::Read + Send>)].into_iter().flatten() {
        let tx = tx.clone();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(out).lines().map_while(Result::ok) {
                if let Some(i) = line.find("https://") { let _ = tx.send(line[i..].trim().to_owned()); }
            }
        });
    }
    // The login process waits for the browser (up to a few minutes) and exits by itself.
    std::thread::spawn(move || { let _ = child.wait(); });
    let url = tauri::async_runtime::spawn_blocking(move || rx.recv_timeout(std::time::Duration::from_secs(15)).ok()).await.ok().flatten();
    proc().login_url = url;
    Ok(status(&core))
}

/// Create this install's named tunnel (once), point `hostname` at it (replacing an old
/// DNS record of that name), switch invites to it and start it.
#[tauri::command]
pub async fn tunnel_setup(core: St<'_>, hostname: String) -> CmdResult<Status> {
    let host = clean_host(&hostname)?;
    let bin = binary(&core).ok_or("先安装 cloudflared")?;
    if !cert().is_file() { return Err("先登录 Cloudflare".into()); }
    let mut c = conf(&core).unwrap_or_default();
    let core2 = core.inner().clone();
    if c.id.is_empty() || !cred_file(&core, &c).is_file() {
        let name = format!("disco-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
        let dir = appdata(&core).join("cloudflared");
        std::fs::create_dir_all(&dir).map_err(err)?;
        let tmp = dir.join(format!("{name}.json"));
        let (b, t, n) = (bin.clone(), tmp.clone(), name.clone());
        let out = tauri::async_runtime::spawn_blocking(move || run(&b, &["tunnel", "create", "-o", "json", "--credentials-file", &t.to_string_lossy(), &n]))
            .await.map_err(err)?.map_err(|e| format!("创建隧道失败：{e}"))?;
        let id = tunnel_id(&out).ok_or("创建隧道失败：没读到隧道 ID")?;
        c = Conf { name, id, hostname: String::new() };
        std::fs::rename(&tmp, cred_file(&core, &c)).map_err(err)?;
    }
    let (b, id, h) = (bin.clone(), c.id.clone(), host.clone());
    tauri::async_runtime::spawn_blocking(move || run(&b, &["tunnel", "route", "dns", "--overwrite-dns", &id, &h]))
        .await.map_err(err)?.map_err(|e| format!("绑定域名失败（域名要在你登录时选的 Cloudflare 账号里）：{e}"))?;
    c.hostname = host;
    let json = serde_json::to_string(&c).map_err(err)?;
    core.db().set_setting(CONF, &json).map_err(err)?;
    core.db().set_setting(VIA, "cloudflare").map_err(err)?;
    start(&core2)?;
    Ok(status(&core))
}

/// Switch how invites travel (`via`: "cloudflare" starts the tunnel, "tailscale" stops it),
/// and/or save the Cloudflare Access service token (both empty = remove it).
#[tauri::command]
pub fn tunnel_set(core: St<'_>, via: Option<String>, access_id: Option<String>, access_secret: Option<String>) -> CmdResult<Status> {
    if let Some(v) = via {
        if v == "cloudflare" {
            if conf(&core).is_none_or(|c| c.id.is_empty()) { return Err("先登录 Cloudflare 并设置域名".into()); }
            core.db().set_setting(VIA, "cloudflare").map_err(err)?;
            start(core.inner())?;
        } else {
            core.db().set_setting(VIA, "").map_err(err)?;
            stop();
        }
    }
    if let (Some(id), Some(sec)) = (access_id, access_secret) {
        let (id, sec) = (id.trim().to_owned(), sec.trim().to_owned());
        if id.is_empty() && sec.is_empty() {
            secret_del(ACCESS_KEY);
            core.db().set_setting(ACCESS_ON, "").map_err(err)?;
        } else {
            if id.is_empty() || sec.is_empty() || id.contains('\n') || sec.contains('\n') { return Err("Client ID 和 Client Secret 都要填".into()); }
            secret_set(ACCESS_KEY, &format!("{id}\n{sec}"))?;
            core.db().set_setting(ACCESS_ON, "1").map_err(err)?;
        }
    }
    Ok(status(&core))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_and_ids() {
        assert_eq!(clean_host(" https://Disco.Example.com/ ").unwrap(), "disco.example.com");
        for bad in ["", "localhost", "a..b.com", ".a.com", "a b.com", "a.com/x"] { assert!(clean_host(bad).is_err(), "{bad}"); }
        let id = "6ff42ae2-765d-4adf-8112-31c55c1551ef";
        assert_eq!(tunnel_id(&format!("Tunnel credentials written to x.json\n{{\"id\":\"{id}\",\"name\":\"disco-1\"}}")).as_deref(), Some(id));
        assert_eq!(tunnel_id(&format!("Created tunnel disco-1 with id {id}")).as_deref(), Some(id));
        assert_eq!(tunnel_id("nothing"), None);
        assert_eq!(last_error("2026-10-30T01:02:03Z INF hi\n2026-10-30T01:02:04Z ERR Unauthorized: bad cert\n").as_deref(), Some("Unauthorized: bad cert"));
    }

    #[test]
    fn config_routes_only_the_hostname() {
        let c = Conf { name: "disco-1".into(), id: "abc".into(), hostname: "d.example.com".into() };
        let y = config_yaml(&c, std::path::Path::new(r"C:\Users\o'k\cf.json"), 7490);
        assert!(y.contains("tunnel: abc\n") && y.contains(r"credentials-file: 'C:\Users\o''k\cf.json'"), "{y}");
        assert!(y.contains("  - hostname: d.example.com\n    service: http://127.0.0.1:7490\n  - service: http_status:404\n"), "{y}");
    }
}
