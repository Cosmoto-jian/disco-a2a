//! The current git branch of a folder, read straight from `.git/HEAD` (no git process)
//! and cached for a few seconds, so the overview can ask for it on every redraw.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

const TTL: Duration = Duration::from_secs(10);
/// Folder -> (read at, branch).
type Cache = HashMap<PathBuf, (Instant, Option<String>)>;
static CACHE: LazyLock<Mutex<Cache>> = LazyLock::new(Default::default);

/// Branch name, or the short commit when HEAD is detached; None outside a repo.
pub fn branch(dir: &Path) -> Option<String> {
    let hit = CACHE.lock().unwrap_or_else(|p| p.into_inner()).get(dir).filter(|(t, _)| t.elapsed() < TTL).map(|(_, b)| b.clone());
    if let Some(b) = hit { return b; }
    let b = head_file(dir).and_then(|f| std::fs::read_to_string(f).ok()).and_then(|s| parse_head(&s));
    CACHE.lock().unwrap_or_else(|p| p.into_inner()).insert(dir.to_owned(), (Instant::now(), b.clone()));
    b
}

/// HEAD of the repo `dir` is in. A worktree (or submodule) has a `.git` file pointing
/// at its real git dir: `gitdir: <path>`, relative to the folder holding the file.
fn head_file(dir: &Path) -> Option<PathBuf> {
    for d in dir.ancestors() {
        let git = d.join(".git");
        if git.is_dir() { return Some(git.join("HEAD")); }
        if git.is_file() {
            let s = std::fs::read_to_string(&git).ok()?;
            let to = s.lines().find_map(|l| l.strip_prefix("gitdir:"))?.trim();
            return Some(d.join(to).join("HEAD"));
        }
    }
    None
}

/// `ref: refs/heads/main` → `main`; a detached commit id → its first 7 characters.
pub(crate) fn parse_head(s: &str) -> Option<String> {
    let s = s.trim();
    if let Some(r) = s.strip_prefix("ref:") {
        let r = r.trim();
        let name = r.strip_prefix("refs/heads/").unwrap_or(r);
        return (!name.is_empty()).then(|| name.to_owned());
    }
    (s.len() >= 7 && s.bytes().all(|b| b.is_ascii_hexdigit())).then(|| s[..7].to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_contents() {
        assert_eq!(parse_head("ref: refs/heads/main\n").as_deref(), Some("main"));
        assert_eq!(parse_head("ref: refs/heads/disco/claude").as_deref(), Some("disco/claude"));
        assert_eq!(parse_head("3f2a9c1d0e4b5a6978877665544332211aabbccd\n").as_deref(), Some("3f2a9c1"));
        assert_eq!(parse_head(""), None);
        assert_eq!(parse_head("ref: "), None);
        assert_eq!(parse_head("not a head"), None);
    }

    #[test]
    fn repo_and_worktree() {
        let root = std::env::temp_dir().join(format!("disco-git-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (repo, wt) = (root.join("repo"), root.join("wt"));
        std::fs::create_dir_all(repo.join(".git/worktrees/wt")).unwrap();
        std::fs::create_dir_all(repo.join("src/deep")).unwrap();
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(repo.join(".git/worktrees/wt/HEAD"), "0123456789abcdef0123456789abcdef01234567\n").unwrap();
        std::fs::write(wt.join(".git"), "gitdir: ../repo/.git/worktrees/wt\n").unwrap();
        assert_eq!(branch(&repo.join("src/deep")).as_deref(), Some("main"));
        assert_eq!(branch(&wt).as_deref(), Some("0123456"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
