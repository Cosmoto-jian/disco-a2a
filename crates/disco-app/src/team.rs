//! Team mode for groups (`/team <task>`): a blackboard behind the scenes.
//!
//! The lead splits the task into roles, members work on them in parallel in
//! their own `team:` sessions (no chat bubbles, just an activity strip), every
//! result lands as a file on the blackboard, and the lead posts one summary.
//!
//! `<app data>/spaces/<thread>/team/`
//!   context.md          the task, the members, recent chat
//!   findings/<role>.md  each member's result
//!   summary.md          the lead's conclusion (also posted to the group)
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use disco_store::GroupRow;
use serde::{Deserialize, Serialize};
use tauri::Emitter;

use crate::{Core, mcp, secret_get};

/// Characters of one finding the lead sees when summarizing.
const FINDING_CAP: usize = 8000;

#[derive(Serialize, Clone)]
pub struct Role { pub agent: String, pub role: String, pub task: String, pub state: &'static str, pub last: String }

#[derive(Serialize, Clone)]
pub struct State {
    pub thread: String,
    pub task: String,
    pub lead: String,
    /// plan · work · summary · done · failed
    pub stage: &'static str,
    pub roles: Vec<Role>,
    pub dir: String,
}

#[derive(Default)]
pub struct Teams(Mutex<HashMap<String, State>>);

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> { m.lock().unwrap_or_else(|p| p.into_inner()) }

/// `/team …` or `/协作 …` → the task.
pub fn command(text: &str) -> Option<&str> {
    let t = text.trim();
    ["/team ", "/协作 ", "/团队 "].iter().find_map(|p| t.strip_prefix(p)).map(str::trim).filter(|t| !t.is_empty())
}

#[derive(Deserialize)]
struct Assign { agent: String, #[serde(default)] role: String, #[serde(default)] task: String }

/// Every balanced JSON array or object in `text` that parses, in order (prose, ``` fences
/// and brackets inside strings around it are fine).
fn json_values(text: &str) -> impl Iterator<Item = serde_json::Value> + '_ {
    text.char_indices().filter(|(_, c)| matches!(c, '[' | '{')).filter_map(|(i, _)| {
        let (mut depth, mut in_str, mut esc) = (0usize, false, false);
        for (j, c) in text[i..].char_indices() {
            match c {
                _ if esc => esc = false,
                '\\' if in_str => esc = true,
                '"' => in_str = !in_str,
                '[' | '{' if !in_str => depth += 1,
                ']' | '}' if !in_str => {
                    depth -= 1;
                    if depth == 0 { return serde_json::from_str(&text[i..=i + j]).ok(); }
                }
                _ => {}
            }
        }
        None
    })
}

/// The assignments in the lead's JSON: an array, `{"roles": [...]}`-style, or one object.
fn assigns(v: serde_json::Value) -> Vec<Assign> {
    match v {
        serde_json::Value::Array(a) => a.into_iter().filter_map(|x| serde_json::from_value(x).ok()).collect(),
        serde_json::Value::Object(o) if !o.contains_key("agent") => o.into_iter().find_map(|(_, x)| x.is_array().then(|| assigns(x))).unwrap_or_default(),
        o => serde_json::from_value(o).into_iter().collect(),
    }
}

/// The lead's plan: the first JSON in its answer that assigns anyone, members only (once each), unique roles.
fn parse_plan(answer: &str, members: &[String], task: &str) -> Vec<(String, String, String)> {
    let parsed = json_values(answer).map(assigns).find(|a| !a.is_empty()).unwrap_or_default();
    let mut out: Vec<(String, String, String)> = Vec::new();
    for a in parsed {
        let Some(m) = members.iter().find(|m| m.eq_ignore_ascii_case(a.agent.trim().trim_start_matches('@'))) else { continue };
        if out.iter().any(|(o, _, _)| o == m) { continue; }
        let mut role = crate::memory::slug(if a.role.trim().is_empty() { m } else { &a.role });
        while out.iter().any(|(_, r, _)| *r == role) { role.push('2'); }
        out.push((m.clone(), role, if a.task.trim().is_empty() { task.to_owned() } else { a.task.trim().to_owned() }));
    }
    if out.is_empty() {
        out = members.iter().map(|m| (m.clone(), crate::memory::slug(m), task.to_owned())).collect();
    }
    out.truncate(8);
    out
}

impl Core {
    fn team_dir(&self, thread: &str) -> Result<PathBuf, String> {
        Ok(mcp::space_dir(self, thread)?.join("team"))
    }

    fn team_emit(&self, thread: &str) {
        if let Some(s) = lock(&self.teams.0).get(thread) { let _ = self.app.emit("team", s.clone()); }
    }

    fn team_update(&self, thread: &str, f: impl FnOnce(&mut State)) {
        if let Some(s) = lock(&self.teams.0).get_mut(thread) { f(s); }
        self.team_emit(thread);
    }

    /// A hidden member used a tool: show it on the strip.
    pub(crate) fn team_activity(&self, thread: &str, agent: &str, what: &str) {
        let what: String = what.chars().take(60).collect();
        self.team_update(thread, |s| {
            if let Some(r) = s.roles.iter_mut().find(|r| r.agent == agent && r.state == "working") { r.last = what; }
        });
    }

    pub(crate) fn team_state(&self, thread: &str) -> Option<State> { lock(&self.teams.0).get(thread).cloned() }

    /// Post as an agent in the group and mirror it to Discord.
    async fn team_post(&self, g: &GroupRow, thread: &str, agent: &str, text: &str) {
        self.post_message(thread, agent, text);
        if let (Some(d), Some(hook)) = (self.discord_rest(), g.webhook_ref.as_deref().and_then(secret_get)) {
            let _ = d.post_as(&hook, agent, None, text).await;
        }
    }

    pub(crate) async fn team(&self, g: &GroupRow, task: &str) {
        let thread = format!("group:{}", g.id);
        let Some(lead) = g.lead_agent.clone().filter(|l| g.members.contains(l)).or_else(|| g.members.first().cloned()) else { return };
        let Ok(dir) = self.team_dir(&thread) else { return };
        {
            let mut teams = lock(&self.teams.0);
            if teams.get(&thread).is_some_and(|s| matches!(s.stage, "plan" | "work" | "summary")) {
                drop(teams);
                self.post_message(&thread, "system", "这个群已经有一个团队任务在进行，等它结束再开新的。");
                return;
            }
            teams.insert(thread.clone(), State { thread: thread.clone(), task: task.into(), lead: lead.clone(), stage: "plan", roles: Vec::new(), dir: dir.to_string_lossy().into_owned() });
        }
        self.team_emit(&thread);
        let _ = std::fs::remove_dir_all(dir.join("findings"));
        let _ = std::fs::remove_file(dir.join("summary.md"));

        // 1. The brief everyone works from.
        let mut context = format!("# 任务\n\n{task}\n\n# 成员\n\n");
        for m in &g.members { context.push_str(&format!("- {m}{}\n", if *m == lead { "（负责人）" } else { "" })); }
        context.push_str("\n# 最近的群聊\n\n");
        let history = self.db().messages(&thread, 20).unwrap_or_default();
        for m in history {
            let who = if m.author == "me" { "用户" } else { m.author.as_str() };
            let body: String = m.body.chars().take(500).collect();
            context.push_str(&format!("[{who}] {body}\n"));
        }
        let _ = mcp::write_doc(&dir.join("context.md"), &context);
        let ctx_of = |a: &str| format!("team:{thread}:{a}");

        // 2. The lead splits it.
        let plan_prompt = format!(
            "你是团队负责人。下面是任务简报（也保存在 {}）。\n\n{context}\n\
             把任务拆成 1 到 {} 个可以并行的子任务，分给成员（可以包括你自己），每人最多一个。\n\
             只输出一个 JSON 数组，不要别的文字：[{{\"agent\":\"成员名\",\"role\":\"英文短角色名\",\"task\":\"具体子任务\"}}]",
            dir.join("context.md").display(), g.members.len());
        let plan = match self.turn(&thread, &lead, &ctx_of(&lead), &plan_prompt).await {
            Ok(a) => parse_plan(&a, &g.members, task),
            Err(e) => {
                self.post_message(&thread, "system", &format!("团队任务失败：{lead} 没能拆分任务（{e}）"));
                self.team_update(&thread, |s| s.stage = "failed");
                return;
            }
        };
        self.team_update(&thread, |s| {
            s.stage = "work";
            s.roles = plan.iter().map(|(a, r, t)| Role { agent: a.clone(), role: r.clone(), task: t.clone(), state: "working", last: String::new() }).collect();
        });

        let lines: Vec<String> = plan.iter().map(|(a, r, t)| format!("{a}·{r}：{}", t.chars().take(80).collect::<String>())).collect();
        self.post_message(&thread, "system", &format!("🧩 {lead} 分好了工，成员在后台并行处理：\n{}", lines.join("\n")));

        // 3. Everyone works in parallel; each result goes on the blackboard.
        let findings = dir.join("findings");
        let runs = plan.iter().map(|(agent, role, sub)| {
            let prompt = format!(
                "团队任务简报：\n\n{context}\n\n你的角色：{role}\n你的子任务：{sub}\n\n\
                 黑板目录是 {}，其他成员完成后会把产出写到 findings/ 下，需要时可以读。\n\
                 专注你的子任务。最后输出你的完整产出（结论、要点、改动或代码），它会原样保存为 findings/{role}.md 交给负责人汇总。",
                dir.display());
            let (thread, findings, ctx) = (&thread, &findings, ctx_of(agent));
            async move {
                let res = self.turn(thread, agent, &ctx, &prompt).await;
                let (state, body) = match &res {
                    Ok(a) if !a.trim().is_empty() => ("done", a.trim().to_owned()),
                    Ok(_) => ("failed", "（没有产出）".to_owned()),
                    Err(e) => ("failed", format!("（出错：{e}）")),
                };
                let _ = mcp::write_doc(&findings.join(format!("{role}.md")), &format!("# {role} · {agent}\n\n{sub}\n\n---\n\n{body}\n"));
                self.team_update(thread, |s| {
                    if let Some(r) = s.roles.iter_mut().find(|r| r.role == *role) { r.state = state; r.last.clear(); }
                });
                (agent.clone(), role.clone(), body)
            }
        });
        let results = futures::future::join_all(runs).await;
        if self.paused.load(std::sync::atomic::Ordering::Relaxed) {
            self.team_update(&thread, |s| s.stage = "failed");
            return;
        }

        // 4. The lead writes the conclusion.
        self.team_update(&thread, |s| s.stage = "summary");
        let mut prompt = format!("所有成员都完成了。任务是：{task}\n\n各角色的产出（完整版在 {}）：\n\n", findings.display());
        for (agent, role, body) in &results {
            let cut: String = body.chars().take(FINDING_CAP).collect();
            prompt.push_str(&format!("## {role}（{agent}）\n\n{cut}\n\n"));
        }
        prompt.push_str("请综合以上产出，写给用户的最终结论：先给答案，再列关键依据和分歧，最后是建议的下一步。直接输出结论正文。");
        match self.turn(&thread, &lead, &ctx_of(&lead), &prompt).await {
            Ok(summary) => {
                let summary = summary.trim().to_owned();
                self.remember_from(&lead, &thread, &summary);
                let _ = mcp::write_doc(&dir.join("summary.md"), &format!("# 团队结论\n\n{task}\n\n---\n\n{summary}\n"));
                let roles: Vec<String> = results.iter().map(|(a, r, _)| format!("{a}·{r}")).collect();
                self.team_post(g, &thread, &lead, &format!("🧩 团队结论（{}）\n\n{summary}", roles.join("、"))).await;
                self.team_update(&thread, |s| s.stage = "done");
            }
            Err(e) => {
                self.post_message(&thread, "system", &format!("团队任务没能汇总：{e}。各角色的产出在协作面板里。"));
                self.team_update(&thread, |s| s.stage = "failed");
            }
        }
    }

    /// The blackboard files of a group's last team run: `(name, content)`.
    pub(crate) fn team_files(&self, thread: &str) -> Vec<(String, String)> {
        let Ok(dir) = self.team_dir(thread) else { return Vec::new() };
        let mut out = Vec::new();
        for name in ["summary.md", "context.md"] {
            if let Ok(t) = std::fs::read_to_string(dir.join(name)) { out.push((name.to_owned(), t)); }
        }
        let mut f: Vec<(String, String)> = std::fs::read_dir(dir.join("findings")).into_iter().flatten().flatten()
            .filter_map(|e| Some((format!("findings/{}", e.file_name().to_string_lossy()), std::fs::read_to_string(e.path()).ok()?))).collect();
        f.sort();
        out.extend(f);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn plan_parsing() {
        let m = vec!["claude".to_string(), "codex".to_string()];
        let p = parse_plan("好的：\n[{\"agent\":\"@Claude\",\"role\":\"research\",\"task\":\"查资料\"},{\"agent\":\"codex\",\"role\":\"research\",\"task\":\"写代码\"},{\"agent\":\"nobody\",\"role\":\"x\"}]", &m, "T");
        assert_eq!(p, vec![("claude".into(), "research".into(), "查资料".into()), ("codex".into(), "research2".into(), "写代码".into())]);
        assert_eq!(parse_plan("没有 JSON", &m, "T").len(), 2);
        // Prose with brackets, a ```json fence, brackets inside strings, a duplicate agent.
        let fenced = "计划如下 [1]：\n```json\n[{\"agent\":\"claude\",\"role\":\"r]1\",\"task\":\"看 \\\"[x]\\\" 和 {y}\"},{\"agent\":\"claude\",\"role\":\"r2\"},{\"agent\":\"codex\"}]\n```\n有问题再说 [1]";
        assert_eq!(parse_plan(fenced, &m, "T"), vec![("claude".into(), "r-1".into(), "看 \"[x]\" 和 {y}".into()), ("codex".into(), "codex".into(), "T".into())]);
        let obj = "{\"roles\":[{\"agent\":\"codex\",\"role\":\"dev\",\"task\":\"写\"}]}";
        assert_eq!(parse_plan(obj, &m, "T"), vec![("codex".into(), "dev".into(), "写".into())]);
        assert_eq!(parse_plan("{\"agent\":\"claude\",\"task\":\"一\"}", &m, "T"), vec![("claude".into(), "claude".into(), "一".into())]);
        assert_eq!(command("/team  调研 A2A "), Some("调研 A2A"));
        assert_eq!(command("/team"), None);
    }
}
