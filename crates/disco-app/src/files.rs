//! Files handed to a chat: copied into the thread's shared space with their
//! SHA-256, so every agent in the group (and the user, via Finder) can read them.
//! A file for an agent on another device (an invite-code share) also rides along
//! with the next A2A message to it, as an inline file part (at most `MAX_A2A`).
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use disco_types::FilePart;
use sha2::{Digest, Sha256};
use tauri::Manager;

use crate::Core;

pub const MAX_SEND: u64 = 100 << 20;
/// Largest file (and largest total per message) sent to or from another device.
pub const MAX_A2A: u64 = 20 << 20;

/// A file waiting to go to remote agent `agent` with its next message in `thread`.
#[derive(Clone)]
pub(crate) struct Pending { agent: String, thread: String, path: PathBuf, name: String, size: u64, sha256: String }

static PENDING: Mutex<Vec<Pending>> = Mutex::new(Vec::new());

/// Take `agent`'s files waiting in `thread`, oldest first, up to `MAX_A2A` in total
/// (the rest wait for the message after).
pub(crate) fn take_pending(agent: &str, thread: &str) -> Vec<Pending> {
    let mut all = PENDING.lock().unwrap_or_else(|p| p.into_inner());
    let (mut out, mut total) = (Vec::new(), 0);
    all.retain(|p| {
        let take = p.agent == agent && p.thread == thread && (out.is_empty() || total + p.size <= MAX_A2A);
        if take { total += p.size; out.push(p.clone()); }
        !take
    });
    out
}

/// Put files back that did not get through (in front, so order is kept).
pub(crate) fn requeue(mut files: Vec<Pending>) {
    let mut all = PENDING.lock().unwrap_or_else(|p| p.into_inner());
    files.append(&mut all);
    *all = files;
}

/// The A2A parts for waiting files (a file that vanished from disk is skipped).
pub(crate) async fn load(files: &[Pending]) -> Vec<FilePart> {
    let mut out = Vec::new();
    for p in files {
        let Ok(bytes) = tokio::fs::read(&p.path).await else { continue };
        out.push(FilePart { name: p.name.clone(), mime: None, size: bytes.len() as u64, sha256: p.sha256.clone(), url: None, bytes });
    }
    out
}

/// The chat line for a file: note, name, size, short hash, then the path on its own line.
pub(crate) fn file_line(note: &str, name: &str, size: u64, hash: &str, path: &Path) -> String {
    let note = note.trim();
    format!("{}📎 {name}（{} · SHA-256 {}）\n{}", if note.is_empty() { String::new() } else { format!("{note}\n") }, human_size(size), &hash[..hash.len().min(12)], path.display())
}

/// Create `dir/name` (or `2-name`, `3-name`, … if taken), atomically.
fn claim(dir: &Path, name: &str) -> Result<(std::fs::File, PathBuf), String> {
    std::fs::create_dir_all(dir).map_err(crate::err)?;
    let (mut dst, mut i) = (dir.join(name), 2);
    loop {
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&dst) {
            Ok(f) => return Ok((f, dst)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => { dst = dir.join(format!("{i}-{name}")); i += 1; }
            Err(e) => return Err(crate::err(e)),
        }
    }
}

/// Save a file that came from another device into `dir`: at most `MAX_A2A`, the size and
/// SHA-256 it was sent with must match, and only its safe base name is used.
pub(crate) fn save_incoming(dir: &Path, f: &FilePart) -> Result<(PathBuf, u64, String), String> {
    use std::io::Write;
    let name = safe_name(&f.name);
    let n = f.bytes.len() as u64;
    if n > MAX_A2A { return Err(format!("文件 {name} 超过 {}，没有收下", human_size(MAX_A2A))); }
    if n != f.size { return Err(format!("文件 {name} 不完整（收到 {}，应为 {}），没有保存", human_size(n), human_size(f.size))); }
    let hash = hex::encode(Sha256::digest(&f.bytes));
    if !f.sha256.is_empty() && !f.sha256.eq_ignore_ascii_case(&hash) { return Err(format!("文件 {name} 的 SHA-256 校验不通过，没有保存")); }
    let (mut to, dst) = claim(dir, &name)?;
    if let Err(e) = to.write_all(&f.bytes).and_then(|()| to.flush()) { drop(to); let _ = std::fs::remove_file(&dst); return Err(crate::err(e)); }
    Ok((dst, n, hash))
}

pub fn safe_name(name: &str) -> String {
    let base = name.replace('\\', "/");
    let base = base.rsplit('/').next().unwrap_or("");
    let s: String = base.chars().map(|c| if c.is_control() || "<>:\"/\\|?*".contains(c) { '_' } else { c }).collect();
    let s = s.trim().trim_start_matches('.').chars().take(200).collect::<String>();
    if s.is_empty() { "file".into() } else { s }
}

/// Copy `src` into `dir` under `name` (or `2-name`, `3-name`, … if taken, claimed
/// atomically), hashing as it copies. Over MAX_SEND bytes is refused and the
/// partial copy removed, whatever the file grew to since it was checked.
fn copy_in(src: &Path, dir: &Path, name: &str) -> Result<(PathBuf, u64, String), String> {
    use std::io::{Read, Write};
    // One byte past the cap is enough to tell "too big" without reading the rest.
    let mut from = std::fs::File::open(src).map_err(|e| format!("打不开文件 {}：{e}", src.display()))?.take(MAX_SEND + 1);
    let (mut to, dst) = claim(dir, name)?;
    let copied = (|| {
        let (mut h, mut buf, mut n) = (Sha256::new(), vec![0u8; 1 << 20], 0u64);
        loop {
            let k = from.read(&mut buf)?;
            if k == 0 { break; }
            n += k as u64;
            if n > MAX_SEND { return Ok(None); }
            h.update(&buf[..k]);
            to.write_all(&buf[..k])?;
        }
        to.flush()?;
        Ok::<_, std::io::Error>(Some((n, hex::encode(h.finalize()))))
    })();
    drop(to);
    match copied {
        Ok(Some((n, hash))) => Ok((dst, n, hash)),
        Ok(None) => { let _ = std::fs::remove_file(&dst); Err(format!("文件超过 {}", human_size(MAX_SEND))) }
        Err(e) => { let _ = std::fs::remove_file(&dst); Err(crate::err(e)) }
    }
}

pub fn human_size(n: u64) -> String {
    match n {
        n if n >= 1 << 20 => format!("{:.1} MB", n as f64 / (1u64 << 20) as f64),
        n if n >= 1 << 10 => format!("{:.0} KB", n as f64 / 1024.0),
        n => format!("{n} B"),
    }
}

/// An absolute file path (`~/` allowed) and its size, or why it can't be sent.
pub(crate) fn sendable(path: &str, max: u64) -> Result<(PathBuf, u64), String> {
    let p = match path.strip_prefix("~/") { Some(r) => crate::hub::dirs_home().join(r), None => path.into() };
    if !p.is_absolute() { return Err("请给出文件的绝对路径".into()); }
    let meta = std::fs::metadata(&p).map_err(|e| format!("找不到文件 {}：{e}", p.display()))?;
    if !meta.is_file() { return Err("只能发送文件，不能发送文件夹".into()); }
    if meta.len() > max { return Err(format!("文件超过 {}{}", human_size(max), if max == MAX_A2A { "，不能发到其他设备" } else { "" })); }
    Ok((p, meta.len()))
}

impl Core {
    /// Agents in `thread` that live on another device (A2A), who get its files over A2A.
    pub(crate) fn remote_members(&self, thread: &str) -> Vec<String> {
        let rows = self.db().list_agents().unwrap_or_default();
        let members: Vec<String> = match thread.split_once(':') {
            Some(("dm", a)) => vec![a.to_owned()],
            Some(("group", g)) => {
                let groups = self.db().list_groups().unwrap_or_default();
                groups.into_iter().find(|x| x.id == g).map(|x| x.members).unwrap_or_default()
            }
            _ => Vec::new(),
        };
        rows.into_iter().filter(|r| r.transport == "a2a" && members.contains(&r.name)).map(|r| r.name).collect()
    }

    /// A file a remote agent sent back: verify it, save it in the thread's space, show it.
    pub(crate) fn receive_file(&self, thread: &str, agent: &str, f: &FilePart) {
        let dir = crate::mcp::space_dir(self, thread).map(|d| d.join("files"));
        match dir.and_then(|d| save_incoming(&d, f)) {
            Ok((dst, size, hash)) => self.post_message(thread, agent, &file_line("", &safe_name(&f.name), size, &hash, &dst)),
            Err(e) => self.post_message(thread, "system", &format!("{agent} 发来的{e}")),
        }
    }

    /// Copy `path` into the thread's shared space and post it there. An agent's
    /// post in a group calls whoever it @s, as with `post`. Agents in the thread that
    /// live on another device get it with their next message.
    pub(crate) async fn share_file(&self, thread: &str, author: &str, path: &str, note: &str) -> Result<String, String> {
        if thread.starts_with("peer:") && author == "me" { return Err("这是朋友和你的 agent 的会话，你在旁观，不能在这里发文件".into()); }
        let remote = self.remote_members(thread);
        let (p, _) = sendable(path, if remote.is_empty() { MAX_SEND } else { MAX_A2A })?;
        let name = safe_name(&p.to_string_lossy());
        let dir = crate::mcp::space_dir(self, thread)?.join("files");
        // Copying and hashing up to 100 MB is blocking work: keep it off the async threads.
        let (dst, size, hash) = {
            let name = name.clone();
            tokio::task::spawn_blocking(move || copy_in(&p, &dir, &name)).await.map_err(crate::err)??
        };
        let line = file_line(note, &name, size, &hash, &dst);
        {
            let mut q = PENDING.lock().unwrap_or_else(|p| p.into_inner());
            q.extend(remote.iter().map(|a| Pending { agent: a.clone(), thread: thread.to_owned(), path: dst.clone(), name: name.clone(), size, sha256: hash.clone() }));
        }
        match thread.strip_prefix("group:") {
            Some(g) if author != "me" => {
                let (core, g, a) = (self.app.state::<Arc<Core>>().inner().clone(), g.to_owned(), author.to_owned());
                tauri::async_runtime::spawn(async move { core.group(g, a, line, false).await });
            }
            _ => self.post_message(thread, author, &line),
        }
        let also = if remote.is_empty() { String::new() } else { format!("；会随下一条消息发给其他设备上的 {}", remote.join("、")) };
        Ok(format!("已放进共享空间：{}{also}", dst.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_sizes() {
        assert_eq!(safe_name("/a/b/../.secret"), "secret");
        assert_eq!(safe_name("x:y?.txt"), "x_y_.txt");
        assert_eq!(safe_name(""), "file");
        assert_eq!(human_size(2048), "2 KB");
    }

    #[test]
    fn copy_in_hashes_and_never_overwrites() {
        let root = std::env::temp_dir().join(format!("disco-copy-{}", std::process::id()));
        let (src, dir) = (root.join("a.txt"), root.join("files"));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&src, b"hello").unwrap();
        let (d1, n, h) = copy_in(&src, &dir, "a.txt").unwrap();
        let (d2, ..) = copy_in(&src, &dir, "a.txt").unwrap();
        assert_eq!((n, d1.file_name().unwrap().to_str(), d2.file_name().unwrap().to_str()), (5, Some("a.txt"), Some("2-a.txt")));
        assert_eq!(h, "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824");
        let _ = std::fs::remove_dir_all(&root);
    }
}
