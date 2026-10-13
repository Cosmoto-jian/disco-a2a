//! Files handed to a chat: copied into the thread's shared space with their
//! SHA-256, so every agent in the group (and the user, via Finder) can read them.
use std::path::Path;
use std::sync::Arc;

use sha2::{Digest, Sha256};
use tauri::Manager;

use crate::Core;

pub const MAX_SEND: u64 = 100 << 20;

pub fn safe_name(name: &str) -> String {
    let base = name.replace('\\', "/");
    let base = base.rsplit('/').next().unwrap_or("");
    let s: String = base.chars().map(|c| if c.is_control() || "<>:\"/\\|?*".contains(c) { '_' } else { c }).collect();
    let s = s.trim().trim_start_matches('.').chars().take(200).collect::<String>();
    if s.is_empty() { "file".into() } else { s }
}

pub fn sha256_file(p: &Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(p)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 { break; }
        h.update(&buf[..n]);
    }
    Ok(hex::encode(h.finalize()))
}

pub fn human_size(n: u64) -> String {
    match n {
        n if n >= 1 << 20 => format!("{:.1} MB", n as f64 / (1u64 << 20) as f64),
        n if n >= 1 << 10 => format!("{:.0} KB", n as f64 / 1024.0),
        n => format!("{n} B"),
    }
}

impl Core {
    /// Copy `path` into the thread's shared space and post it there. An agent's
    /// post in a group calls whoever it @s, as with `post`.
    pub(crate) async fn share_file(&self, thread: &str, author: &str, path: &str, note: &str) -> Result<String, String> {
        let p = match path.strip_prefix("~/") { Some(r) => crate::hub::dirs_home().join(r), None => path.into() };
        if !p.is_absolute() { return Err("请给出文件的绝对路径".into()); }
        let meta = std::fs::metadata(&p).map_err(|e| format!("找不到文件 {}：{e}", p.display()))?;
        if !meta.is_file() { return Err("只能发送文件，不能发送文件夹".into()); }
        if meta.len() > MAX_SEND { return Err(format!("文件超过 {}", human_size(MAX_SEND))); }
        let name = safe_name(&p.to_string_lossy());
        let dir = crate::mcp::doc_path(self, thread).with_file_name("files");
        std::fs::create_dir_all(&dir).map_err(crate::err)?;
        let mut dst = dir.join(&name);
        let mut i = 2;
        while dst.exists() { dst = dir.join(format!("{i}-{name}")); i += 1; }
        std::fs::copy(&p, &dst).map_err(crate::err)?;
        let hash = sha256_file(&dst).map_err(crate::err)?;
        let note = note.trim();
        let line = format!("{}📎 {name}（{} · SHA-256 {}）\n{}", if note.is_empty() { String::new() } else { format!("{note}\n") }, human_size(meta.len()), &hash[..12], dst.display());
        match thread.strip_prefix("group:") {
            Some(g) if author != "me" => {
                let (core, g, a) = (self.app.state::<Arc<Core>>().inner().clone(), g.to_owned(), author.to_owned());
                tauri::async_runtime::spawn(async move { core.group(g, a, line, false).await });
            }
            _ => self.post_message(thread, author, &line),
        }
        Ok(format!("已放进共享空间：{}", dst.display()))
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
}
