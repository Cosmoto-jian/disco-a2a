//! Layered, file-based long-term memory.
//!
//! `<app data>/memory/<scope>/` holds one markdown file per topic (`- fact` lines)
//! plus a generated `MEMORY.md` index. Scopes layer from wide to narrow:
//! `all` (every agent), `group-<id>` (one group) and `<agent>` (one agent).
//! A new session gets the full notes of its layers while they fit the budget,
//! otherwise only the indexes, and reads topics on demand (`memory_read`).
//! Plain files, so the user can open, edit or sync them like any notes.
use std::path::PathBuf;

use serde::Serialize;
use tauri::Manager;

use crate::Core;

const INDEX: &str = "MEMORY.md";
/// Characters of notes a new session is shown before falling back to indexes.
const BUDGET: usize = 4000;
pub const DEFAULT_TOPIC: &str = "general";
/// `memory_off/<thread>` = "1": that chat neither reads nor writes memory.
pub const MEM_OFF: &str = "memory_off/";

#[derive(Serialize)]
pub struct Note { pub key: String, pub scope: String, pub topic: String, pub text: String }

/// File-system-safe name: letters (any script), digits, `-`, `_`; at most 40 chars.
pub fn slug(s: &str) -> String {
    let out: String = s.trim().chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect::<String>().trim_matches('-').chars().take(40).collect();
    if out.is_empty() { DEFAULT_TOPIC.into() } else { out }
}

fn items(doc: &str) -> impl Iterator<Item = &str> { doc.lines().filter_map(|l| l.strip_prefix("- ")) }

fn topics(dir: &std::path::Path) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = std::fs::read_dir(dir).into_iter().flatten().flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let topic = name.strip_suffix(".md").filter(|t| *t != "MEMORY")?.to_owned();
            Some((topic, std::fs::read_to_string(e.path()).ok()?))
        }).collect();
    v.sort();
    v
}

/// Regenerate `MEMORY.md`: one line per topic with its size and newest note.
fn reindex(dir: &std::path::Path) {
    let mut out = String::from("# 记忆索引\n\n");
    for (topic, doc) in topics(dir) {
        let n = items(&doc).count();
        if n == 0 { continue; }
        let last: String = items(&doc).last().unwrap_or("").chars().take(50).collect();
        out.push_str(&format!("- [{topic}]({topic}.md) · {n} 条 · 最近：{last}\n"));
    }
    let _ = crate::mcp::write_doc(&dir.join(INDEX), &out);
}

impl Core {
    fn memory_dir(&self, scope: &str) -> PathBuf {
        let base = self.app.path().app_data_dir().unwrap_or_else(|_| std::env::temp_dir());
        base.join("memory").join(slug(scope))
    }

    /// Save a note under `topic` (default `general`); exact duplicates are skipped.
    pub(crate) fn remember_topic(&self, scope: &str, topic: Option<&str>, text: &str) {
        let text = text.trim().replace('\n', " ");
        if text.is_empty() { return; }
        let dir = self.memory_dir(scope);
        let topic = slug(topic.unwrap_or(DEFAULT_TOPIC));
        let path = dir.join(format!("{topic}.md"));
        let mut doc = std::fs::read_to_string(&path).unwrap_or_else(|_| format!("# {topic}\n\n"));
        if items(&doc).any(|l| l == text) { return; }
        if !doc.ends_with('\n') { doc.push('\n'); }
        doc.push_str(&format!("- {text}\n"));
        if crate::mcp::write_doc(&path, &doc).is_ok() { reindex(&dir); }
    }

    pub(crate) fn remember(&self, scope: &str, text: &str) { self.remember_topic(scope, None, text) }

    /// One topic's file (or the scope's index when `topic` is None).
    pub(crate) fn memory_read(&self, scope: &str, topic: Option<&str>) -> String {
        let dir = self.memory_dir(scope);
        let file = topic.map_or(INDEX.to_owned(), |t| format!("{}.md", slug(t)));
        std::fs::read_to_string(dir.join(file)).unwrap_or_default()
    }

    /// Whether the chat `context` belongs to has memory switched on (the default).
    pub(crate) fn memory_on(&self, context: &str) -> bool {
        thread_of(context).is_none_or(|t| self.db().setting(&format!("{MEM_OFF}{t}")).as_deref() != Some("1"))
    }

    /// What a new session is told first. `context` locates it (a group's sessions
    /// also get that group's layer).
    pub(crate) fn memory_preamble(&self, agent: &str, context: &str) -> String {
        let mut layers = vec![("all", "所有 agent 共享".to_owned()), (agent, "你自己的".to_owned())];
        let group = group_of(context);
        if let Some(g) = &group { layers.push((g.as_str(), "本群的".to_owned())); }
        let docs: Vec<(String, Vec<(String, String)>)> = layers.iter().map(|(s, label)| (label.clone(), topics(&self.memory_dir(s)))).collect();
        let total: usize = docs.iter().flat_map(|(_, t)| t.iter().map(|(_, d)| d.len())).sum();
        let mut out = String::from("【Disco 长期记忆】\n");
        for ((label, ts), (scope, _)) in docs.iter().zip(&layers) {
            let notes: Vec<(&String, Vec<&str>)> = ts.iter().map(|(t, d)| (t, items(d).collect::<Vec<_>>())).filter(|(_, v)| !v.is_empty()).collect();
            if notes.is_empty() { continue; }
            out.push_str(&format!("## {label}（scope={}）\n", if *scope == agent { "self" } else if *scope == "all" { "all" } else { "group" }));
            for (topic, v) in notes {
                if total <= BUDGET {
                    out.push_str(&format!("### {topic}\n"));
                    for l in v { out.push_str(&format!("- {l}\n")); }
                } else {
                    out.push_str(&format!("- {topic} · {} 条 · 最近：{}\n", v.len(), v.last().map(|l| l.chars().take(50).collect::<String>()).unwrap_or_default()));
                }
            }
        }
        if total > BUDGET { out.push_str("（记忆较多，以上只是索引；需要细节时用 memory_read 读对应主题。）\n"); }
        out.push_str(&format!("（需要长期记住的事实或偏好，单独一行以「{}」开头写出，Disco 会保存。）\n", crate::MEM_TAG));
        if self.hub.mcp_port.get().is_some() && self.agent(agent).is_ok_and(|r| r.transport == "acp") {
            out.push_str("（你有 disco 工具：overview 看自己在哪，post 发到群或通知用户，ask 请教其他 agent，ask_user 向用户提问（选择或确认），todo_add/todo_list/todo_done 管待办，doc_read/doc_write 读写群文档，remember 按主题保存长期记忆，memory_read 读记忆。）\n");
        }
        out.push('\n');
        out
    }

    /// Every note, for the settings page. `key` = `scope/topic/text`.
    pub(crate) fn memory_list(&self) -> Vec<Note> {
        let base = self.memory_dir("x");
        let base = base.parent().map(PathBuf::from).unwrap_or_default();
        let mut out = Vec::new();
        for e in std::fs::read_dir(&base).into_iter().flatten().flatten() {
            let scope = e.file_name().to_string_lossy().into_owned();
            for (topic, doc) in topics(&e.path()) {
                for l in items(&doc) {
                    out.push(Note { key: format!("{scope}/{topic}/{l}"), scope: scope.clone(), topic: topic.clone(), text: l.to_owned() });
                }
            }
        }
        out
    }

    pub(crate) fn memory_forget(&self, key: &str) -> Result<(), String> {
        let mut parts = key.splitn(3, '/');
        let (Some(scope), Some(topic), Some(text)) = (parts.next(), parts.next(), parts.next()) else { return Err("bad key".into()) };
        let dir = self.memory_dir(scope);
        let path = dir.join(format!("{}.md", slug(topic)));
        let doc = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
        let kept: String = doc.lines().filter(|l| l.strip_prefix("- ") != Some(text)).map(|l| format!("{l}\n")).collect();
        if items(&kept).count() == 0 { let _ = std::fs::remove_file(&path); } else { crate::mcp::write_doc(&path, &kept)?; }
        reindex(&dir);
        Ok(())
    }

    pub(crate) fn memory_rename(&self, old: &str, new: &str) {
        let (a, b) = (self.memory_dir(old), self.memory_dir(new));
        if a.exists() && !b.exists() { let _ = std::fs::rename(a, b); }
    }

    /// One-time move of v0.7 notes (settings `mem/<scope>/<ts>`) into files.
    pub(crate) fn memory_migrate(&self) {
        let old = self.db().settings_with_prefix(crate::MEM).unwrap_or_default();
        for (k, text) in old {
            if !text.is_empty() { self.remember(k.split('/').next().unwrap_or("all"), &text); }
            let _ = self.db().set_setting(&format!("{}{k}", crate::MEM), "");
        }
    }
}

/// The chat a session context belongs to (`group:<id>` or `dm:<agent>`), if any.
pub fn thread_of(context: &str) -> Option<String> {
    match group_of(context) {
        Some(g) => Some(format!("group:{}", &g["group-".len()..])),
        None => context.starts_with("dm:").then(|| context.to_owned()),
    }
}

/// `group-<id>` for contexts inside a group (`group:<id>`, `team:group:<id>:…`).
pub fn group_of(context: &str) -> Option<String> {
    let rest = context.strip_prefix("team:").unwrap_or(context).strip_prefix("group:")?;
    Some(format!("group-{}", rest.split(':').next().unwrap_or(rest)))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn slugs_and_groups() {
        assert_eq!(slug("代码 风格/偏好"), "代码-风格-偏好");
        assert_eq!(slug("  "), DEFAULT_TOPIC);
        assert_eq!(group_of("group:123").as_deref(), Some("group-123"));
        assert_eq!(group_of("team:group:local-x:claude").as_deref(), Some("group-local-x"));
        assert_eq!(group_of("dm:claude"), None);
        assert_eq!(thread_of("team:group:local-x:claude").as_deref(), Some("group:local-x"));
        assert_eq!(thread_of("dm:claude").as_deref(), Some("dm:claude"));
        assert_eq!(thread_of("ask:coder"), None);
    }
}
