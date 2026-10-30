//! Disco's own MCP server: the tools that let an agent act in Disco instead of only
//! answering — post to a group or to the user, ask another agent, keep the group's
//! to-dos and shared doc, save long-term notes.
//!
//! Every ACP session is handed this same executable in `--mcp` mode (a tiny stdio
//! proxy) with a token minted for that session. The proxy forwards each JSON-RPC
//! line to a loopback HTTP endpoint inside the app, where the token alone says *who*
//! is calling and *where* they are; nothing the caller sends can change either.
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use tauri::Manager;

use crate::{Core, GroupRow, mentions, secret_get};

const TODO: &str = "todo/";
/// How long `ask_user` waits for the human.
const ASK_USER_WAIT: std::time::Duration = std::time::Duration::from_secs(30 * 60);
/// Longest doc an agent may write in one call; keeps a runaway agent from filling the disk.
const DOC_MAX: usize = 200_000;

// ───────────────────────── stdio side (runs as `disco-app --mcp`) ─────────────────────────

/// Forward MCP JSON-RPC lines from stdin to the app; print the replies on stdout.
/// Requests run on their own threads, so a slow `ask` never blocks a `ping`.
/// Exits as soon as stdin closes (the agent is gone) or Disco stops answering on its
/// port, so the proxy never outlives either.
pub fn stdio_proxy() {
    use std::io::{BufRead, Write};
    let port: u16 = std::env::var("DISCO_MCP_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(0);
    let token = std::env::var("DISCO_MCP_TOKEN").unwrap_or_default();
    // Started outside Disco (e.g. pi in a terminal reading its mcp.json): no tools, no noise.
    let offline = port == 0;
    if !offline {
        std::thread::spawn(move || {
            let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
            let mut misses = 0;
            loop {
                std::thread::sleep(std::time::Duration::from_secs(5));
                let up = std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_secs(2)).is_ok();
                misses = if up { 0 } else { misses + 1 };
                if misses >= 3 { std::process::exit(0); }
            }
        });
    }
    let out = Arc::new(std::sync::Mutex::new(std::io::stdout()));
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() { continue; }
        let (out, token) = (out.clone(), token.clone());
        std::thread::spawn(move || {
            let reply = match if offline { Ok(offline_reply(&line)) } else { forward(port, &token, &line) } {
                Ok(body) => body,
                Err(e) => match serde_json::from_str::<Value>(&line).ok().and_then(|v| v.get("id").cloned()) {
                    Some(id) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32000, "message": format!("Disco 没有响应：{e}")}}).to_string(),
                    None => String::new(),
                },
            };
            if !reply.trim().is_empty() {
                let mut o = out.lock().unwrap_or_else(|p| p.into_inner());
                let _ = writeln!(o, "{}", reply.trim());
                let _ = o.flush();
            }
        });
    }
    // EOF: nobody reads the replies of requests still in flight (a long `ask`), so don't wait.
    std::process::exit(0);
}

/// Answers when Disco isn't running: an empty server, so the host just shows no tools.
fn offline_reply(line: &str) -> String {
    let Ok(msg) = serde_json::from_str::<Value>(line) else { return String::new() };
    let Some(id) = msg.get("id").cloned() else { return String::new() };
    let result = match msg.get("method").and_then(Value::as_str).unwrap_or("") {
        "initialize" => json!({
            "protocolVersion": msg.pointer("/params/protocolVersion").and_then(Value::as_str).unwrap_or("2025-06-18"),
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "disco", "version": env!("CARGO_PKG_VERSION") },
        }),
        "tools/list" => json!({ "tools": [] }),
        _ => json!({}),
    };
    json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string()
}

fn forward(port: u16, token: &str, line: &str) -> std::io::Result<String> {
    use std::io::{Read, Write};
    let body = format!("{{\"msg\":{line}}}");
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port))?;
    write!(s, "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())?;
    let mut resp = String::new();
    s.read_to_string(&mut resp)?;
    let (head, body) = resp.split_once("\r\n\r\n").unwrap_or((&resp, ""));
    let ok = head.split(' ').nth(1).is_some_and(|c| c.starts_with('2'));
    if ok { Ok(body.to_owned()) } else { Err(std::io::Error::other(head.lines().next().unwrap_or("bad response").to_owned())) }
}

// ───────────────────────── app side ─────────────────────────

/// Listen on a loopback port and remember it, so agents started from now on get the tools.
pub fn start(core: Arc<Core>) {
    let Ok(listener) = std::net::TcpListener::bind("127.0.0.1:0") else { return };
    let Ok(addr) = listener.local_addr() else { return };
    if listener.set_nonblocking(true).is_err() { return; }
    let _ = core.hub.mcp_port.set(addr.port());
    tauri::async_runtime::spawn(async move {
        let Ok(l) = tokio::net::TcpListener::from_std(listener) else { return };
        let app = axum::Router::new().route("/mcp", axum::routing::post(handle)).with_state(core);
        let _ = axum::serve(l, app).await;
    });
}

/// Any `context` a caller adds is ignored: the token decides.
#[derive(serde::Deserialize)]
struct Envelope { msg: Value }

/// What a friend's session counts as: only `send_file` (back to the friend's device).
const PEER: &str = "peer:";

async fn handle(State(core): State<Arc<Core>>, headers: HeaderMap, body: String) -> Response {
    let token = headers.get("authorization").and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).unwrap_or("");
    let Some((agent, mut context)) = core.hub.mcp_tokens.lookup(token) else { return StatusCode::UNAUTHORIZED.into_response() };
    let Ok(Envelope { msg }) = serde_json::from_str(&body) else { return StatusCode::BAD_REQUEST.into_response() };
    if context.is_empty() {
        // The agent-wide token (pi): it acts where the agent was last asked, but never while a
        // friend is driving the agent, since that friend's session holds the same token.
        let last = core.hub.last_context.lock().unwrap_or_else(|p| p.into_inner()).get(&agent).cloned();
        context = if core.hub.peer_busy(&agent) { PEER.into() } else { last.unwrap_or_else(|| format!("dm:{agent}")) };
    }
    // Notifications (no id) need no answer.
    let Some(id) = msg.get("id").cloned() else { return StatusCode::ACCEPTED.into_response() };
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    let result = match msg.get("method").and_then(Value::as_str).unwrap_or("") {
        "initialize" => Ok(json!({
            "protocolVersion": params.get("protocolVersion").and_then(Value::as_str).unwrap_or("2025-06-18"),
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "disco", "version": env!("CARGO_PKG_VERSION") },
            "instructions": "Disco 是你所在的多 agent 协作应用。用这些工具在群里发言、请教其他 agent、维护待办和群文档、保存长期记忆。不确定自己在哪时先调用 overview。",
        })),
        "ping" => Ok(json!({})),
        "tools/list" if context.starts_with(PEER) => Ok(json!({ "tools": tools().as_array().into_iter().flatten().filter(|t| t["name"] == "send_file").map(|t| {
            let mut t = t.clone();
            t["description"] = json!("把一个文件直接发给正在和你对话的朋友（在另一台设备上，最多 20 MB）。对方的 Disco 会校验 SHA-256 后存进它的会话。");
            if let Some(p) = t["inputSchema"]["properties"].as_object_mut() { p.remove("group"); }
            t
        }).collect::<Vec<_>>() })),
        "tools/list" => Ok(json!({ "tools": tools() })),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("").to_owned();
            let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
            let r = Call { core: &core, agent: &agent, context: &context }.run(&name, &args).await;
            let _ = core.db().audit("mcp", Some(&agent), &format!("tool.{name}"), if r.is_ok() { "ok" } else { "error" });
            Ok(match r {
                Ok(text) => json!({ "content": [{ "type": "text", "text": text }] }),
                Err(e) => json!({ "content": [{ "type": "text", "text": e }], "isError": true }),
            })
        }
        m => Err(format!("unknown method {m}")),
    };
    let reply = match result {
        Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
        Err(e) => json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": e } }),
    };
    axum::Json(reply).into_response()
}

/// The names of the tools Disco hands every agent (`/tools` in a chat).
pub(crate) fn tool_names() -> Vec<String> {
    tools().as_array().into_iter().flatten().filter_map(|t| t["name"].as_str().map(str::to_owned)).collect()
}

fn tools() -> Value {
    let group = json!({ "type": "string", "description": "群名或群 id。不填 = 当前所在的群" });
    json!([
        { "name": "overview", "description": "你是谁、现在在哪（哪个群或私聊）、你所在的群和成员、所有 agent、群文档位置。",
          "inputSchema": { "type": "object", "properties": {} } },
        { "name": "post", "description": "以你的身份发一条消息。发到群里时，消息里 @名字 的成员会被叫来回复；在私聊里用它可以主动通知用户。不要用它发你本轮的最终回答（最终回答会自动发出）。",
          "inputSchema": { "type": "object", "properties": { "text": { "type": "string" }, "group": group, "to_user": { "type": "boolean", "description": "true = 发到你和用户的私聊" } }, "required": ["text"] } },
        { "name": "ask", "description": "向另一个 agent 请教并等待它的回答（委派：对方单独作答，答案直接返回给你，不会发到群里）。适合需要别人专长的子问题。",
          "inputSchema": { "type": "object", "properties": { "agent": { "type": "string" }, "question": { "type": "string" } }, "required": ["agent", "question"] } },
        { "name": "ask_user", "description": "向用户提问并等待回答（会在 Disco 和 Discord 里弹出卡片，用户在任一端回答即可）。需要用户做选择、确认或补充信息时用；能自己判断的不要问。给 options 时用户点选即可。",
          "inputSchema": { "type": "object", "properties": { "question": { "type": "string" }, "options": { "type": "array", "items": { "type": "string" }, "maxItems": 10, "description": "可选项（最多 10 个）" }, "allow_text": { "type": "boolean", "description": "允许用户自由输入（不给 options 时默认 true）" } }, "required": ["question"] } },
        { "name": "todo_add", "description": "在群（或私聊）的待办里加一项。",
          "inputSchema": { "type": "object", "properties": { "text": { "type": "string" }, "assignee": { "type": "string", "description": "负责人：agent 名或 me（用户）" }, "group": group }, "required": ["text"] } },
        { "name": "todo_list", "description": "列出待办（默认只列未完成）。",
          "inputSchema": { "type": "object", "properties": { "group": group, "include_done": { "type": "boolean" } } } },
        { "name": "todo_done", "description": "把一项待办标记为完成。",
          "inputSchema": { "type": "object", "properties": { "id": { "type": "string" }, "group": group }, "required": ["id"] } },
        { "name": "doc_read", "description": "读取群文档（group.md）：群的目标、结论、分工等共享上下文。",
          "inputSchema": { "type": "object", "properties": { "group": group } } },
        { "name": "doc_write", "description": "写群文档。mode=append 在末尾追加（默认），replace 整篇替换。改动前先 doc_read。",
          "inputSchema": { "type": "object", "properties": { "content": { "type": "string" }, "mode": { "type": "string", "enum": ["append", "replace"] }, "group": group }, "required": ["content"] } },
        { "name": "remember", "description": "保存一条长期记忆，以后的新会话都会看到。scope：self 只给你自己（默认），group 给当前群的所有成员，all 给所有 agent。topic 是主题（如 code-style、user-prefs），同主题的记忆存在一起。只记持久的事实和偏好。",
          "inputSchema": { "type": "object", "properties": { "text": { "type": "string" }, "scope": { "type": "string", "enum": ["self", "group", "all"] }, "topic": { "type": "string" } }, "required": ["text"] } },
        { "name": "send_file", "description": "把一个文件发到当前群或私聊：放进共享空间并在会话里贴出路径和 SHA-256，群成员都能读到。会话里有其他设备上的 agent（通过邀请码共享来的）时，文件也会随下一条消息发给它（最多 20 MB）。",
          "inputSchema": { "type": "object", "properties": { "path": { "type": "string", "description": "文件的绝对路径" }, "note": { "type": "string", "description": "附言（可选）" }, "group": group }, "required": ["path"] } },
        { "name": "memory_read", "description": "读长期记忆：不给 topic 时返回该层的索引（MEMORY.md），给 topic 返回该主题的全部记忆。",
          "inputSchema": { "type": "object", "properties": { "scope": { "type": "string", "enum": ["self", "group", "all"] }, "topic": { "type": "string" } } } },
    ])
}

/// Where a session lives.
enum Place { Dm, Group(GroupRow), Ask, Elsewhere }

struct Call<'a> { core: &'a Arc<Core>, agent: &'a str, context: &'a str }

fn s<'v>(args: &'v Value, k: &str) -> Option<&'v str> {
    args.get(k).and_then(Value::as_str).map(str::trim).filter(|v| !v.is_empty())
}

impl Call<'_> {
    fn my_groups(&self) -> Vec<GroupRow> {
        self.core.db().list_groups().unwrap_or_default().into_iter().filter(|g| g.members.iter().any(|m| m == self.agent)).collect()
    }

    fn place(&self) -> Place {
        if self.context == format!("dm:{}", self.agent) { return Place::Dm; }
        if self.context.starts_with("ask:") { return Place::Ask; }
        // Team sessions (`team:group:<id>:<agent>`) live in their group too.
        let ctx = self.context.strip_prefix("team:").unwrap_or(self.context);
        if let Some(rest) = ctx.strip_prefix("group:")
            && let id = rest.split(':').next().unwrap_or(rest)
            && let Some(g) = self.my_groups().into_iter().find(|g| g.id == id)
        {
            return Place::Group(g);
        }
        Place::Elsewhere
    }

    /// The group a call targets: the named one (must be one of ours), else the current one.
    fn group(&self, args: &Value) -> Result<Option<GroupRow>, String> {
        if let Some(want) = s(args, "group") {
            let w = want.trim_start_matches('#');
            return self.my_groups().into_iter().find(|g| g.id == w || g.name == w).map(Some)
                .ok_or_else(|| format!("你不在群「{want}」里（用 overview 查看你所在的群）"));
        }
        Ok(match self.place() { Place::Group(g) => Some(g), _ => None })
    }

    /// Thread + space for to-dos and the doc: the target group, else this DM.
    fn space(&self, args: &Value) -> Result<(String, String), String> {
        match self.group(args)? {
            Some(g) => Ok((format!("group:{}", g.id), g.name)),
            None if matches!(self.place(), Place::Dm) => Ok((format!("dm:{}", self.agent), "私聊".into())),
            None => Err("这里不是群也不是私聊，请用 group 参数指定群".into()),
        }
    }

    async fn run(&self, name: &str, args: &Value) -> Result<String, String> {
        // A friend's session on a shared agent must never reach the owner's groups or notes:
        // the one tool there sends a file back to the friend.
        if self.context.starts_with(PEER) {
            return match (name, s(args, "path")) {
                ("send_file", Some(path)) => self.core.peer_send_file(self.context, self.agent, path, s(args, "note").unwrap_or("")).await,
                ("send_file", None) => Err("path 不能为空".into()),
                _ => Err("这是朋友通过共享访问的会话，这里只能用 send_file 把文件发给对方".into()),
            };
        }
        if name != "overview" && self.core.paused.load(std::sync::atomic::Ordering::Relaxed) {
            return Err("用户已全部停止，暂不执行".into());
        }
        match name {
            "overview" => Ok(self.overview()),
            "post" => self.post(args),
            "ask" => self.ask(args).await,
            "ask_user" => self.ask_user(args).await,
            "todo_add" => self.todo_add(args),
            "todo_list" => self.todo_list(args),
            "todo_done" => self.todo_done(args),
            "doc_read" => self.doc_read(args),
            "doc_write" => self.doc_write(args),
            "remember" | "memory_read" if !self.core.memory_on(self.context) => Err("用户关闭了这个对话的记忆，现在不能读写记忆".into()),
            "remember" => {
                let text = s(args, "text").ok_or("text 不能为空")?;
                let (scope, label) = self.mem_scope(args)?;
                self.core.remember_topic(&scope, s(args, "topic"), text)?;
                Ok(format!("已记住（{label}）"))
            }
            "send_file" => {
                let path = s(args, "path").ok_or("path 不能为空")?;
                let thread = self.space(args)?.0;
                self.core.share_file(&thread, self.agent, path, s(args, "note").unwrap_or("")).await
            }
            "memory_read" => {
                let (scope, _) = self.mem_scope(args)?;
                let doc = self.core.memory_read(&scope, s(args, "topic"));
                Ok(if doc.trim().is_empty() { "（还没有记忆）".into() } else { doc })
            }
            n => Err(format!("没有工具 {n}")),
        }
    }

    /// The memory layer a call names: `(scope dir, label)`.
    fn mem_scope(&self, args: &Value) -> Result<(String, &'static str), String> {
        match s(args, "scope") {
            Some("all") => Ok(("all".into(), "所有 agent 可见")),
            Some("group") => crate::memory::group_of(self.context).map(|g| (g, "本群成员可见")).ok_or_else(|| "当前不在群里，不能用 scope=group".into()),
            _ => Ok((self.agent.to_owned(), "只有你自己可见")),
        }
    }

    fn overview(&self) -> String {
        let mut o = format!("你是 {}。\n当前位置：", self.agent);
        o.push_str(&match self.place() {
            Place::Dm => "和用户的私聊".to_owned(),
            Place::Group(g) => format!("群「{}」（id {}，成员 {}）", g.name, g.id, g.members.join("、")),
            Place::Ask => "另一个 agent 通过 ask 向你请教（你的回答会直接交给它）".to_owned(),
            Place::Elsewhere => "未知".to_owned(),
        });
        o.push_str("\n\n你所在的群：\n");
        let groups = self.my_groups();
        if groups.is_empty() { o.push_str("（无）\n"); }
        for g in &groups {
            o.push_str(&format!("- {}（id {}）成员：{}{}\n", g.name, g.id, g.members.join("、"),
                g.lead_agent.as_deref().map(|l| format!("，组长 {l}")).unwrap_or_default()));
        }
        let rows: Vec<_> = self.core.db().list_agents().unwrap_or_default().into_iter().filter(|a| a.name != self.agent).collect();
        o.push_str(if rows.is_empty() { "\n其他 agent：（无）\n" } else { "\n其他 agent：\n" });
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
        for a in &rows {
            let (since, doing) = self.core.hub.activity(&a.name);
            let busy = since.map(|t| (now - t).max(0) / 1000);
            o.push_str(&format!("- {}\n", agent_line(&a.name, busy, self.core.hub.branch(a).as_deref(), doing.as_deref())));
        }
        if let Ok((thread, _)) = self.space(&json!({})) && let Ok(p) = doc_path(self.core, &thread) {
            o.push_str(&format!("\n这里的群文档文件：{}\n", p.display()));
        }
        o
    }

    fn post(&self, args: &Value) -> Result<String, String> {
        let text = s(args, "text").ok_or("text 不能为空")?.to_owned();
        let to_user = args.get("to_user").and_then(Value::as_bool).unwrap_or(false);
        let g = if to_user { None } else { self.group(args)? };
        let Some(g) = g else {
            if !to_user && !matches!(self.place(), Place::Dm) {
                return Err("请用 group 指定群，或设 to_user=true 发给用户".into());
            }
            let thread = format!("dm:{}", self.agent);
            self.core.post_message(&thread, self.agent, &text);
            if let Some(ch) = self.core.agent_channel(self.agent) && let Some(d) = self.core.discord_rest()
                && let Some(hook) = secret_get(&format!("webhook/{ch}"))
            {
                let (agent, text) = (self.agent.to_owned(), text.clone());
                tauri::async_runtime::spawn(async move { let _ = d.post_as(&hook, &agent, None, &text).await; });
            }
            return Ok("已发给用户".into());
        };
        let called = mentions(&text, &g.members, Some(self.agent));
        let (core, id, agent) = (self.core.clone(), g.id.clone(), self.agent.to_owned());
        // Routing runs on its own: the members we @ answer after this call returns.
        tauri::async_runtime::spawn(async move { core.group(id, agent, text, false).await; });
        Ok(if called.is_empty() { format!("已发到群「{}」", g.name) } else { format!("已发到群「{}」，已叫 {} 来回复", g.name, called.join("、")) })
    }

    async fn ask(&self, args: &Value) -> Result<String, String> {
        if matches!(self.place(), Place::Ask) {
            return Err("你正在回答别人的请教，不能再转问（防止循环）。请直接作答。".into());
        }
        let target = s(args, "agent").ok_or("agent 不能为空")?.trim_start_matches('@').to_owned();
        let question = s(args, "question").ok_or("question 不能为空")?;
        if target == self.agent { return Err("不能问自己".into()); }
        self.core.agent(&target)?;
        let thread = match self.place() { Place::Group(g) => format!("group:{}", g.id), _ => format!("dm:{}", self.agent) };
        self.core.post_message(&thread, self.agent, &format!("→ 请教 {target}：{question}"));
        let prompt = format!("{} 通过 Disco 向你请教。你的回答会直接交给它，不会发到群里。请简洁作答：\n\n{question}", self.agent);
        // The asking chat is part of the context, so its memory switch holds here too.
        let answer = self.core.turn(&thread, &target, &format!("ask:{}:{thread}", self.agent), &prompt).await?;
        let answer = answer.trim().to_owned();
        self.core.post_message(&thread, &target, &format!("↩ 回复 {}：{answer}", self.agent));
        Ok(answer)
    }

    async fn ask_user(&self, args: &Value) -> Result<String, String> {
        let question = s(args, "question").ok_or("question 不能为空")?;
        let options: Vec<String> = args.get("options").and_then(Value::as_array).map(|a| a.iter()
            .filter_map(|v| v.as_str().map(str::trim).filter(|v| !v.is_empty()).map(str::to_owned)).take(10).collect()).unwrap_or_default();
        let text = args.get("allow_text").and_then(Value::as_bool).unwrap_or(options.is_empty());
        let thread = match self.place() { Place::Group(g) => format!("group:{}", g.id), _ => format!("dm:{}", self.agent) };
        self.core.post_message(&thread, self.agent, &format!("❓ {question}"));
        let (id, rx) = self.core.inbox_question(&thread, self.agent, question, options, text);
        // Waiting on a person isn't work: free this turn's slot for others meanwhile.
        self.core.hub.yield_slot(self.agent, self.context);
        match tokio::time::timeout(ASK_USER_WAIT, rx).await {
            Ok(Ok(answer)) => Ok(format!("用户回答：{answer}")),
            Ok(Err(_)) => Err("用户撤回了这个问题（已全部停止）".into()),
            Err(_) => {
                let mins = ASK_USER_WAIT.as_secs() / 60;
                self.core.inbox_withdraw(&id, &format!("（已超时：{mins} 分钟内没有回答）"));
                Err(format!("{mins} 分钟内用户没有回答，请按最稳妥的方式继续或稍后再问"))
            }
        }
    }

    fn todo_add(&self, args: &Value) -> Result<String, String> {
        let text = s(args, "text").ok_or("text 不能为空")?;
        let (thread, label) = self.space(args)?;
        let assignee = s(args, "assignee").map(|a| a.trim_start_matches('@').to_owned());
        let id = add_todo(self.core, &thread, text, assignee.as_deref(), self.agent);
        let who = assignee.as_deref().map(|a| format!("（{}）", if a == "me" { "用户" } else { a })).unwrap_or_default();
        self.core.post_message(&thread, "system", &format!("📌 {} 添加待办：{text}{who}", self.agent));
        Ok(format!("已加到「{label}」的待办，id {id}"))
    }

    fn todo_list(&self, args: &Value) -> Result<String, String> {
        let (thread, label) = self.space(args)?;
        let all = args.get("include_done").and_then(Value::as_bool).unwrap_or(false);
        let items: Vec<Todo> = todos(self.core, &thread).into_iter().filter(|t| all || !t.done).collect();
        if items.is_empty() { return Ok(format!("「{label}」没有{}待办", if all { "" } else { "未完成的" })); }
        Ok(items.iter().map(|t| format!("[{}] {} {}{}", t.id, if t.done { "✓" } else { "○" }, t.text,
            t.assignee.as_deref().map(|a| format!(" → {a}")).unwrap_or_default())).collect::<Vec<_>>().join("\n"))
    }

    fn todo_done(&self, args: &Value) -> Result<String, String> {
        let id = s(args, "id").ok_or("id 不能为空")?;
        let (thread, _) = self.space(args)?;
        let t = set_todo_done(self.core, &thread, id, true)?;
        self.core.post_message(&thread, "system", &format!("✅ {} 完成待办：{}", self.agent, t.text));
        Ok(format!("已完成：{}", t.text))
    }

    fn doc_read(&self, args: &Value) -> Result<String, String> {
        let (thread, label) = self.space(args)?;
        let doc = std::fs::read_to_string(doc_path(self.core, &thread)?).unwrap_or_default();
        Ok(if doc.trim().is_empty() { format!("「{label}」的群文档还是空的") } else { doc })
    }

    fn doc_write(&self, args: &Value) -> Result<String, String> {
        let content = args.get("content").and_then(Value::as_str).ok_or("content 不能为空")?;
        let (thread, label) = self.space(args)?;
        let path = doc_path(self.core, &thread)?;
        let replace = s(args, "mode") == Some("replace");
        let _guard = crate::memory::fs_lock();
        let mut doc = if replace { String::new() } else { std::fs::read_to_string(&path).unwrap_or_default() };
        if !doc.is_empty() && !doc.ends_with('\n') { doc.push('\n'); }
        doc.push_str(content);
        if !doc.ends_with('\n') { doc.push('\n'); }
        if doc.len() > DOC_MAX { return Err(format!("文档会超过 {} KB，请精简后用 replace 重写", DOC_MAX / 1000)); }
        write_doc(&path, &doc)?;
        drop(_guard);
        self.core.post_message(&thread, "system", &format!("📝 {} {}了群文档", self.agent, if replace { "重写" } else { "更新" }));
        let _ = self.core.app.emit_space(&thread);
        Ok(format!("已写入「{label}」的群文档（{}）", path.display()))
    }
}

// ───────────────────────── storage shared with the UI ─────────────────────────

#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub struct Todo {
    #[serde(default)]
    pub id: String,
    pub text: String,
    #[serde(default)]
    pub assignee: Option<String>,
    #[serde(default)]
    pub by: String,
    #[serde(default)]
    pub done: bool,
}

pub(crate) fn todo_prefix(thread: &str) -> String { format!("{TODO}{thread}/") }

pub fn todos(core: &Core, thread: &str) -> Vec<Todo> {
    let prefix = todo_prefix(thread);
    core.db().settings_with_prefix(&prefix).unwrap_or_default().into_iter().filter_map(|(k, v)| {
        let mut t: Todo = serde_json::from_str(&v).ok()?;
        t.id = short_id(&k); // keys come back without the prefix
        Some(t)
    }).collect()
}

/// Ids are microsecond keys; their last 6 digits are plenty to tell a list apart.
/// One agent at a glance: `claude · busy 2m · main · 写 README 章节`.
fn agent_line(name: &str, busy_secs: Option<i64>, branch: Option<&str>, doing: Option<&str>) -> String {
    let busy = match busy_secs {
        None => "idle".to_owned(),
        Some(s) if s < 60 => format!("busy {s}s"),
        Some(s) if s < 3600 => format!("busy {}m", s / 60),
        Some(s) => format!("busy {}h{}m", s / 3600, s % 3600 / 60),
    };
    [Some(name), Some(busy.as_str()), branch, doing].into_iter().flatten().collect::<Vec<_>>().join(" · ")
}

fn short_id(key: &str) -> String { key[key.len().saturating_sub(6)..].to_owned() }

pub fn add_todo(core: &Core, thread: &str, text: &str, assignee: Option<&str>, by: &str) -> String {
    let mut ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_micros()).unwrap_or(0);
    let db = core.db();
    while db.setting(&format!("{}{ts:020}", todo_prefix(thread))).is_some() { ts += 1; }
    let t = Todo { id: String::new(), text: text.to_owned(), assignee: assignee.map(Into::into), by: by.to_owned(), done: false };
    let _ = db.set_setting(&format!("{}{ts:020}", todo_prefix(thread)), &serde_json::to_string(&t).unwrap_or_default());
    drop(db);
    let _ = core.app.emit_space(thread);
    short_id(&format!("{ts:020}"))
}

fn find_todo(core: &Core, thread: &str, id: &str) -> Result<(String, Todo), String> {
    let prefix = todo_prefix(thread);
    core.db().settings_with_prefix(&prefix).unwrap_or_default().into_iter()
        .find(|(k, v)| k.ends_with(id) && !v.is_empty())
        .and_then(|(k, v)| Some((format!("{prefix}{k}"), serde_json::from_str(&v).ok()?)))
        .ok_or_else(|| format!("没有 id 为 {id} 的待办"))
}

pub fn set_todo_done(core: &Core, thread: &str, id: &str, done: bool) -> Result<Todo, String> {
    let (key, mut t) = find_todo(core, thread, id)?;
    t.done = done;
    core.db().set_setting(&key, &serde_json::to_string(&t).unwrap_or_default()).map_err(|e| e.to_string())?;
    let _ = core.app.emit_space(thread);
    Ok(t)
}

pub fn delete_todo(core: &Core, thread: &str, id: &str) -> Result<(), String> {
    let (key, _) = find_todo(core, thread, id)?;
    core.db().set_setting(&key, "").map_err(|e| e.to_string())?;
    let _ = core.app.emit_space(thread);
    Ok(())
}

/// The folder name of a thread's space: `group:1` → `group-1`. Anything that could step
/// outside `spaces/` (`..`, a path separator, NUL) is refused.
pub(crate) fn space_name(thread: &str) -> Result<String, String> {
    if thread.is_empty() || thread.contains("..") || thread.contains(['/', '\\', '\0']) {
        return Err(format!("无效的会话：{thread:?}"));
    }
    Ok(thread.replace(':', "-"))
}

/// `<app data>/spaces/<thread>/`: one folder per group or DM (doc, team blackboard, files).
pub fn space_dir(core: &Core, thread: &str) -> Result<PathBuf, String> {
    let base = core.app.path().app_data_dir().unwrap_or_else(|_| std::env::temp_dir());
    Ok(base.join("spaces").join(space_name(thread)?))
}

/// `<app data>/spaces/<thread>/group.md`.
pub fn doc_path(core: &Core, thread: &str) -> Result<PathBuf, String> {
    Ok(space_dir(core, thread)?.join("group.md"))
}

pub fn write_doc(path: &std::path::Path, doc: &str) -> Result<(), String> {
    if let Some(d) = path.parent() { std::fs::create_dir_all(d).map_err(|e| e.to_string())?; }
    // Write then rename, so a reader never sees half a file.
    let tmp = tmp_path(path);
    std::fs::write(&tmp, doc).and_then(|()| std::fs::rename(&tmp, path)).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        e.to_string()
    })
}

/// A scratch name beside `path`, unique per process and call, so concurrent
/// writers never share one (hidden, and not `.md`, so no listing picks it up).
pub(crate) fn tmp_path(path: &std::path::Path) -> PathBuf {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let name = path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
    path.with_file_name(format!(".{name}.{}-{n}.tmp", std::process::id()))
}

/// Tell the UI a thread's to-dos or doc changed.
pub trait EmitSpace { fn emit_space(&self, thread: &str) -> tauri::Result<()>; }
impl EmitSpace for tauri::AppHandle {
    fn emit_space(&self, thread: &str) -> tauri::Result<()> {
        use tauri::Emitter;
        self.emit("space", thread)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn agent_lines() {
        assert_eq!(super::agent_line("claude", Some(130), Some("main"), Some("写 README 章节")), "claude · busy 2m · main · 写 README 章节");
        assert_eq!(super::agent_line("pi", None, None, None), "pi · idle");
        assert_eq!(super::agent_line("x", Some(5), None, Some("跑测试")), "x · busy 5s · 跑测试");
        assert_eq!(super::agent_line("x", Some(3720), Some("dev"), None), "x · busy 1h2m · dev");
    }

    #[test]
    fn space_names_stay_inside() {
        assert_eq!(super::space_name("group:123").as_deref(), Ok("group-123"));
        assert_eq!(super::space_name("dm:小助手").as_deref(), Ok("dm-小助手"));
        for bad in ["", "..", "dm:..", "group:../x", "dm:a/b", "dm:a\\b", "dm:a\0", "/etc"] {
            assert!(super::space_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn tmp_names_are_unique() {
        let p = std::path::Path::new("/x/y/group.md");
        let (a, b) = (super::tmp_path(p), super::tmp_path(p));
        assert_ne!(a, b);
        assert_eq!(a.parent(), p.parent());
        let name = a.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with(".group.md.") && name.ends_with(".tmp"), "{name}");
    }

    #[test]
    fn offline_is_an_empty_server() {
        let init = super::offline_reply(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26"}}"#);
        assert!(init.contains("\"protocolVersion\":\"2025-03-26\""));
        assert!(super::offline_reply(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#).contains("\"tools\":[]"));
        assert!(super::offline_reply(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).is_empty());
    }
}
