//! One place for everything that waits on the user: tool permissions and questions
//! agents ask (`ask_user`). Items live here, not in the UI, so every surface sees
//! the same list: the app shows a card in the item's own chat and a badge
//! everywhere else, and replays the list after a reload; Discord gets the same
//! card with buttons. Whichever side answers first wins; the other is settled.
use std::collections::HashMap;
use std::sync::Mutex;

use disco_types::PermissionDecision;
use serde::Serialize;
use tauri::Emitter;
use tokio::sync::oneshot;

use crate::Core;

#[derive(Serialize, Clone)]
pub struct Opt { pub id: String, pub label: String, pub allow: bool, #[serde(default)] pub always: bool }

#[derive(Serialize, Clone)]
pub struct Item {
    pub id: String,
    /// `permission` or `question`.
    pub kind: &'static str,
    pub thread: String,
    pub agent: String,
    pub title: String,
    pub options: Vec<Opt>,
    /// The question also takes a free-text answer.
    pub text: bool,
    pub ts: i64,
}

enum Reply {
    Permission { request_id: String },
    Question(oneshot::Sender<String>),
}

struct Entry { item: Item, reply: Reply, discord: Option<(String, String)> }

#[derive(Default)]
pub struct Inbox { items: Mutex<HashMap<String, Entry>> }

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> { m.lock().unwrap_or_else(|p| p.into_inner()) }

impl Inbox {
    pub fn list(&self) -> Vec<Item> {
        let mut v: Vec<Item> = lock(&self.items).values().map(|e| e.item.clone()).collect();
        v.sort_by_key(|i| i.ts);
        v
    }
}

fn question_item(thread: &str, agent: &str, question: &str, options: Vec<String>, text: bool) -> (Item, oneshot::Sender<String>, oneshot::Receiver<String>) {
    let (tx, rx) = oneshot::channel();
    let item = Item {
        id: uuid::Uuid::new_v4().simple().to_string(), kind: "question", thread: thread.into(), agent: agent.into(), title: question.into(),
        options: options.into_iter().enumerate().map(|(i, l)| Opt { id: i.to_string(), label: l, allow: false, always: false }).collect(),
        text, ts: crate::now(),
    };
    (item, tx, rx)
}

/// What the user picked.
pub struct Answer { pub option: Option<String>, pub text: Option<String> }

impl Core {
    fn inbox_changed(&self) { let _ = self.app.emit("inbox", self.inbox.list()); }

    /// The Discord channel a thread is mirrored to, if any.
    fn thread_channel(&self, thread: &str) -> Option<String> {
        if let Some(a) = thread.strip_prefix("dm:") { return self.agent_channel(a); }
        thread.strip_prefix("group:").filter(|id| !id.starts_with("local-")).map(str::to_owned)
    }

    fn inbox_add(&self, item: Item, reply: Reply) { self.inbox_add_once(item, reply, false); }

    /// Add an item; with `once`, nothing is added (false) while the same agent already
    /// has one with this title open. Checked and inserted under one lock.
    fn inbox_add_once(&self, item: Item, reply: Reply, once: bool) -> bool {
        let id = item.id.clone();
        let discord = self.discord_rest().zip(self.thread_channel(&item.thread));
        let qq = crate::qq::target(self, &item.thread);
        let card = format!("{} **{}** {}：{}", if item.kind == "permission" { "🔐" } else { "❓" }, item.agent,
            if item.kind == "permission" { "请求执行" } else { "问你" }, item.title);
        let mut buttons: Vec<(String, String, bool)> = item.options.iter().enumerate()
            .map(|(i, o)| (format!("disco:{id}:{i}"), o.label.clone(), o.allow)).collect();
        if item.kind == "permission" { buttons.push((format!("disco:{id}:x"), "拒绝".into(), false)); }
        let note = if item.text && item.options.is_empty() { "\n（请在 Disco 里回答）" } else { "" };
        {
            let mut items = lock(&self.inbox.items);
            if once && items.values().any(|e| e.item.agent == item.agent && e.item.title == item.title) { return false; }
            items.insert(id.clone(), Entry { item, reply, discord: None });
        }
        self.inbox_changed();
        // QQ can't edit a sent card: its buttons grey out once one is pressed.
        if let Some((rest, openid)) = qq {
            let (text, buttons) = (format!("{card}{note}"), buttons.clone());
            tauri::async_runtime::spawn(async move { let _ = crate::qq::post_card(&rest, &openid, &text, &buttons).await; });
        }
        if let Some((rest, ch)) = discord {
            let inbox = self.inbox.clone();
            tauri::async_runtime::spawn(async move {
                if let Ok(msg) = rest.post_buttons(&ch, &format!("{card}{note}"), &buttons).await {
                    // Answered before Discord even got the card: settle it right away.
                    let late = match lock(&inbox.items).get_mut(&id) {
                        Some(e) => { e.discord = Some((ch.clone(), msg.clone())); false }
                        None => true,
                    };
                    if late { let _ = rest.settle_message(&ch, &msg, &format!("{card}\n（已处理）")).await; }
                }
            });
        }
        true
    }

    /// An agent asked for permission (ACP `session/request_permission`).
    pub(crate) fn inbox_permission(&self, thread: &str, agent: &str, request_id: &str, title: &str, options: &[disco_types::PermissionChoice]) {
        let item = Item {
            id: uuid::Uuid::new_v4().simple().to_string(), kind: "permission", thread: thread.into(), agent: agent.into(),
            title: if title.is_empty() { "工具调用".into() } else { title.into() },
            options: options.iter().map(|o| Opt { id: o.id.clone(), label: o.label.clone(), allow: o.allow, always: o.always }).collect(),
            text: false, ts: crate::now(),
        };
        self.inbox_add(item, Reply::Permission { request_id: request_id.into() });
    }

    /// An agent's question for the user: its item id, and the answer (an error if
    /// it is withdrawn, e.g. the user stopped everything).
    pub(crate) fn inbox_question(&self, thread: &str, agent: &str, question: &str, options: Vec<String>, text: bool) -> (String, oneshot::Receiver<String>) {
        let (item, tx, rx) = question_item(thread, agent, question, options, text);
        let id = item.id.clone();
        self.inbox_add(item, Reply::Question(tx));
        (id, rx)
    }

    /// Like `inbox_question`, but None while `agent` already has this question open
    /// (repeated failures must not stack identical cards).
    pub(crate) fn inbox_question_once(&self, thread: &str, agent: &str, question: &str, options: Vec<String>) -> Option<oneshot::Receiver<String>> {
        let (item, tx, rx) = question_item(thread, agent, question, options, false);
        self.inbox_add_once(item, Reply::Question(tx), true).then_some(rx)
    }

    /// Take one item back (nobody waits on it any more, e.g. `ask_user` timed out).
    pub(crate) fn inbox_withdraw(&self, id: &str, why: &str) {
        let Some(e) = lock(&self.inbox.items).remove(id) else { return };
        self.inbox_changed();
        self.settle_discord(vec![e], why);
    }

    /// Drop the permission prompts a finished turn left open, and cancel them on
    /// the agent's side so nothing keeps waiting on them.
    pub(crate) async fn inbox_expire(&self, request_ids: &[String]) {
        if request_ids.is_empty() { return; }
        let gone: Vec<Entry> = {
            let mut items = lock(&self.inbox.items);
            let ids: Vec<String> = items.iter()
                .filter(|(_, e)| matches!(&e.reply, Reply::Permission { request_id } if request_ids.contains(request_id)))
                .map(|(k, _)| k.clone()).collect();
            ids.iter().filter_map(|k| items.remove(k)).collect()
        };
        if gone.is_empty() { return; }
        self.inbox_changed();
        for e in &gone {
            if let Reply::Permission { request_id } = &e.reply { self.hub.decide(&e.item.agent, request_id, PermissionDecision::Cancel).await; }
        }
        self.settle_discord(gone, "（已失效：这一轮已结束）");
    }

    /// Withdraw everything (kill switch): questions resolve as unanswered.
    pub(crate) fn inbox_clear(&self) {
        let gone: Vec<Entry> = lock(&self.inbox.items).drain().map(|(_, e)| e).collect();
        if gone.is_empty() { return; }
        self.inbox_changed();
        self.settle_discord(gone, "（已撤回：全部停止）");
    }

    fn settle_discord(&self, gone: Vec<Entry>, why: &str) {
        for e in gone { self.settle_card(e.discord, format!("~~{}~~ {why}", e.item.title)); }
    }

    /// Replace a Discord card's buttons with `text`.
    fn settle_card(&self, discord: Option<(String, String)>, text: String) {
        let (Some((ch, msg)), Some(rest)) = (discord, self.discord_rest()) else { return };
        tauri::async_runtime::spawn(async move { let _ = rest.settle_message(&ch, &msg, &text).await; });
    }

    /// Answer an item. `via` names the surface for the record ("Disco" / "Discord").
    /// Returns the line describing the outcome.
    pub(crate) async fn inbox_answer(&self, id: &str, ans: Answer, via: &str) -> Result<String, String> {
        let Some(e) = ({
            let mut items = lock(&self.inbox.items);
            // An empty answer to a question leaves it open instead of losing it.
            let empty = |e: &Entry| e.item.kind == "question" && ans.option.as_ref().is_none_or(|o| !e.item.options.iter().any(|x| x.id == *o))
                && ans.text.as_deref().is_none_or(|t| t.trim().is_empty());
            if items.get(id).is_some_and(empty) { return Err("回答不能为空".into()); }
            items.remove(id)
        }) else { return Err("这一项已经处理过或已失效".into()) };
        self.inbox_changed();
        let Entry { item, reply, discord } = e;
        let label = ans.option.as_ref().and_then(|o| item.options.iter().find(|x| x.id == *o)).map(|o| o.label.clone());
        let line = match reply {
            Reply::Permission { request_id } => {
                let d = match &ans.option { Some(o) if label.is_some() => PermissionDecision::Select { option_id: o.clone() }, _ => PermissionDecision::Cancel };
                if !self.hub.decide(&item.agent, &request_id, d).await {
                    self.settle_card(discord, format!("~~{}~~ （已失效：这一轮已结束）", item.title));
                    return Err("agent 已结束这一轮，这个请求失效了".into());
                }
                format!("{}：{}", label.as_deref().unwrap_or("已拒绝"), item.title)
            }
            Reply::Question(tx) => {
                // Validated above: a question only gets here with a real answer.
                let answer = label.clone().or(ans.text.map(|t| t.trim().to_owned())).filter(|t| !t.is_empty()).unwrap_or_default();
                if tx.send(answer.clone()).is_err() {
                    self.settle_card(discord, format!("~~{}~~ （已失效：agent 不再等待）", item.title));
                    return Err("agent 已不再等待这个回答".into());
                }
                self.post_message(&item.thread, "me", &format!("回答 {}：{answer}", item.agent));
                answer
            }
        };
        let _ = self.db().audit(via, Some(&item.agent), &format!("inbox.{}", item.kind), &line);
        if via != "Discord" { self.settle_card(discord, format!("~~{}~~ → {line}（在 {via} 中处理）", item.title)); }
        Ok(line)
    }

    /// The option a button picked (`pick` = its index; `x` or anything else = none, i.e. deny).
    pub(crate) fn inbox_option(&self, id: &str, pick: &str) -> Option<String> {
        lock(&self.inbox.items).get(id).and_then(|e| pick.parse::<usize>().ok().and_then(|i| e.item.options.get(i)).map(|o| o.id.clone()))
    }

    /// A button press from Discord (`custom_id` = `disco:<item>:<option index | x>`).
    pub(crate) async fn inbox_press(&self, author_id: &str, author: &str, interaction: (String, String, String)) {
        let (iid, token, custom) = interaction;
        let Some(rest) = self.discord_rest() else { return };
        let mut parts = custom.splitn(3, ':');
        let (Some("disco"), Some(id), Some(pick)) = (parts.next(), parts.next(), parts.next()) else { return };
        if !self.allowed(author_id) {
            let _ = rest.interaction_reply(&iid, &token, false, "你没有权限操作这个 Disco。").await;
            return;
        }
        let option = self.inbox_option(id, pick);
        let title = lock(&self.inbox.items).get(id).map(|e| e.item.title.clone()).unwrap_or_default();
        match self.inbox_answer(id, Answer { option, text: None }, "Discord").await {
            Ok(line) => { let _ = rest.interaction_reply(&iid, &token, true, &format!("~~{title}~~ → {line}（{author}）")).await; }
            Err(e) => { let _ = rest.interaction_reply(&iid, &token, false, &e).await; }
        }
    }
}

