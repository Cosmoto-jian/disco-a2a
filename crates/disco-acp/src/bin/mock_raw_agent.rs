//! A second test agent spoken in raw JSON-RPC, for what the typed mock can't do:
//! the legacy `models` / `session/set_model` API (as Hermes uses) and, with the
//! `auth` argument, ACP sign-in (`auth_required` until `authenticate`).
use std::io::{BufRead, Write};

use serde_json::{Value, json};

fn main() {
    let needs_auth = std::env::args().any(|a| a == "auth");
    let mut authed = false;
    let mut model = "m-fast".to_owned();
    let out = std::io::stdout();
    let send = |v: Value| { let mut o = out.lock(); let _ = writeln!(o, "{v}"); let _ = o.flush(); };
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
        let (Some(id), Some(method)) = (msg.get("id").cloned(), msg["method"].as_str()) else { continue };
        let p = &msg["params"];
        let reply = match method {
            "initialize" => Ok(json!({
                "protocolVersion": 1, "agentCapabilities": {},
                "authMethods": if needs_auth { json!([
                    { "id": "mock-key", "name": "Mock Key" },
                    { "id": "mock-term", "name": "Mock Terminal", "type": "terminal", "args": ["--login"] },
                ]) } else { json!([]) },
            })),
            "authenticate" if p["methodId"] == "mock-key" => { authed = true; Ok(json!({})) }
            "session/new" if needs_auth && !authed => Err(json!({ "code": -32000, "message": "Authentication required" })),
            "session/new" => Ok(json!({
                "sessionId": "raw1",
                "models": { "availableModels": [{ "modelId": "m-fast", "name": "Fast" }, { "modelId": "m-smart", "name": "Smart" }], "currentModelId": model },
            })),
            "session/set_model" => { model = p["modelId"].as_str().unwrap_or_default().to_owned(); Ok(json!({})) }
            "session/prompt" => {
                send(json!({ "jsonrpc": "2.0", "method": "session/update", "params": { "sessionId": p["sessionId"],
                    "update": { "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": format!("raw {model}") } } } }));
                Ok(json!({ "stopReason": "end_turn" }))
            }
            _ => Err(json!({ "code": -32601, "message": format!("unknown {method}") })),
        };
        send(match reply {
            Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
            Err(e) => json!({ "jsonrpc": "2.0", "id": id, "error": e }),
        });
    }
}
