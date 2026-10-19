//! Files handed to a chat: copied into the thread's shared space with their
//! SHA-256, so every agent in the group (and the user, via Finder) can read them.
use std::path::{Path, PathBuf};
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

/// Copy `src` into `dir` under `name` (or `2-name`, `3-name`, … if taken, claimed
/// atomically), hashing as it copies. Over MAX_SEND bytes is refused and the
/// partial copy removed, whatever the file grew to since it was checked.
fn copy_in(src: &Path, dir: &Path, name: &str) -> Result<(PathBuf, u64, String), String> {
    use std::io::{Read, Write};
    // One byte past the cap is enough to tell "too big" without reading the rest.
    let mut from = std::fs::File::open(src).map_err(|e| format!("打不开文件 {}：{e}", src.display()))?.take(MAX_SEND + 1);
    std::fs::create_dir_all(dir).map_err(crate::err)?;
    let (mut dst, mut i) = (dir.join(name), 2);
    let mut to = loop {
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&dst) {
            Ok(f) => break f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => { dst = dir.join(format!("{i}-{name}")); i += 1; }
            Err(e) => return Err(crate::err(e)),
        }
    };
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
        // Copying and hashing up to 100 MB is blocking work: keep it off the async threads.
        let (dst, size, hash) = {
            let name = name.clone();
            tokio::task::spawn_blocking(move || copy_in(&p, &dir, &name)).await.map_err(crate::err)??
        };
        let note = note.trim();
        let line = format!("{}📎 {name}（{} · SHA-256 {}）\n{}", if note.is_empty() { String::new() } else { format!("{note}\n") }, human_size(size), &hash[..12], dst.display());
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
