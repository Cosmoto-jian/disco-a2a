//! OpenAI-compatible chat endpoints (OpenRouter, DeepSeek, Ollama, LM Studio …) as agents.
//! One history per conversation; Disco's MCP tools are offered as functions and run
//! through the same loopback MCP endpoint ACP agents use, so the same rules apply.
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use disco_types::{AgentEvent, TaskPhase};
use serde_json::{Value, json};

/// Messages kept per conversation (the system prompt aside).
const KEEP: usize = 40;
/// Tool rounds per turn before the model must answer.
const MAX_ROUNDS: usize = 8;

pub struct OpenAi {
    base: String,
    key: Option<String>,
    pub model: Mutex<String>,
    http: reqwest::Client,
    history: Mutex<HashMap<String, Vec<Value>>>,
    stop: Mutex<HashMap<String, Arc<AtomicBool>>>,
    /// Loopback Disco MCP for tool calls: (port, tokens, this agent's name).
    mcp: Option<(u16, crate::hub::McpTokens, String)>,
}

/// Base URL as the user typed it, minus a trailing `/` or a pasted `/chat/completions`.
pub fn normalize(base: &str) -> String {
    base.trim().trim_end_matches('/').trim_end_matches("/chat/completions").trim_end_matches('/').to_owned()
}

/// A client that honours the proxy set in the user's shell (a Dock-launched app has none).
pub fn client(env: &[(String, String)]) -> reqwest::Client {
    let get = |k: &str| env.iter().rev().find(|(n, _)| n.eq_ignore_ascii_case(k)).map(|(_, v)| v.clone()).or_else(|| std::env::var(k).ok());
    let mut b = reqwest::Client::builder().connect_timeout(std::time::Duration::from_secs(15));
    if let Some(p) = get("HTTPS_PROXY").or_else(|| get("ALL_PROXY")).filter(|p| !p.is_empty())
        && let Ok(proxy) = reqwest::Proxy::https(&p)
    {
        let no = format!("127.0.0.1,localhost,{}", get("NO_PROXY").unwrap_or_default());
        b = b.proxy(proxy.no_proxy(reqwest::NoProxy::from_string(&no)));
    }
    b.build().unwrap_or_default()
}

fn auth(req: reqwest::RequestBuilder, key: Option<&str>) -> reqwest::RequestBuilder {
    match key { Some(k) if !k.is_empty() => req.bearer_auth(k), _ => req }
}

fn http_err(status: reqwest::StatusCode, body: &str) -> String {
    let msg = serde_json::from_str::<Value>(body).ok()
        .and_then(|v| v.pointer("/error/message").or_else(|| v.get("error")).or_else(|| v.get("message")).and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_else(|| body.chars().take(300).collect());
    match status.as_u16() {
        401 | 403 => format!("API Key 无效或没有权限（{status}）：{msg}"),
        404 => format!("地址或模型不存在（404）：{msg}。Base URL 一般以 /v1 结尾"),
        429 => format!("请求太频繁或额度用完（429）：{msg}"),
        _ => format!("模型服务返回 {status}：{msg}"),
    }
}

/// Model ids the endpoint lists (empty when it has no /models).
pub async fn models(http: &reqwest::Client, base: &str, key: Option<&str>) -> Result<Vec<String>, String> {
    let r = auth(http.get(format!("{base}/models")), key).send().await.map_err(|e| format!("连不上 {base}：{e}"))?;
    let status = r.status();
    let body = r.text().await.unwrap_or_default();
    if !status.is_success() { return Err(http_err(status, &body)); }
    let v: Value = serde_json::from_str(&body).unwrap_or_default();
    let list = v.get("data").or_else(|| v.get("models")).and_then(Value::as_array).cloned().unwrap_or_default();
    let mut ids: Vec<String> = list.iter().filter_map(|m| m.get("id").or_else(|| m.get("name")).and_then(Value::as_str).map(str::to_owned)).collect();
    ids.sort();
    Ok(ids)
}

impl OpenAi {
    pub fn new(base: &str, key: Option<String>, model: String, http: reqwest::Client, mcp: Option<(u16, crate::hub::McpTokens, String)>) -> Self {
        Self { base: normalize(base), key, model: Mutex::new(model), http, history: Mutex::default(), stop: Mutex::default(), mcp }
    }

    pub async fn models(&self) -> Result<Vec<String>, String> { models(&self.http, &self.base, self.key.as_deref()).await }

    pub fn cancel(&self, context: &str) -> bool {
        self.stop.lock().unwrap_or_else(|p| p.into_inner()).get(context).map(|f| f.store(true, Ordering::Relaxed)).is_some()
    }

    /// Disco's MCP tools as OpenAI functions.
    async fn tools(&self, context: &str) -> Vec<Value> {
        let Ok(v) = self.mcp_call(context, "tools/list", json!({})).await else { return Vec::new() };
        v.get("tools").and_then(Value::as_array).into_iter().flatten().map(|t| json!({ "type": "function", "function": {
            "name": t["name"], "description": t["description"], "parameters": t["inputSchema"] } })).collect()
    }

    async fn mcp_call(&self, context: &str, method: &str, params: Value) -> Result<Value, String> {
        let (port, tokens, agent) = self.mcp.as_ref().ok_or("Disco 工具不可用")?;
        // A friend's conversation gets no tools; any other gets a token bound to it.
        if context.starts_with("peer:") { return Err("Disco 工具不可用".into()); }
        let token = tokens.mint(agent, context);
        let body = json!({ "msg": { "jsonrpc": "2.0", "id": 1, "method": method, "params": params } });
        let r = self.http.post(format!("http://127.0.0.1:{port}/mcp")).bearer_auth(token).json(&body).send().await.map_err(|e| e.to_string())?;
        let v: Value = r.json().await.map_err(|e| e.to_string())?;
        v.get("result").cloned().ok_or_else(|| v.pointer("/error/message").and_then(Value::as_str).unwrap_or("工具调用失败").to_owned())
    }

    async fn run_tool(&self, context: &str, name: &str, args: &str) -> String {
        let args: Value = serde_json::from_str(args).unwrap_or_else(|_| json!({}));
        match self.mcp_call(context, "tools/call", json!({ "name": name, "arguments": args })).await {
            Ok(r) => r.get("content").and_then(Value::as_array).into_iter().flatten()
                .filter_map(|c| c.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n"),
            Err(e) => format!("错误：{e}"),
        }
    }

    /// One user turn in `context`: stream text, run tool calls, repeat until the model answers.
    pub async fn prompt(&self, context: &str, text: &str, on_event: &mut impl FnMut(AgentEvent)) -> Result<(), String> {
        let flag = Arc::new(AtomicBool::new(false));
        self.stop.lock().unwrap_or_else(|p| p.into_inner()).insert(context.to_owned(), flag.clone());
        let mut msgs = self.history.lock().unwrap_or_else(|p| p.into_inner()).get(context).cloned().unwrap_or_default();
        msgs.push(json!({ "role": "user", "content": text }));
        let tools = self.tools(context).await;
        let model = self.model.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let mut result = Ok(());
        for round in 0.. {
            let offer = !tools.is_empty() && round < MAX_ROUNDS;
            let mut body = json!({ "model": model, "messages": msgs, "stream": true });
            if offer { body["tools"] = Value::Array(tools.clone()); }
            let (reply, calls) = match self.stream(&body, &flag, on_event).await {
                Ok(r) => r,
                // Some local servers reject `tools`: retry the turn once without them.
                Err(e) if offer && round == 0 && e.contains("tool") => {
                    body.as_object_mut().map(|o| o.remove("tools"));
                    match self.stream(&body, &flag, on_event).await { Ok(r) => r, Err(e) => { result = Err(e); break; } }
                }
                Err(e) => { result = Err(e); break; }
            };
            if calls.is_empty() {
                msgs.push(json!({ "role": "assistant", "content": reply }));
                break;
            }
            msgs.push(json!({ "role": "assistant", "content": if reply.is_empty() { Value::Null } else { Value::String(reply) }, "tool_calls": calls }));
            for c in &calls {
                let (id, name, args) = (c["id"].as_str().unwrap_or(""), c["function"]["name"].as_str().unwrap_or(""), c["function"]["arguments"].as_str().unwrap_or("{}"));
                on_event(AgentEvent::Tool { id: id.into(), title: name.into(), status: "in_progress".into() });
                let out = self.run_tool(context, name, args).await;
                on_event(AgentEvent::Tool { id: id.into(), title: name.into(), status: "completed".into() });
                msgs.push(json!({ "role": "tool", "tool_call_id": id, "content": out }));
            }
            if flag.load(Ordering::Relaxed) { result = Err("已停止".into()); break; }
        }
        self.stop.lock().unwrap_or_else(|p| p.into_inner()).remove(context);
        if result.is_ok() {
            // Keep the tail, starting at a user message so no tool reply loses its call.
            let cut = msgs.len().saturating_sub(KEEP);
            let start = msgs[cut..].iter().position(|m| m["role"] == "user").map_or(msgs.len(), |i| cut + i);
            self.history.lock().unwrap_or_else(|p| p.into_inner()).insert(context.to_owned(), msgs.split_off(start));
        }
        on_event(AgentEvent::Done { phase: if result.is_ok() { TaskPhase::Completed } else { TaskPhase::Failed }, stop_reason: result.as_ref().err().cloned() });
        result
    }

    /// One streamed completion: (text, tool calls).
    async fn stream(&self, body: &Value, flag: &AtomicBool, on_event: &mut impl FnMut(AgentEvent)) -> Result<(String, Vec<Value>), String> {
        let req = auth(self.http.post(format!("{}/chat/completions", self.base)), self.key.as_deref()).json(body);
        let mut r = req.send().await.map_err(|e| format!("连不上模型服务：{e}"))?;
        let status = r.status();
        if !status.is_success() { return Err(http_err(status, &r.text().await.unwrap_or_default())); }
        let (mut text, mut calls, mut buf) = (String::new(), Vec::<Value>::new(), String::new());
        let mut whole = String::new();
        loop {
            if flag.load(Ordering::Relaxed) { return Err("已停止".into()); }
            let Some(chunk) = r.chunk().await.map_err(|e| format!("连接中断：{e}"))? else { break };
            buf.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(i) = buf.find('\n') {
                let line = buf[..i].trim().to_owned();
                buf.drain(..=i);
                let Some(data) = line.strip_prefix("data:").map(str::trim) else { whole.push_str(&line); continue };
                if data == "[DONE]" { continue; }
                let Ok(v) = serde_json::from_str::<Value>(data) else { continue };
                if let Some(e) = v.get("error") { return Err(format!("模型服务出错：{}", e.get("message").and_then(Value::as_str).unwrap_or(&e.to_string()))); }
                let d = &v["choices"][0]["delta"];
                if let Some(t) = d.get("reasoning_content").or_else(|| d.get("reasoning")).and_then(Value::as_str).filter(|t| !t.is_empty()) {
                    on_event(AgentEvent::Thought { text: t.into() });
                }
                if let Some(t) = d.get("content").and_then(Value::as_str).filter(|t| !t.is_empty()) {
                    text.push_str(t);
                    on_event(AgentEvent::TextChunk { text: t.into() });
                }
                for tc in d.get("tool_calls").and_then(Value::as_array).into_iter().flatten() {
                    let i = tc.get("index").and_then(Value::as_u64).map_or(calls.len(), |i| i as usize);
                    while calls.len() <= i { calls.push(json!({ "id": "", "type": "function", "function": { "name": "", "arguments": "" } })); }
                    let c = &mut calls[i];
                    if let Some(id) = tc.get("id").and_then(Value::as_str) { c["id"] = id.into(); }
                    for k in ["name", "arguments"] {
                        if let Some(s) = tc["function"].get(k).and_then(Value::as_str) {
                            let cur = c["function"][k].as_str().unwrap_or("").to_owned();
                            c["function"][k] = Value::String(cur + s);
                        }
                    }
                }
            }
        }
        // A server that ignored `stream` answers with one plain JSON body.
        whole.push_str(buf.trim());
        if text.is_empty() && calls.is_empty() && let Ok(v) = serde_json::from_str::<Value>(&whole) {
            let m = &v["choices"][0]["message"];
            if let Some(t) = m.get("content").and_then(Value::as_str) { text = t.to_owned(); on_event(AgentEvent::TextChunk { text: text.clone() }); }
            calls = m.get("tool_calls").and_then(Value::as_array).cloned().unwrap_or_default();
        }
        for (n, c) in calls.iter_mut().enumerate() {
            if c["id"].as_str().is_none_or(str::is_empty) { c["id"] = format!("call_{n}").into(); }
        }
        Ok((text, calls))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn base_url_forms() {
        for b in ["https://x.ai/v1", "https://x.ai/v1/", "https://x.ai/v1/chat/completions", " https://x.ai/v1/chat/completions/ "] {
            assert_eq!(super::normalize(b), "https://x.ai/v1");
        }
    }
}
