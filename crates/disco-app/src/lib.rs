//! Disco desktop shell (Tauri 2). The UI only calls the typed commands below;
//! secrets go straight to the macOS Keychain and never touch SQLite or the webview.
#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use disco_a2a::{Credential, RemoteAgent};
use disco_store::{AgentRow, Store};
use disco_types::AgentEvent;
use futures::StreamExt;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};

const KEYCHAIN_SERVICE: &str = "dev.cosmoto.disco";
const DISCORD_KEY: &str = "discord-bot-token";

struct AppState {
    store: Mutex<Store>,
    // Connected clients are cached; resolving a card costs a round trip.
    agents: tokio::sync::Mutex<HashMap<String, Arc<RemoteAgent>>>,
}

type CmdResult<T> = Result<T, String>;

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

fn keychain(account: &str) -> CmdResult<keyring::Entry> {
    keyring::Entry::new(KEYCHAIN_SERVICE, account).map_err(err)
}

fn store<'a>(s: &'a State<'_, AppState>) -> std::sync::MutexGuard<'a, Store> {
    s.store.lock().unwrap_or_else(|p| p.into_inner())
}

#[derive(Serialize)]
struct Status {
    schema: i64,
    vec_version: String,
    agents: usize,
    discord_token_saved: bool,
}

#[tauri::command]
fn app_status(state: State<'_, AppState>) -> CmdResult<Status> {
    let s = store(&state);
    Ok(Status {
        schema: s.schema_version().map_err(err)?,
        vec_version: s.vec_version().map_err(err)?,
        agents: s.list_agents().map_err(err)?.len(),
        discord_token_saved: keychain(DISCORD_KEY)?.get_password().is_ok(),
    })
}

#[derive(Serialize)]
struct AgentView {
    name: String,
    transport: String,
    endpoint: String,
    description: String,
}

#[tauri::command]
fn list_agents(state: State<'_, AppState>) -> CmdResult<Vec<AgentView>> {
    Ok(store(&state)
        .list_agents()
        .map_err(err)?
        .into_iter()
        .map(|a| {
            let description = a
                .card_json
                .as_deref()
                .and_then(|j| serde_json::from_str::<serde_json::Value>(j).ok())
                .and_then(|v| v.get("description").and_then(|d| d.as_str()).map(str::to_owned))
                .unwrap_or_default();
            AgentView { name: a.name, transport: a.transport, endpoint: a.endpoint, description }
        })
        .collect())
}

fn credential_for(row: &AgentRow) -> Credential {
    let Some(r) = &row.credential_ref else { return Credential::None };
    let Ok(secret) = keychain(r).and_then(|e| e.get_password().map_err(err)) else { return Credential::None };
    match secret.split_once('\n') {
        Some((name, value)) => Credential::Header { name: name.to_owned(), value: value.to_owned() },
        None => Credential::None,
    }
}

#[tauri::command]
async fn add_a2a_agent(
    state: State<'_, AppState>,
    name: String,
    url: String,
    header_name: Option<String>,
    header_value: Option<String>,
) -> CmdResult<String> {
    let name = name.trim().to_owned();
    if name.is_empty() || !url.starts_with("http") {
        return Err("名称不能为空，URL 需以 http(s) 开头".into());
    }
    let cred = match (header_name.filter(|s| !s.is_empty()), header_value.filter(|s| !s.is_empty())) {
        (Some(n), Some(v)) => Credential::Header { name: n, value: v },
        _ => Credential::None,
    };
    let agent = RemoteAgent::connect(&url, cred.clone()).await.map_err(err)?;
    let credential_ref = match &cred {
        Credential::Header { name: h, value } => {
            let account = format!("agent/{name}");
            keychain(&account)?.set_password(&format!("{h}\n{value}")).map_err(err)?;
            Some(account)
        }
        Credential::None => None,
    };
    let card_name = agent.card.name.clone();
    let row = AgentRow {
        id: uuid::Uuid::new_v4().to_string(),
        name: name.clone(),
        transport: "a2a".into(),
        endpoint: url,
        credential_ref,
        card_json: serde_json::to_string(&agent.card).ok(),
    };
    store(&state).upsert_agent(&row).map_err(err)?;
    state.agents.lock().await.insert(name, Arc::new(agent));
    Ok(card_name)
}

#[tauri::command]
async fn remove_agent(state: State<'_, AppState>, name: String) -> CmdResult<()> {
    let removed = {
        let mut s = store(&state);
        let cref = s.list_agents().map_err(err)?.into_iter().find(|a| a.name == name).and_then(|a| a.credential_ref);
        if let Some(c) = cref {
            let _ = keychain(&c).and_then(|e| e.delete_credential().map_err(err));
        }
        s.delete_agent(&name).map_err(err)?
    };
    state.agents.lock().await.remove(&name);
    if removed { Ok(()) } else { Err("未找到该 agent".into()) }
}

#[derive(Clone, Serialize)]
struct AgentEventMsg {
    agent: String,
    event: AgentEvent,
}

#[tauri::command]
async fn prompt_agent(
    app: AppHandle,
    state: State<'_, AppState>,
    name: String,
    context_id: String,
    text: String,
) -> CmdResult<()> {
    let agent = {
        let mut cache = state.agents.lock().await;
        match cache.get(&name) {
            Some(a) => a.clone(),
            None => {
                let row = store(&state).list_agents().map_err(err)?.into_iter().find(|a| a.name == name).ok_or("未找到该 agent")?;
                let a = Arc::new(RemoteAgent::connect(&row.endpoint, credential_for(&row)).await.map_err(err)?);
                cache.insert(name.clone(), a.clone());
                a
            }
        }
    };
    let mut events = agent.prompt(&context_id, &text).await.map_err(err)?;
    tokio::spawn(async move {
        while let Some(event) = events.next().await {
            let _ = app.emit("agent-event", AgentEventMsg { agent: name.clone(), event });
        }
    });
    Ok(())
}

#[tauri::command]
fn save_discord_token(token: String) -> CmdResult<()> {
    let token = token.trim();
    if token.len() < 50 {
        return Err("token 看起来不完整".into());
    }
    keychain(DISCORD_KEY)?.set_password(token).map_err(err)
}

#[tauri::command]
async fn discord_check() -> CmdResult<String> {
    let token = keychain(DISCORD_KEY)?.get_password().map_err(|_| "还没有保存 Discord token".to_owned())?;
    let r = disco_discord::gateway_check(token, Duration::from_secs(20)).await?;
    Ok(format!("已连接：{}，所在服务器 {} 个，READY 用时 {} ms", r.bot_name, r.guilds, r.ready_after.as_millis()))
}

pub fn run() {
    disco_discord::install_crypto();
    tauri::Builder::default()
        .setup(|app| {
            let dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&dir)?;
            let store = Store::open(dir.join("disco.db"))?;
            app.manage(AppState { store: Mutex::new(store), agents: Default::default() });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            app_status,
            list_agents,
            add_a2a_agent,
            remove_agent,
            prompt_agent,
            save_discord_token,
            discord_check
        ])
        .run(tauri::generate_context!())
        .expect("error while running Disco");
}
