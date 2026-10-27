//! End-to-end self test of the real app (debug builds only): `DISCO_SELFTEST=<mock ACP agent>`
//! runs every main flow through the same command functions the UI calls, prints one line
//! per check and exits 0 (all passed) or 1. A step that hangs (a deadlock) fails on its timeout.
use std::sync::Arc;
use std::time::Duration;

use tauri::{AppHandle, Manager};

use crate::{inbox, Core};

async fn within<T>(secs: u64, f: impl std::future::Future<Output = T>) -> Result<T, String> {
    tokio::time::timeout(Duration::from_secs(secs), f).await.map_err(|_| format!("超时 {secs}s（可能死锁）"))
}

fn last_from(core: &Core, thread: &str, author: &str) -> Option<String> {
    core.db().messages(thread, 200).ok()?.into_iter().rev().find(|m| m.author == author).map(|m| m.body)
}

async fn wait_msg(core: &Core, thread: &str, author: &str, before: usize, secs: u64) -> Result<String, String> {
    within(secs, async {
        loop {
            let n = core.db().messages(thread, 500).map(|v| v.iter().filter(|m| m.author == author).count()).unwrap_or(0);
            if n > before { return last_from(core, thread, author).unwrap_or_default(); }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }).await
}

fn count(core: &Core, thread: &str, author: &str) -> usize {
    core.db().messages(thread, 500).map(|v| v.iter().filter(|m| m.author == author).count()).unwrap_or(0)
}

pub fn spawn(app: AppHandle, mock: String) {
    tauri::async_runtime::spawn(async move {
        let core: Arc<Core> = app.state::<Arc<Core>>().inner().clone();
        // Any panic is a failure, not a hang.
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| { prev(info); println!("SELFTEST PANIC"); std::process::exit(101); }));
        // Approve every permission the mock asks for, like a user would.
        let c2 = core.clone();
        tauri::async_runtime::spawn(async move {
            loop {
                for it in c2.inbox.list().into_iter().filter(|i| i.kind == "permission") {
                    let opt = it.options.iter().find(|o| o.allow).map(|o| o.id.clone());
                    let _ = c2.inbox_answer(&it.id, inbox::Answer { option: opt, text: None }, "selftest").await;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        });
        let mut fails = 0;
        macro_rules! check {
            ($name:expr, $e:expr) => {{
                let r: Result<String, String> = async { $e }.await;
                match &r { Ok(d) => println!("PASS {} {}", $name, d), Err(e) => { fails += 1; println!("FAIL {} {}", $name, e) } }
                r
            }};
        }
        let st = || app.state::<Arc<Core>>();
        let _ = check!("overview(空)", within(5, crate::overview(st())).await.and_then(|r| r.map(|o| format!("{} agents", o.agents.len()))));
        let spec = |name: &str| crate::NewAgent { template: "command".into(), name: name.into(), url: None, header_name: None, header_value: None,
            command: Some(format!("\"{mock}\"")), cwd: None, count: None, worktree: None, conda: None, model: None };
        let _ = check!("add_agent m1", within(20, crate::add_agent(st(), spec("m1"))).await.and_then(|r| r.map(|v| v.join(","))));
        let _ = check!("add_agent m2", within(20, crate::add_agent(st(), spec("m2"))).await.and_then(|r| r.map(|v| v.join(","))));
        let _ = check!("overview(2)", within(5, crate::overview(st())).await.and_then(|r| r.and_then(|o| if o.agents.len() == 2 { Ok("2 agents".into()) } else { Err(format!("{} agents", o.agents.len())) })));

        // 1:1 chat
        let n = count(&core, "dm:m1", "m1");
        let _ = check!("dm send", crate::send(st(), "dm:m1".into(), "hi".into()).map(|_| String::new()));
        let _ = check!("dm reply", wait_msg(&core, "dm:m1", "m1", n, 30).await.and_then(|b| if b.contains("hello from mock") { Ok(b) } else { Err(b) }));
        let _ = check!("dm 再发一遍", { let n = count(&core, "dm:m1", "m1"); crate::send(st(), "dm:m1".into(), "hi".into())?; wait_msg(&core, "dm:m1", "m1", n, 30).await });

        // memory switch
        let _ = check!("memory off", crate::set_memory(st(), "dm:m1".into(), false).map(|_| String::new()));
        let _ = check!("overview memory_off", within(5, crate::overview(st())).await.and_then(|r| r.and_then(|o| if o.memory_off == vec!["dm:m1".to_string()] { Ok(String::new()) } else { Err(format!("{:?}", o.memory_off)) })));
        let _ = check!("记住(关)", { let n = count(&core, "dm:m1", "system"); crate::send(st(), "dm:m1".into(), "记住 不该保存".into())?; wait_msg(&core, "dm:m1", "system", n, 5).await
            .and_then(|b| if b.contains("记忆已关闭") && !core.memory_list().iter().any(|m| m.text.contains("不该保存")) { Ok(b) } else { Err(b) }) });
        let _ = check!("memory on", crate::set_memory(st(), "dm:m1".into(), true).map(|_| String::new()));
        let _ = check!("记住(开)", { let n = count(&core, "dm:m1", "system"); crate::send(st(), "dm:m1".into(), "记住 喜欢简洁".into())?; wait_msg(&core, "dm:m1", "system", n, 5).await?;
            if core.memory_list().iter().any(|m| m.text == "喜欢简洁") { Ok(String::new()) } else { Err("没存上".into()) } });
        let key = core.memory_list().into_iter().find(|m| m.text == "喜欢简洁").map(|m| m.key).unwrap_or_default();
        let _ = check!("forget+restore", { crate::forget(st(), key.clone())?; crate::memory_restore(st(), key)?;
            if core.memory_list().iter().any(|m| m.text == "喜欢简洁") { Ok(String::new()) } else { Err("没恢复".into()) } });

        // groups
        let gid = check!("save_group", within(10, crate::save_group(st(), None, None, "dev".into(), vec!["m1".into(), "m2".into()], Some("m1".into()))).await.and_then(|r| r)).unwrap_or_default();
        let gt = format!("group:{gid}");
        let _ = check!("群 不@ → 组长", { let n = count(&core, &gt, "m1"); crate::send(st(), gt.clone(), "大家好".into())?; wait_msg(&core, &gt, "m1", n, 30).await });
        let _ = check!("群 @m2", { let n = count(&core, &gt, "m2"); crate::send(st(), gt.clone(), "@m2 你好".into())?; wait_msg(&core, &gt, "m2", n, 30).await });
        let _ = check!("群 @m1 @m2 并发", { let (a, b) = (count(&core, &gt, "m1"), count(&core, &gt, "m2")); crate::send(st(), gt.clone(), "@m1 @m2 一起".into())?;
            wait_msg(&core, &gt, "m1", a, 30).await?; wait_msg(&core, &gt, "m2", b, 30).await });
        let _ = check!("/help", { let n = count(&core, &gt, "system"); crate::send(st(), gt.clone(), "/help".into())?; wait_msg(&core, &gt, "system", n, 5).await
            .and_then(|b| if b.starts_with("群规") { Ok(String::new()) } else { Err(b) }) });
        // Someone else joins (as a Discord person would appear): no-@ messages wake nobody.
        core.post_message(&gt, "Amy (Discord)", "@m1 我是 Amy");
        let _ = check!("roster", within(5, crate::overview(st())).await.and_then(|r| r.and_then(|o| {
            let g = o.groups.iter().find(|g| g.id == gid).ok_or("没群")?;
            if g.people == vec!["Amy".to_string()] { Ok(String::new()) } else { Err(format!("{:?}", g.people)) } })));
        let _ = check!("多人群 不@ 安静", { let n = count(&core, &gt, "m1"); crate::send(st(), gt.clone(), "随便聊聊".into())?;
            tokio::time::sleep(Duration::from_secs(3)).await; if count(&core, &gt, "m1") == n { Ok(String::new()) } else { Err("组长插话了".into()) } });
        let _ = check!("多人群 回复以@开头", { let n = count(&core, &gt, "m1"); crate::send(st(), gt.clone(), "@m1 在吗".into())?; wait_msg(&core, &gt, "m1", n, 30).await
            .and_then(|b| if b.starts_with("@") { Ok(b) } else { Err(b) }) });
        let _ = check!("delete+restore group", {
            crate::delete_group(st(), gid.clone())?;
            crate::restore_group(st(), gid.clone(), String::new(), "dev".into(), vec!["m1".into(), "m2".into()], Some("m1".into()))?;
            within(5, crate::overview(st())).await?.and_then(|o| if o.groups.iter().any(|g| g.id == gid) { Ok(String::new()) } else { Err("没恢复".into()) }) });

        // Discord: what the gateway delivers, fed through the same handler.
        *core.owners.lock().unwrap() = vec!["owner1".into()];
        let _ = core.db().upsert_group(&crate::GroupRow { id: "chan1".into(), guild_id: "g1".into(), name: "dc".into(), lead_agent: Some("m1".into()), webhook_ref: None, members: vec!["m1".into()] });
        let _ = core.db().set_setting(&format!("{}m1", crate::SET_AGENT_CH), "chanA");
        let msg = |ch: &str, id: &str, name: &str, text: &str| disco_discord::Incoming { guild_id: "g1".into(), channel_id: ch.into(), author_id: id.into(), author: name.into(), content: text.into(), webhook_id: None, bot: false, press: None };
        let _ = check!("Discord 私聊频道 主人", { let n = count(&core, "dm:m1", "m1"); core.on_incoming(msg("chanA", "owner1", "Jian", "hi")); wait_msg(&core, "dm:m1", "m1", n, 30).await });
        let _ = check!("Discord 私聊频道 陌生人被拦", { let n = count(&core, "dm:m1", "Bob (Discord)"); core.on_incoming(msg("chanA", "u9", "Bob", "hi")); tokio::time::sleep(Duration::from_secs(1)).await;
            if count(&core, "dm:m1", "Bob (Discord)") == n { Ok(String::new()) } else { Err("陌生人进来了".into()) } });
        let _ = check!("Discord 群 主人不@ → 组长", { let n = count(&core, "group:chan1", "m1"); core.on_incoming(msg("chan1", "owner1", "Jian", "大家好")); wait_msg(&core, "group:chan1", "m1", n, 30).await });
        let _ = check!("Discord 群 朋友 @m1", { let n = count(&core, "group:chan1", "m1"); core.on_incoming(msg("chan1", "u9", "Bob", "@m1 你好")); wait_msg(&core, "group:chan1", "m1", n, 30).await
            .and_then(|b| if b.starts_with("@Bob") { Ok(b) } else { Err(b) }) });
        let _ = check!("Discord 别的 bot 被忽略", { let n = core.db().messages("group:chan1", 500).map(|v| v.len()).unwrap_or(0);
            core.on_incoming(disco_discord::Incoming { bot: true, ..msg("chan1", "b1", "MEE6", "level up!") }); tokio::time::sleep(Duration::from_secs(1)).await;
            if core.db().messages("group:chan1", 500).map(|v| v.len()).unwrap_or(0) == n { Ok(String::new()) } else { Err("记下来了".into()) } });
        let _ = check!("Discord 另一个 Disco 转发的人", { core.on_incoming(disco_discord::Incoming { bot: true, ..msg("chan1", "b2", "TheirBot", "**Amy**：大家好") });
            wait_msg(&core, "group:chan1", "Amy (Discord)", 0, 5).await });
        let _ = check!("Discord 另一个 Disco 的 agent @m1", { let n = count(&core, "group:chan1", "m1");
            core.on_incoming(disco_discord::Incoming { webhook_id: Some("hook9".into()), ..msg("chan1", "hook9", "coder", "@m1 帮我看看") }); wait_msg(&core, "group:chan1", "m1", n, 30).await });
        // v1.5.12 routing
        let _ = check!("Discord 别的 Disco 的 webhook 叫 me/m1 也不冒充", {
            let me = count(&core, "group:chan1", "me");
            for name in ["me", "m1"] { core.on_incoming(disco_discord::Incoming { webhook_id: Some("hook9".into()), ..msg("chan1", "hook9", name, "你好") }); }
            wait_msg(&core, "group:chan1", &format!("m1{}", crate::FOREIGN_TAG), 0, 5).await?;
            wait_msg(&core, "group:chan1", &format!("me{}", crate::FOREIGN_TAG), 0, 5).await?;
            if count(&core, "group:chan1", "me") == me { Ok(String::new()) } else { Err("被当成了主人".into()) } });
        let _ = check!("Discord @m1中文 也能叫到", { let n = count(&core, "group:chan1", "m1"); core.on_incoming(msg("chan1", "owner1", "Jian", "@m1帮我看看")); wait_msg(&core, "group:chan1", "m1", n, 30).await });
        let _ = check!("保留名称不能用", { let r = within(5, crate::add_agent(st(), spec("System"))).await?; if r.is_err() { Ok(String::new()) } else { Err("System 被接受了".into()) } });
        // end v1.5.12 routing

        // Older model API (Hermes) and ACP sign-in, with the raw mock agent.
        let raw = mock.replace("mock-acp-agent", "mock-raw-agent");
        let raw_spec = |name: &str, arg: &str| crate::NewAgent { command: Some(format!("\"{raw}\" {arg}")), ..spec(name) };
        let _ = check!("旧版模型接口 列出模型", { within(20, crate::add_agent(st(), raw_spec("r1", ""))).await??;
            let list = within(20, crate::agent_configs(st(), "r1".into())).await??;
            let m = list.iter().find(|c| c.category == "model").ok_or(format!("没有模型：{}", list.len()))?;
            if m.options.len() == 2 { Ok(m.current.clone()) } else { Err(format!("{:?}", m.options)) } });
        let _ = check!("旧版模型接口 切换模型", { within(10, crate::set_agent_config(st(), "r1".into(), "model".into(), "m-smart".into())).await??;
            let n = count(&core, "dm:r1", "r1"); crate::send(st(), "dm:r1".into(), "hi".into())?;
            wait_msg(&core, "dm:r1", "r1", n, 20).await.and_then(|b| if b == "raw m-smart" { Ok(b) } else { Err(b) }) });
        let _ = check!("模型选择被保存", {
            let row = core.agent("r1")?;
            if row.card_json.as_deref().is_some_and(|j| j.contains("m-smart")) { Ok(String::new()) } else { Err(format!("{:?}", row.card_json)) } });
        let _ = check!("需要登录 → 卡片", { within(20, crate::add_agent(st(), raw_spec("r2", "auth"))).await??;
            let n = count(&core, "dm:r2", "system"); crate::send(st(), "dm:r2".into(), "hi".into())?;
            let line = wait_msg(&core, "dm:r2", "system", n, 20).await?;
            let card = within(10, async { loop {
                if let Some(i) = core.inbox.list().into_iter().find(|i| i.agent == "r2") { return i; }
                tokio::time::sleep(Duration::from_millis(100)).await;
            } }).await?;
            let labels: Vec<String> = card.options.iter().map(|o| o.label.clone()).collect();
            if !line.contains("💡") || labels != ["Mock Key", "在终端登录：Mock Terminal"] { return Err(format!("{line} {labels:?}")); }
            let n = count(&core, "dm:r2", "system");
            let opt = card.options[0].id.clone();
            core.inbox_answer(&card.id, inbox::Answer { option: Some(opt), text: None }, "selftest").await?;
            wait_msg(&core, "dm:r2", "system", n, 10).await.and_then(|b| if b.contains("已登录") { Ok(b) } else { Err(b) }) });
        let _ = check!("登录后可以聊", { let n = count(&core, "dm:r2", "r2"); crate::send(st(), "dm:r2".into(), "hi".into())?;
            wait_msg(&core, "dm:r2", "r2", n, 20).await });
        for r in ["r1", "r2"] { let _ = within(10, crate::remove_agent(st(), r.into(), Some(true))).await; }

        // the rest of the surface
        let _ = check!("messages", crate::messages(st(), gt.clone()).map(|v| format!("{} 条", v.len())));
        let _ = check!("待办 增/完成/删", {
            let id = crate::todo_new(st(), gt.clone(), "写测试".into())?;
            let has = |done: bool| crate::space(st(), gt.clone()).map(|s| s.todos.iter().any(|t| t.id == id && t.done == done));
            if !has(false)? { return Err("新增后看不到".into()); }
            crate::todo_set(st(), gt.clone(), id.clone(), true)?;
            if !has(true)? { return Err("没标记完成".into()); }
            crate::todo_remove(st(), gt.clone(), id.clone())?;
            if crate::space(st(), gt.clone())?.todos.iter().any(|t| t.id == id) { return Err("删不掉".into()); }
            Ok(String::new()) });
        let _ = check!("群文档", { crate::doc_save(st(), gt.clone(), "# 目标\n".into())?;
            let d = crate::space(st(), gt.clone())?.doc; if d.contains("目标") { Ok(String::new()) } else { Err(d) } });
        let _ = check!("agent_commands", within(5, crate::agent_commands(st(), "m1".into())).await.and_then(|r| r.map(|v| format!("{} 个", v.len()))));
        let _ = check!("rename", within(20, crate::rename_agent(st(), "m2".into(), "m3".into())).await.and_then(|r| r).map(|_| String::new()));
        let _ = check!("改名后群聊", { let n = count(&core, &gt, "m3"); crate::send(st(), gt.clone(), "@m3 改名了".into())?; wait_msg(&core, &gt, "m3", n, 30).await });
        let _ = check!("重启 agent 后还能聊", { within(10, crate::restart_agent(st(), "m1".into())).await??;
            let n = count(&core, "dm:m1", "m1"); crate::send(st(), "dm:m1".into(), "重启后".into())?; wait_msg(&core, "dm:m1", "m1", n, 30).await });
        let _ = check!("stop_all", within(10, crate::stop_all(st())).await.and_then(|r| r).map(|n| format!("{n}")));
        crate::resume(st());
        let _ = check!("恢复后私聊", { let n = count(&core, "dm:m1", "m1"); crate::send(st(), "dm:m1".into(), "回来了".into())?; wait_msg(&core, "dm:m1", "m1", n, 30).await });
        let _ = check!("OpenAI 兼容 agent", {
            let base = mock_openai().await?;
            let o = crate::NewAgent { template: "openai".into(), name: "o1".into(), url: Some(format!("{base}/chat/completions")), header_name: None, header_value: None,
                command: None, cwd: None, count: None, worktree: None, conda: None, model: None };
            within(10, crate::add_agent(st(), o)).await??;
            let n = count(&core, "dm:o1", "o1");
            crate::send(st(), "dm:o1".into(), "记一个待办".into())?;
            let b = wait_msg(&core, "dm:o1", "o1", n, 30).await?;
            if !b.contains("好了") { return Err(b); }
            if !crate::mcp::todos(&core, "dm:o1").iter().any(|t| t.text == "来自模型") { return Err("工具调用没生效".into()); }
            Ok(b) });
        let _ = check!("对方群可回话", {
            let pc = |ctx: &str| { let _ = core.db().set_setting(&format!("{}st", crate::share::PEER_CTX), ctx); core.peer_channel("st") };
            if pc("group:1234567890").as_deref() != Some("1234567890") { return Err("认不出 Discord 群".into()); }
            if pc("team:group:1234567890:m1").as_deref() != Some("1234567890") || pc("ask:m1:group:1234567890").as_deref() != Some("1234567890") { return Err("认不出团队/委托里的群".into()); }
            if pc("group:local-x").is_some() || pc("dm:x").is_some() || pc("ask:m1:dm:x").is_some() { return Err("本地群/私聊不该能回话".into()); }
            Ok(String::new()) });
        let _ = check!("对方群需你和 Bot 都在", {
            let _ = core.db().set_setting(&format!("{}st", crate::share::PEER_CTX), "group:1234567890");
            let m = within(10, core.peer_check("st")).await?.unwrap_or_default();
            if m.is_empty() { return Err("没连 Discord 却被当成可以说话".into()); }
            if core.peer_say("st", "hi").await.is_ok() { return Err("没确认就发出去了".into()); }
            Ok(m) });
        // Sharing end to end over loopback: invite → the guest's remote agent → our m1 answers.
        let _ = check!("共享 邀请码接入后能聊", {
            let code = crate::share::create_share(st(), "m1".into(), "小王".into(), "chat".into(), 0)?;
            let guest = within(20, crate::share::accept_invite(st(), code, Some("guest1".into()))).await??;
            let n = count(&core, "dm:guest1", "guest1"); crate::send(st(), "dm:guest1".into(), "你好".into())?;
            wait_msg(&core, "dm:guest1", "guest1", n, 40).await.and_then(|b| if b.contains("hello from mock") { Ok(guest) } else { Err(b) }) });
        let _ = check!("共享 撤销后被拒", {
            let id = core.shares().into_iter().find(|(_, s)| s.agent == "m1").map(|(id, _)| id).ok_or("找不到共享")?;
            crate::share::revoke_share(st(), id);
            let n = count(&core, "dm:guest1", "system"); crate::send(st(), "dm:guest1".into(), "还在吗".into())?;
            wait_msg(&core, "dm:guest1", "system", n, 30).await.and_then(|b| if b.contains("撤销") { Ok(b) } else { Err(b) }) });
        // v1.5.13 share
        let _ = check!("共享 不泄露存在与用户名", {
            crate::share::create_share(st(), "m1".into(), "小李".into(), "chat".into(), 0)?;
            let id = core.shares().into_iter().find(|(_, s)| s.guest == "小李").map(|(id, _)| id).ok_or("找不到共享")?;
            let (base, http) = (core.share_base()?, crate::openai::client(&[]));
            let body = serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "SendMessage", "params": {} });
            let mut seen = Vec::new();
            for a in [id.as_str(), "000000000000"] {
                let r = within(10, http.post(format!("{base}/a/{a}/jsonrpc")).bearer_auth("wrong").json(&body).send()).await?.map_err(|e| e.to_string())?;
                seen.push((r.status(), r.text().await.map_err(|e| e.to_string())?));
            }
            let card = within(10, http.get(format!("{base}/a/{id}/.well-known/agent-card.json")).send()).await?.map_err(|e| e.to_string())?.text().await.map_err(|e| e.to_string())?;
            crate::share::revoke_share(st(), id);
            if seen[0] != seen[1] { return Err(format!("真假共享的拒绝不一样：{seen:?}")); }
            if card.contains(&format!("{} 通过", crate::share::owner_name())) { return Err(format!("名片带了用户名：{card}")); }
            Ok(seen[0].0.to_string()) });
        let _ = within(10, crate::remove_agent(st(), "guest1".into(), Some(true))).await;
        let _ = check!("发文件到共享空间", {
            let f = std::env::temp_dir().join("disco-selftest.txt"); std::fs::write(&f, "hello").map_err(|e| e.to_string())?;
            within(10, crate::send_file(st(), gt.clone(), f.display().to_string(), None)).await? });
        let _ = check!("/team 分工到完成", {
            let tid = within(10, crate::save_group(st(), None, None, "team".into(), vec!["m1".into(), "m3".into()], Some("m1".into()))).await??;
            let tt = format!("group:{tid}"); crate::send(st(), tt.clone(), "/team 写一个加法函数".into())?;
            within(120, async { loop {
                if let Some(s) = core.team_state(&tt).filter(|s| s.stage == "done" || s.stage == "failed") { return s; }
                tokio::time::sleep(Duration::from_millis(200)).await;
            } }).await.and_then(|s| if s.stage == "done" { Ok(format!("{} 人", s.roles.len())) } else { Err(s.stage.to_string()) }) });
        let _ = check!("团队黑板文件", {
            let groups = core.db().list_groups().map_err(|e| e.to_string())?;
            let tid = groups.into_iter().find(|g| g.name == "team").map(|g| g.id).ok_or("没有 team 群")?;
            let names: Vec<String> = crate::team_files(st(), format!("group:{tid}")).into_iter().map(|(n, _)| n).collect();
            if names.iter().any(|n| n == "context.md") && names.iter().any(|n| n == "summary.md") { Ok(names.join(",")) } else { Err(format!("{names:?}")) } });
        let bad = crate::NewAgent { command: Some("/nonexistent/agent --acp".into()), ..spec("bad") };
        let _ = check!("自检提示", { within(10, crate::add_agent(st(), bad)).await??; wait_msg(&core, "dm:bad", "system", 0, 10).await });
        let _ = check!("remove_agent", within(10, crate::remove_agent(st(), "bad".into(), Some(true))).await.and_then(|r| r).map(|_| String::new()));
        let _ = check!("overview(结束)", within(5, crate::overview(st())).await.and_then(|r| r.map(|o| format!("{} agents, {} groups", o.agents.len(), o.groups.len()))));
        // v1.5.12 runtime
        // One JSON-RPC envelope to the loopback MCP endpoint, as an agent's proxy would send it.
        async fn mcp_post(core: &Core, token: &str, body: serde_json::Value) -> Result<serde_json::Value, String> {
            let port = *core.hub.mcp_port.get().ok_or("MCP 没启动")?;
            let r = crate::openai::client(&[]).post(format!("http://127.0.0.1:{port}/mcp")).bearer_auth(token).json(&body).send().await.map_err(|e| e.to_string())?;
            if !r.status().is_success() { return Err(format!("HTTP {}", r.status())); }
            r.json().await.map_err(|e| e.to_string())
        }
        let _ = check!("崩溃的 agent 下一条消息自动重启", {
            let n = count(&core, "dm:m1", "m1"); crate::send(st(), "dm:m1".into(), "__die__".into())?; wait_msg(&core, "dm:m1", "m1", n, 30).await?;
            tokio::time::sleep(Duration::from_millis(800)).await; // the mock exits 100 ms after answering
            let n = count(&core, "dm:m1", "m1"); crate::send(st(), "dm:m1".into(), "还在吗".into())?;
            wait_msg(&core, "dm:m1", "m1", n, 30).await.and_then(|b| if b.contains("hello from mock") { Ok(b) } else { Err(b) }) });
        let call = |tool: &str, args: serde_json::Value| serde_json::json!({ "context": gt, "msg": { "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": tool, "arguments": args } } });
        let text = |v: &serde_json::Value| (v["result"]["content"][0]["text"].as_str().unwrap_or_default().to_owned(), v["result"]["isError"] == true);
        let _ = check!("MCP 伪造 context 被拒", {
            let peer = core.hub.mcp_tokens.mint("m1", "peer:st|dm:x");
            let (t, err) = text(&mcp_post(&core, &peer, call("doc_read", serde_json::json!({ "group": gid }))).await?);
            if !err || t.contains("目标") { return Err(format!("朋友会话读到了群文档：{t}")); }
            let list = mcp_post(&core, &peer, serde_json::json!({ "msg": { "jsonrpc": "2.0", "id": 2, "method": "tools/list" } })).await?;
            let names: Vec<&str> = list["result"]["tools"].as_array().into_iter().flatten().filter_map(|t| t["name"].as_str()).collect();
            if names != ["send_file"] { return Err(format!("朋友会话应只有 send_file：{list}")); }
            if mcp_post(&core, "forged", call("doc_read", serde_json::json!({}))).await.is_ok() { return Err("假 token 通过了".into()); }
            let own = core.hub.mcp_tokens.mint("m1", &gt);
            let (t, err) = text(&mcp_post(&core, &own, call("doc_read", serde_json::json!({}))).await?);
            if err || !t.contains("目标") { return Err(format!("群内会话读不到群文档：{t}")); }
            Ok(String::new()) });
        let _ = check!("满载时 ask 不死锁", {
            let held = within(5, core.hub.slots.clone().acquire_many_owned(crate::hub::MAX_CONCURRENT as u32)).await?.map_err(|e| e.to_string())?;
            let tok = core.hub.mcp_tokens.mint("m1", "dm:m1");
            let r = within(40, mcp_post(&core, &tok, call("ask", serde_json::json!({ "agent": "m3", "question": "1+1" })))).await;
            drop(held);
            let (t, err) = text(&r??);
            if err || !t.contains("hello from mock") { Err(t) } else { Ok(t) } });
        // v1.5.13 core
        let _ = check!("ask_user 超时后撤回，迟到的回答报错", {
            let (id, rx) = core.inbox_question("dm:m1", "m1", "超时的问题", vec!["好".into()], false);
            core.inbox_withdraw(&id, "（已超时）");
            if core.inbox.list().iter().any(|i| i.id == id) { return Err("还在收件箱".into()); }
            if rx.await.is_ok() { return Err("撤回后还收到了回答".into()); }
            let (id, rx) = core.inbox_question("dm:m1", "m1", "没人等的问题", vec!["好".into()], false);
            drop(rx);
            match core.inbox_answer(&id, inbox::Answer { option: Some("0".into()), text: None }, "selftest").await {
                Ok(l) => Err(format!("没人等也报成功：{l}")),
                Err(_) => Ok(String::new()),
            } });
        let _ = check!("记住了吗？ 不当成记住", {
            let before = core.memory_list().len();
            let n = count(&core, "dm:m1", "m1"); crate::send(st(), "dm:m1".into(), "记住了吗？".into())?; wait_msg(&core, "dm:m1", "m1", n, 30).await?;
            if core.memory_list().len() == before { Ok(String::new()) } else { Err("被存进了记忆".into()) } });
        // end v1.5.13 core
        // v1.5.13 runtime
        let _ = check!("OpenAI 同一会话两轮排队", {
            let base = mock_openai().await?;
            let o = crate::openai::OpenAi::new(&base, None, "mock-model".into(), crate::openai::client(&[]), None);
            let turn = |t: &'static str| { let o = &o; async move { o.prompt("dm:st", t, &mut |_| {}).await } };
            let (a, b) = within(20, async { tokio::join!(turn("一"), turn("二")) }).await?;
            a?; b?;
            Ok(String::new()) });
        // end v1.5.13 runtime
        // v1.5.14
        let _ = check!("改名带走记忆开关、待办、文档和记忆", {
            within(20, crate::add_agent(st(), spec("m5"))).await??;
            crate::set_memory(st(), "dm:m5".into(), false)?;
            crate::doc_save(st(), "dm:m5".into(), "# 私聊文档\n".into())?;
            core.remember("m5", "改名前的记忆")?;
            let tid = crate::todo_new(st(), "dm:m5".into(), "改名前的待办".into())?;
            within(20, crate::rename_agent(st(), "m5".into(), "m6".into())).await??;
            let off = |t: &str| core.db().setting(&format!("{}{t}", crate::memory::MEM_OFF));
            if off("dm:m6").as_deref() != Some("1") || off("dm:m5").is_some() { return Err(format!("记忆开关没跟过去：{:?} {:?}", off("dm:m6"), off("dm:m5"))); }
            let sp = crate::space(st(), "dm:m6".into())?;
            if !sp.doc.contains("私聊文档") { return Err(format!("文档没跟过去：{}", sp.doc)); }
            if !sp.todos.iter().any(|t| t.id == tid) { return Err("待办没跟过去".into()); }
            if !core.memory_list().iter().any(|m| m.scope == "m6" && m.text == "改名前的记忆") { return Err("记忆没跟过去".into()); }
            Ok(String::new()) });
        let _ = within(10, crate::remove_agent(st(), "m6".into(), Some(true))).await;
        let _ = check!("登录卡片不重复", {
            within(20, crate::add_agent(st(), raw_spec("r3", "auth"))).await??;
            let cards = || core.inbox.list().into_iter().filter(|i| i.agent == "r3").collect::<Vec<_>>();
            let n = count(&core, "dm:r3", "system"); crate::send(st(), "dm:r3".into(), "hi".into())?;
            wait_msg(&core, "dm:r3", "system", n, 20).await?;
            within(10, async { while cards().is_empty() { tokio::time::sleep(Duration::from_millis(100)).await; } }).await?;
            for _ in 0..3 { core.offer_login("dm:r3", "r3"); }
            let n = count(&core, "dm:r3", "system"); crate::send(st(), "dm:r3".into(), "再试".into())?;
            wait_msg(&core, "dm:r3", "system", n, 20).await?;
            tokio::time::sleep(Duration::from_millis(1500)).await;
            let open = cards();
            for c in &open { core.inbox_withdraw(&c.id, "（selftest）"); }
            if open.len() == 1 { Ok(String::new()) } else { Err(format!("{} 张卡片", open.len())) } });
        let _ = within(10, crate::remove_agent(st(), "r3".into(), Some(true))).await;
        // end v1.5.14
        // v1.6.0
        let _ = check!("自定义头像：设置、改名保留、清除", {
            within(20, crate::add_agent(st(), spec("p1"))).await??;
            let png = "data:image/png;base64,iVBORw0KGgo=";
            let pic = |name: &'static str| async move {
                within(5, crate::overview(st())).await?.map(|o| o.agents.into_iter().find(|a| a.name == name).and_then(|a| a.avatar))
            };
            crate::set_avatar(st(), "p1".into(), Some(png.into()))?;
            if pic("p1").await?.as_deref() != Some(png) { return Err("设置后 overview 里没有头像".into()); }
            if crate::set_avatar(st(), "nobody".into(), Some(png.into())).is_ok() { return Err("不存在的 agent 也能设置".into()); }
            within(20, crate::rename_agent(st(), "p1".into(), "p2".into())).await??;
            if pic("p2").await?.as_deref() != Some(png) { return Err("改名后头像没跟过去".into()); }
            crate::set_avatar(st(), "p2".into(), None)?;
            if pic("p2").await?.is_some() { return Err("清除后还有头像".into()); }
            Ok(String::new()) });
        let _ = within(10, crate::remove_agent(st(), "p2".into(), Some(true))).await;
        // end v1.6.0
        // v1.6.2
        let _ = check!("计划步骤：回复中显示 doing，结束后清除", {
            within(20, crate::add_agent(st(), spec("c1"))).await??;
            let doing = || async {
                within(5, crate::overview(st())).await?.map(|o| o.agents.into_iter().find(|a| a.name == "c1").map(|a| (a.doing, a.busy_since)).unwrap_or_default())
            };
            let n = count(&core, "dm:c1", "c1");
            crate::send(st(), "dm:c1".into(), "__plan__".into())?;
            let seen = within(10, async {
                loop {
                    if let Ok((Some(d), Some(_))) = doing().await { return d; }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }).await?;
            if seen != "写测试" { return Err(format!("doing = {seen}")); }
            wait_msg(&core, "dm:c1", "c1", n, 30).await?;
            let after = doing().await?;
            if after != (None, None) { return Err(format!("结束后还有 {after:?}")); }
            Ok(seen) });
        let _ = within(10, crate::remove_agent(st(), "c1".into(), Some(true))).await;
        // end v1.6.2
        // v1.6.5 baseline (docs/features.md): the backend features nothing above covered.
        use serde_json::json;
        async fn tool(core: &Core, token: &str, name: &str, args: serde_json::Value) -> Result<String, String> {
            let v = mcp_post(core, token, serde_json::json!({ "msg": { "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": name, "arguments": args } } })).await?;
            let t = v["result"]["content"][0]["text"].as_str().unwrap_or_default().to_owned();
            if v["result"]["isError"] == true { Err(t) } else { Ok(t) }
        }
        async fn wait_busy(core: &Core, agent: &str, busy: bool) -> Result<(), String> {
            within(20, async { while core.hub.activity(agent).0.is_some() != busy { tokio::time::sleep(Duration::from_millis(100)).await; } }).await
        }
        let _ = check!("权限卡片 允许后执行", {
            let n = count(&core, "dm:m1", "m1"); crate::send(st(), "dm:m1".into(), "hi".into())?;
            wait_msg(&core, "dm:m1", "m1", n, 30).await.and_then(|b| if b.ends_with(" ran") { Ok(b) } else { Err(b) }) });
        let _ = check!("权限 这一轮已结束时作答报错并撤卡", {
            let opt = disco_types::PermissionChoice { id: "allow".into(), label: "允许".into(), allow: true, always: false };
            core.inbox_permission("dm:m1", "m1", "no-such-request", "过期的请求", &[opt]);
            let id = core.inbox.list().into_iter().find(|i| i.title == "过期的请求").map(|i| i.id);
            // The approver may get there first; either way nobody may get a success.
            if let Some(id) = id && core.inbox_answer(&id, inbox::Answer { option: Some("allow".into()), text: None }, "selftest").await.is_ok() {
                return Err("过期的请求也报成功".into());
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
            if core.inbox.list().iter().any(|i| i.title == "过期的请求") { Err("卡片还在".into()) } else { Ok(String::new()) } });
        let mcp_gt = core.hub.mcp_tokens.mint("m1", &gt);
        let _ = check!("ask_user 先答者生效", {
            let (c, tok) = (core.clone(), mcp_gt.clone());
            let h = tokio::spawn(async move { tool(&c, &tok, "ask_user", json!({ "question": "选哪个？", "options": ["A", "B"] })).await });
            let item = within(10, async { loop {
                if let Some(i) = core.inbox.list().into_iter().find(|i| i.kind == "question" && i.title == "选哪个？") { return i; }
                tokio::time::sleep(Duration::from_millis(100)).await;
            } }).await?;
            let first = core.inbox_answer(&item.id, inbox::Answer { option: Some(item.options[1].id.clone()), text: None }, "Discord").await?;
            if core.inbox_answer(&item.id, inbox::Answer { option: Some(item.options[0].id.clone()), text: None }, "Disco").await.is_ok() { return Err("第二个回答也成功了".into()); }
            let t = within(10, h).await?.map_err(|e| e.to_string())??;
            if first != "B" || t != "用户回答：B" { return Err(format!("{first} / {t}")); }
            if last_from(&core, &gt, "me").as_deref() != Some("回答 m1：B") { return Err("聊天里没有回答记录".into()); }
            Ok(t) });
        let _ = check!("提问 空回答不算、自由回答、全部停止撤回", {
            let (id, rx) = core.inbox_question("dm:m1", "m1", "随便说点", vec![], true);
            if crate::inbox(st()).iter().all(|i| i.id != id) { return Err("inbox 命令里看不到".into()); }
            if crate::inbox_answer(st(), id.clone(), None, Some("  ".into())).await.is_ok() { return Err("空回答被接受".into()); }
            if !core.inbox.list().iter().any(|i| i.id == id) { return Err("空回答后问题丢了".into()); }
            let line = crate::inbox_answer(st(), id.clone(), None, Some(" 自由回答 ".into())).await?;
            let got = within(5, rx).await?.map_err(|e| e.to_string())?;
            if line != "自由回答" || got != "自由回答" { return Err(format!("{line} / {got}")); }
            let (_, rx) = core.inbox_question("dm:m1", "m1", "会被撤回", vec!["好".into()], false);
            core.inbox_clear();
            if within(5, rx).await?.is_ok() { return Err("全部停止后还在等回答".into()); }
            Ok(String::new()) });
        let _ = check!("stop_reply 停止私聊里的回复", {
            within(20, crate::add_agent(st(), spec("h1"))).await??;
            crate::send(st(), "dm:h1".into(), "__hang__".into())?;
            wait_busy(&core, "h1", true).await?;
            tokio::time::sleep(Duration::from_millis(1500)).await; // the prompt must reach the agent first
            within(10, crate::stop_reply(st(), "dm:h1".into(), "h1".into())).await??;
            wait_busy(&core, "h1", false).await.map(|_| String::new()) });
        let _ = check!("群 /stop 停止回复", {
            let sid = within(10, crate::save_group(st(), None, None, "stop".into(), vec!["h1".into()], Some("h1".into()))).await??;
            let sg = format!("group:{sid}");
            crate::send(st(), sg.clone(), "__hang__".into())?;
            wait_busy(&core, "h1", true).await?;
            tokio::time::sleep(Duration::from_millis(1500)).await; // the prompt must reach the agent first
            crate::send(st(), sg.clone(), "/stop".into())?;
            wait_busy(&core, "h1", false).await?;
            within(10, async { loop {
                let said = core.db().messages(&sg, 50).map(|v| v.into_iter().find(|m| m.author == "system" && m.body.starts_with("已停止")).map(|m| m.body)).unwrap_or_default();
                if let Some(b) = said { return b; }
                tokio::time::sleep(Duration::from_millis(100)).await;
            } }).await });
        let groups = core.db().list_groups().unwrap_or_default();
        if let Some(g) = groups.into_iter().find(|g| g.name == "stop") { let _ = crate::delete_group(st(), g.id); }
        let _ = within(10, crate::remove_agent(st(), "h1".into(), Some(true))).await;

        // Sharing: peer turns called the way the A2A server calls them.
        let (etx, _erx) = tokio::sync::mpsc::unbounded_channel();
        let mk_share = |agent: &str, guest: &str, mode: &str, limit: u32| -> Result<String, String> {
            crate::share::create_share(st(), agent.into(), guest.into(), mode.into(), limit)?;
            core.shares().into_iter().find(|(_, s)| s.guest == guest).map(|(id, _)| id).ok_or_else(|| "找不到共享".to_string())
        };
        let _ = check!("共享 每日额度用完即拒、没开始的失败退回额度", {
            let id = mk_share("m1", "额度", "chat", 1)?;
            within(40, core.peer_turn(&id, "local", "dm:g", "hi", etx.clone())).await??;
            let used = core.used_today(&id);
            if used != 1 { return Err(format!("用了 {used}")); }
            let e = within(10, core.peer_turn(&id, "local", "dm:g", "hi", etx.clone())).await?.err().unwrap_or_default();
            let used = core.used_today(&id);
            crate::share::revoke_share(st(), id);
            if !e.contains("额度已用完") || used != 1 { return Err(format!("{e} used={used}")); }
            within(20, crate::add_agent(st(), crate::NewAgent { command: Some("/nonexistent/agent --acp".into()), ..spec("bad2") })).await??;
            let id = mk_share("bad2", "退回", "chat", 1)?;
            let r = within(30, core.peer_turn(&id, "local", "dm:g", "hi", etx.clone())).await?;
            let used = core.used_today(&id);
            crate::share::revoke_share(st(), id);
            let _ = within(10, crate::remove_agent(st(), "bad2".into(), Some(true))).await;
            if r.is_ok() { return Err("坏 agent 也成功了".into()); }
            if used == 0 { Ok(e) } else { Err(format!("失败的消息算了额度：{used}")) } });
        let _ = check!("共享 chat 模式拒绝需审批的操作，ask 模式由主人审批", {
            let chat = mk_share("m1", "模式chat", "chat", 0)?;
            let a = within(40, core.peer_turn(&chat, "local", "dm:g", "hi", etx.clone())).await?;
            let none = within(10, crate::share::peer_check(st(), chat.clone())).await??;
            crate::share::revoke_share(st(), chat);
            let ask = mk_share("m1", "模式ask", "ask", 0)?;
            let b = within(40, core.peer_turn(&ask, "local", "dm:g", "hi", etx.clone())).await?;
            crate::share::revoke_share(st(), ask);
            let (a, b) = (a?, b?);
            if none.is_some() { return Err(format!("没有群却有对方群状态：{none:?}")); }
            if a.trim_end().ends_with("skipped") && b.trim_end().ends_with("ran") { Ok(String::new()) } else { Err(format!("{a} | {b}")) } });
        let _ = check!("免打扰 net_policy_set 往返，期间拒绝且不扣额度", {
            use chrono::Timelike;
            let now = chrono::Local::now();
            let m = now.hour() * 60 + now.minute();
            let hm = |x: u32| format!("{:02}:{:02}", (x % 1440) / 60, x % 60);
            // Quiet around the clock except the minute after next.
            let p = crate::tailnet::Policy { quiet_start: hm(m + 2), quiet_end: hm(m + 1), allow: vec!["Box.Tail1.ts.net".into()], ..Default::default() };
            let saved = crate::net_policy_set(st(), p)?;
            if crate::net_policy(st()) != saved || saved.allow != ["box.tail1.ts.net"] { return Err(format!("{saved:?}")); }
            if crate::net_policy_set(st(), crate::tailnet::Policy { deny: vec!["bad rule".into()], ..Default::default() }).is_ok() { return Err("无效规则被接受".into()); }
            if crate::net_policy(st()) != saved { return Err("无效规则覆盖了已保存的规则".into()); }
            let id = mk_share("m1", "免打扰", "chat", 0)?;
            let e = within(10, core.peer_turn(&id, "local", "dm:g", "hi", etx.clone())).await?.err().unwrap_or_default();
            let used = core.used_today(&id);
            crate::share::revoke_share(st(), id);
            crate::net_policy_set(st(), crate::tailnet::Policy::default())?;
            if e.contains("免打扰") && used == 0 { Ok(e) } else { Err(format!("{e} used={used}")) } });
        let _ = check!("对方群 peer_turn 记下候选群（team:/ask: 形式），私聊不覆盖，换群重新确认", {
            let id = mk_share("m1", "群候选", "chat", 0)?;
            let ctx = |id: &str| core.db().setting(&format!("{}{id}", crate::share::PEER_CTX));
            within(40, core.peer_turn(&id, "local", "team:group:777:m1", "hi", etx.clone())).await??;
            let c1 = ctx(&id);
            within(40, core.peer_turn(&id, "local", "dm:g", "hi", etx.clone())).await??;
            let c2 = ctx(&id);
            let _ = core.db().set_setting(&format!("{}{id}", crate::share::PEER_OK), ""); // as if confirmed
            within(40, core.peer_turn(&id, "local", "ask:m9:group:888", "hi", etx.clone())).await??;
            let c3 = ctx(&id);
            let missing = core.peer_missing(&id).unwrap_or_default();
            let checked = within(10, crate::share::peer_check(st(), id.clone())).await??.unwrap_or_default();
            let view = within(5, crate::overview(st())).await??.peers.into_iter().find(|p| p.thread == format!("peer:{id}")).map(|p| (p.group, p.talk));
            crate::share::revoke_share(st(), id);
            if c1.as_deref() != Some("group:777") || c2 != c1 || c3.as_deref() != Some("group:888") { return Err(format!("{c1:?} {c2:?} {c3:?}")); }
            if missing.is_empty() || checked.is_empty() { return Err("换群后仍当作可以说话".into()); }
            if view != Some((true, false)) { return Err(format!("{view:?}")); }
            Ok(checked) });
        let _ = check!("共享状态、重发邀请码、坏邀请码", {
            let id = mk_share("m1", "状态", "ask", 3)?;
            let v = serde_json::to_value(crate::share::share_status(st())).map_err(|e| e.to_string())?;
            let code = crate::share::share_invite(st(), id.clone());
            crate::share::revoke_share(st(), id.clone());
            let row = v["shares"].as_array().and_then(|a| a.iter().find(|r| r["id"] == id.as_str()).cloned()).ok_or("状态里没有这个共享")?;
            if row["mode"] != "ask" || row["limit"] != 3 || row["used_today"] != 0 || v["base"].as_str().is_none_or(|b| !b.starts_with("http://")) { return Err(v.to_string()); }
            if !code?.starts_with("disco1:") { return Err("邀请码格式不对".into()); }
            if crate::share::share_invite(st(), id).is_ok() { return Err("撤销后还能拿到邀请码".into()); }
            for bad in ["hello", "disco1:!!!", "disco1:e30"] {
                if within(10, crate::share::accept_invite(st(), bad.into(), None)).await?.is_ok() { return Err(format!("{bad} 被接受")); }
            }
            Ok(String::new()) });

        // Disco MCP tools an agent uses in a group.
        let _ = check!("MCP 待办 增/列/完成", {
            let r = tool(&core, &mcp_gt, "todo_add", json!({ "text": "MCP 待办", "assignee": "me" })).await?;
            let id = r.rsplit(' ').next().unwrap_or_default().to_owned();
            let l = tool(&core, &mcp_gt, "todo_list", json!({})).await?;
            if !l.contains("MCP 待办") || !l.contains(&id) { return Err(l); }
            tool(&core, &mcp_gt, "todo_done", json!({ "id": id })).await?;
            if !crate::mcp::todos(&core, &gt).iter().any(|t| t.text == "MCP 待办" && t.done) { return Err("没标记完成".into()); }
            let l = tool(&core, &mcp_gt, "todo_list", json!({})).await?;
            if l.contains("MCP 待办") { return Err(format!("默认列表里还有已完成的：{l}")); }
            Ok(id) });
        let _ = check!("MCP 群文档 追加/替换/读取", {
            tool(&core, &mcp_gt, "doc_write", json!({ "content": "MCP 追加" })).await?;
            let d = tool(&core, &mcp_gt, "doc_read", json!({})).await?;
            if !d.contains("目标") || !d.contains("MCP 追加") { return Err(d); }
            tool(&core, &mcp_gt, "doc_write", json!({ "content": "# 重写", "mode": "replace" })).await?;
            let d = crate::space(st(), gt.clone())?.doc;
            if d == "# 重写\n" { Ok(String::new()) } else { Err(d) } });
        let _ = check!("分层记忆 self/group/all 各归其层，新会话按层拿到", {
            for (text, scope) in [("基线-自己", "self"), ("基线-本群", "group"), ("基线-所有", "all")] {
                tool(&core, &mcp_gt, "remember", json!({ "text": text, "scope": scope, "topic": "baseline" })).await?;
            }
            // Scope folders are slugs (at most 40 chars), and a local group id is longer.
            let g = crate::memory::slug(&crate::memory::group_of(&gt).ok_or("没有群层")?);
            let notes = crate::memories(st());
            let scope_of = |t: &str| notes.iter().find(|m| m.text == t).map(|m| (m.scope.clone(), m.topic.clone()));
            let want = |s: &str| Some((s.to_owned(), "baseline".to_owned()));
            if scope_of("基线-自己") != want("m1") || scope_of("基线-本群") != want(&g) || scope_of("基线-所有") != want("all") {
                return Err(format!("{:?} {:?} {:?}", scope_of("基线-自己"), scope_of("基线-本群"), scope_of("基线-所有")));
            }
            let r = tool(&core, &mcp_gt, "memory_read", json!({ "scope": "group", "topic": "baseline" })).await?;
            if !r.contains("基线-本群") { return Err(r); }
            let in_group = core.memory_preamble("m1", &gt);
            if !["基线-自己", "基线-本群", "基线-所有"].iter().all(|t| in_group.contains(t)) { return Err(in_group); }
            let in_dm = core.memory_preamble("m3", "dm:m3");
            if !in_dm.contains("基线-所有") || in_dm.contains("基线-本群") || in_dm.contains("基线-自己") { return Err(in_dm); }
            let dm_tok = core.hub.mcp_tokens.mint("m1", "dm:m1");
            if tool(&core, &dm_tok, "remember", json!({ "text": "私聊里的群记忆", "scope": "group" })).await.is_ok() { return Err("私聊里存进了群层".into()); }
            crate::set_memory(st(), gt.clone(), false)?;
            let off = tool(&core, &mcp_gt, "remember", json!({ "text": "关了也存" })).await;
            crate::set_memory(st(), gt.clone(), true)?;
            if off.is_ok() { return Err("记忆关闭时 MCP 还能存".into()); }
            for m in core.memory_list().into_iter().filter(|m| m.topic == "baseline") { let _ = crate::forget(st(), m.key); }
            Ok(String::new()) });

        // Settings, diagnostics and the guards around them.
        let _ = check!("一键诊断返回检查项", {
            let items = within(30, crate::diagnose(st())).await??;
            let names: Vec<&str> = items.iter().map(|c| c.name.as_str()).collect();
            let want = ["网络代理", "Discord Bot token", "Tailscale", "Node.js / npx"];
            if want.iter().all(|w| names.contains(w)) && items.iter().all(|c| !c.detail.is_empty()) { Ok(format!("{} 项", items.len())) } else { Err(format!("{names:?}")) } });
        let _ = check!("没连 Discord 时设置页不崩、给出原因", {
            let s = within(10, crate::discord_settings(st(), None, Some("123, abc，456".into()))).await??;
            let _ = within(10, crate::discord_settings(st(), None, Some(String::new()))).await?;
            if s.problem.as_deref().is_none_or(str::is_empty) || !s.guilds.is_empty() || s.invite_url.is_some() || s.allow != "123, 456" {
                return Err(format!("{:?} {}", s.problem, s.allow));
            }
            if within(10, crate::save_discord_token(st(), "short".into())).await?.is_ok() { return Err("不完整的 token 被接受".into()); }
            if within(10, crate::discord_channels(st())).await?.is_ok() || within(10, crate::discord_sync(st())).await?.is_ok() { return Err("没连 Discord 却成功了".into()); }
            let saved = core.db().setting(crate::SET_TOKEN_SAVED).is_some();
            if !saved && crate::discord_connect(st()).is_ok() { return Err("没有 token 也能连".into()); }
            Ok(s.problem.unwrap_or_default()) });
        let _ = check!("set_look 校验并出现在 overview", {
            crate::set_look(st(), "m1".into(), "orbit.brand".into())?;
            let look = within(5, crate::overview(st())).await??.agents.into_iter().find(|a| a.name == "m1").map(|a| a.look);
            let bad = crate::set_look(st(), "m1".into(), "a b".into()).is_ok() || crate::set_look(st(), "nobody".into(), "x".into()).is_ok();
            crate::set_look(st(), "m1".into(), String::new())?;
            if look.as_deref() == Some("orbit.brand") && !bad { Ok(String::new()) } else { Err(format!("{look:?} bad={bad}")) } });
        let _ = check!("只打开白名单链接、只显示数据目录里的文件", {
            if crate::open_url("https://evil.example/".into()).is_ok() || crate::open_url("file:///etc/passwd".into()).is_ok() { return Err("白名单外的链接被打开".into()); }
            let base = app.path().app_data_dir().map_err(|e| e.to_string())?;
            for p in ["/etc/passwd".to_owned(), base.join("..").join("x").display().to_string(), base.join("no-such-file").display().to_string()] {
                if crate::reveal_path(st(), p.clone()).is_ok() { return Err(format!("{p} 被显示")); }
            }
            Ok(String::new()) });
        let _ = check!("检测已安装的 agent 和 conda 环境", {
            let d = crate::detect_agents(st());
            let n = crate::conda_envs().len();
            if d.is_empty() { Err("没有已知 agent".into()) } else { Ok(format!("{} 种已知，{n} 个 conda 环境", d.len())) } });
        let _ = check!("一次添加多个实例", {
            let names = within(20, crate::add_agent(st(), crate::NewAgent { count: Some(2), worktree: Some(false), ..spec("k") })).await??;
            for n in &names { let _ = within(10, crate::remove_agent(st(), n.clone(), Some(true))).await; }
            if names == ["k", "k-2"] { Ok(String::new()) } else { Err(format!("{names:?}")) } });
        let _ = check!("群里 记住 存进本群记忆", {
            let n = count(&core, &gt, "system");
            crate::send(st(), gt.clone(), "记住 群里的约定".into())?;
            let b = wait_msg(&core, &gt, "system", n, 5).await?;
            let g = crate::memory::slug(&crate::memory::group_of(&gt).unwrap_or_default());
            let note = core.memory_list().into_iter().find(|m| m.text == "群里的约定");
            let ok = note.as_ref().is_some_and(|m| m.scope == g);
            if let Some(m) = note { let _ = crate::forget(st(), m.key); }
            if ok && b.contains("本群") { Ok(b) } else { Err(b) } });
        let _ = check!("Discord 朋友不能用改状态的命令", {
            let n = count(&core, "group:chan1", "system");
            core.on_incoming(msg("chan1", "u9", "Bob", "@m1 /model opus"));
            wait_msg(&core, "group:chan1", "system", n, 10).await.and_then(|b| if b.contains("只有它的主人能用") { Ok(b) } else { Err(b) }) });
        let _ = check!("别的 Disco 的 agent 接力限流", {
            let ok: Vec<bool> = (0..7).map(|_| core.foreign_turn_ok("chan-rate")).collect();
            if ok[..6].iter().all(|b| *b) && !ok[6] { Ok(String::new()) } else { Err(format!("{ok:?}")) } });
        let _ = check!("回答里的【记住】存进记忆", {
            core.remember_from("m1", "dm:m1", &format!("好的\n{}来自回答", crate::MEM_TAG));
            let note = crate::memories(st()).into_iter().find(|m| m.text == "来自回答");
            let ok = note.as_ref().is_some_and(|m| m.scope == "m1");
            if let Some(m) = note { let _ = crate::forget(st(), m.key); }
            if ok { Ok(String::new()) } else { Err("没存上".into()) } });
        let _ = check!("MCP post 叫来被 @ 的成员、send_file 放进空间", {
            let n = count(&core, &gt, "m3");
            let r = tool(&core, &mcp_gt, "post", json!({ "text": "@m3 看一下" })).await?;
            wait_msg(&core, &gt, "m3", n, 30).await?;
            let f = std::env::temp_dir().join("disco-selftest-mcp.txt");
            std::fs::write(&f, "hi").map_err(|e| e.to_string())?;
            let s = tool(&core, &mcp_gt, "send_file", json!({ "path": f.display().to_string() })).await?;
            if r.contains("m3") && s.starts_with("已放进共享空间") { Ok(String::new()) } else { Err(format!("{r} / {s}")) } });
        // end v1.6.5 baseline

        // v1.8.0 cross-device files: guest and owner are this same app over a loopback share.
        use sha2::Digest as _;
        use disco_types::FilePart;
        use crate::files::MAX_A2A;
        let sha = |b: &[u8]| hex::encode(sha2::Sha256::digest(b));
        let tmp = std::env::temp_dir().join(format!("disco-xfer-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&tmp);
        let fshare = mk_share("m1", "文件友", "chat", 0).unwrap_or_default();
        let owner_copy = crate::mcp::space_dir(&core, &format!("peer:{fshare}")).map(|d| d.join("files").join("报告.txt")).unwrap_or_default();
        let _ = check!("跨设备文件 访客附件随下一条消息送达，主人校验 SHA 后存下并告诉 agent", {
            let code = crate::share::share_invite(st(), fshare.clone())?;
            within(20, crate::share::accept_invite(st(), code, Some("fg1".into()))).await??;
            // 11 MB: more than the A2A server's own 10 MB body limit, so the guard must take it out.
            let data: Vec<u8> = (0..11u32 << 20).map(|i| (i % 251) as u8).collect();
            let f = tmp.join("报告.txt");
            std::fs::write(&f, &data).map_err(|e| e.to_string())?;
            let r = within(20, crate::send_file(st(), "dm:fg1".into(), f.display().to_string(), None)).await??;
            if !r.contains("fg1") { return Err(r); }
            let n = count(&core, "dm:fg1", "fg1");
            crate::send(st(), "dm:fg1".into(), "__echo__ 看看".into())?;
            let b = wait_msg(&core, "dm:fg1", "fg1", n, 60).await?;
            if !b.contains(&sha(&data)) || !b.contains(&owner_copy.display().to_string()) { return Err(b.chars().take(300).collect()); }
            if std::fs::read(&owner_copy).ok() != Some(data) { return Err("主人那边存的内容不对".into()); }
            if !last_from(&core, &format!("peer:{fshare}"), "文件友").is_some_and(|l| l.contains("📎 报告.txt")) { return Err("主人的会话里没显示文件".into()); }
            Ok(String::new()) });
        let _ = check!("跨设备文件 主人的 agent 用 MCP send_file 发回访客（只聊天只能发共享空间里的）", {
            let ctx = format!("peer:{fshare}|dm:fg1");
            let n = count(&core, "dm:fg1", "fg1");
            crate::send(st(), "dm:fg1".into(), "__hang__".into())?;
            wait_busy(&core, "m1", true).await?;
            tokio::time::sleep(Duration::from_millis(1500)).await; // the prompt must reach the agent first
            let tok = core.hub.mcp_tokens.mint("m1", &ctx);
            let outside = tmp.join("outside.txt");
            std::fs::write(&outside, "no").map_err(|e| e.to_string())?;
            let refused = tool(&core, &tok, "send_file", json!({ "path": outside.display().to_string() })).await;
            let sent = tool(&core, &tok, "send_file", json!({ "path": owner_copy.display().to_string(), "note": "给你" })).await;
            let line = within(30, async { loop {
                let got = core.db().messages("dm:fg1", 50).ok().and_then(|v| v.into_iter().rev().find(|m| m.author == "fg1" && m.body.contains("📎")).map(|m| m.body));
                if let Some(l) = got && count(&core, "dm:fg1", "fg1") > n { return l; }
                tokio::time::sleep(Duration::from_millis(100)).await;
            } }).await;
            core.hub.cancel("m1", &ctx).await;
            let _ = wait_busy(&core, "m1", false).await;
            if !refused.as_ref().is_err_and(|e| e.contains("只聊天")) { return Err(format!("空间外的文件：{refused:?}")); }
            let sent = sent?;
            let line = line?;
            let path = line.lines().last().unwrap_or_default().to_owned();
            if std::fs::read(&path).ok() != std::fs::read(&owner_copy).ok() { return Err(format!("访客收到的不对：{line}")); }
            Ok(sent) });
        let _ = check!("跨设备文件 需要时问我：主人拒绝不发、同意后对方不在线也不发", {
            let id = mk_share("m1", "问我", "ask", 0)?;
            let tok = core.hub.mcp_tokens.mint("m1", &format!("peer:{id}|dm:x"));
            let f = tmp.join("secret.txt");
            std::fs::write(&f, "s").map_err(|e| e.to_string())?;
            let mut got = Vec::new();
            for pick in [1, 0] {
                let (c, t, fp) = (core.clone(), tok.clone(), f.display().to_string());
                let h = tokio::spawn(async move { tool(&c, &t, "send_file", json!({ "path": fp })).await });
                let item = within(10, async { loop {
                    if let Some(i) = core.inbox.list().into_iter().find(|i| i.title.contains("secret.txt")) { return i; }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                } }).await?;
                core.inbox_answer(&item.id, inbox::Answer { option: Some(item.options[pick].id.clone()), text: None }, "Disco").await?;
                got.push(within(10, h).await?.map_err(|e| e.to_string())?);
            }
            crate::share::revoke_share(st(), id);
            match (&got[0], &got[1]) {
                (Err(a), Err(b)) if a.contains("没有同意") && b.contains("这一轮已经结束") => Ok(String::new()),
                _ => Err(format!("{got:?}")),
            } });
        let _ = check!("跨设备文件 超过 20 MB 被拒（附件、MCP、接收）", {
            let big = tmp.join("big.bin");
            std::fs::File::create(&big).and_then(|f| f.set_len(MAX_A2A + 1)).map_err(|e| e.to_string())?;
            let a = within(10, crate::send_file(st(), "dm:fg1".into(), big.display().to_string(), None)).await?;
            let tok = core.hub.mcp_tokens.mint("m1", &format!("peer:{fshare}|dm:fg1"));
            let b = tool(&core, &tok, "send_file", json!({ "path": big.display().to_string() })).await;
            let c = crate::files::save_incoming(&tmp.join("in"), &FilePart { name: "x".into(), size: MAX_A2A + 1, bytes: vec![0; (MAX_A2A + 1) as usize], ..Default::default() });
            let _ = std::fs::remove_file(&big);
            let all = [a.err(), b.err(), c.err()];
            if all.iter().all(|e| e.as_deref().is_some_and(|e| e.contains("20 MB"))) { Ok(String::new()) } else { Err(format!("{all:?}")) } });
        let _ = check!("跨设备文件 SHA 不符或不完整就丢弃，文件名不能越界", {
            let dir = tmp.join("in");
            let good = FilePart { name: "../../etc/passwd".into(), size: 2, sha256: sha(b"hi"), bytes: b"hi".to_vec(), ..Default::default() };
            let (p, ..) = crate::files::save_incoming(&dir, &good)?;
            if p.parent() != Some(dir.as_path()) || p.file_name().and_then(|n| n.to_str()) != Some("passwd") { return Err(p.display().to_string()); }
            let bad = crate::files::save_incoming(&dir, &FilePart { sha256: "00".repeat(32), ..good.clone() }).err().unwrap_or_default();
            let short = crate::files::save_incoming(&dir, &FilePart { size: 3, ..good.clone() }).err().unwrap_or_default();
            let n = std::fs::read_dir(&dir).map(|d| d.count()).unwrap_or(0);
            if bad.contains("SHA-256") && short.contains("不完整") && n == 1 { Ok(String::new()) } else { Err(format!("{bad} / {short} / {n}")) } });
        let _ = check!("跨设备文件 旁观中不能发、取回地址要共享 token 且只能取一次", {
            let f = tmp.join("x.txt");
            std::fs::write(&f, "x").map_err(|e| e.to_string())?;
            let e = within(10, crate::send_file(st(), format!("peer:{fshare}"), f.display().to_string(), None)).await?.err().unwrap_or_default();
            if !e.contains("旁观") { return Err(e); }
            let (base, http) = (core.share_base()?, crate::openai::client(&[]));
            let token = crate::secret_get(&format!("share/{fshare}")).ok_or("没有共享 token")?;
            let url = format!("{base}/a/{fshare}/f/{}", "0".repeat(32));
            let anon = within(10, http.get(&url).bearer_auth("wrong").send()).await?.map_err(|e| e.to_string())?.status();
            let gone = within(10, http.get(&url).bearer_auth(&token).send()).await?.map_err(|e| e.to_string())?.status();
            if anon == 401 && gone == 404 { Ok(String::new()) } else { Err(format!("{anon} {gone}")) } });
        crate::share::revoke_share(st(), fshare);
        let _ = within(10, crate::remove_agent(st(), "fg1".into(), Some(true))).await;
        let _ = std::fs::remove_dir_all(&tmp);
        println!("SELFTEST {}", if fails == 0 { "OK".to_string() } else { format!("{fails} FAILED") });
        std::process::exit(if fails == 0 { 0 } else { 1 });
    });
}

/// A tiny OpenAI-compatible server: lists one model; asks for `todo_add` when tools are
/// offered, then answers with what the tool returned. Returns its base URL.
async fn mock_openai() -> Result<String, String> {
    use axum::{routing::{get, post}, Json};
    use serde_json::{json, Value};
    async fn chat(Json(req): Json<Value>) -> String {
        let msgs = req["messages"].as_array().cloned().unwrap_or_default();
        let sse = |delta: Value| format!("data: {}\n\n", json!({ "choices": [{ "index": 0, "delta": delta }] }));
        if let Some(t) = msgs.iter().rev().find(|m| m["role"] == "tool") {
            format!("{}{}data: [DONE]\n\n", sse(json!({ "content": "好了：" })), sse(json!({ "content": t["content"] })))
        } else if req.get("tools").is_some() {
            format!("{}{}data: [DONE]\n\n",
                sse(json!({ "tool_calls": [{ "index": 0, "id": "c1", "type": "function", "function": { "name": "todo_add", "arguments": "{\"text\":" } }] })),
                sse(json!({ "tool_calls": [{ "index": 0, "function": { "arguments": "\"来自模型\"}" } }] })))
        } else {
            format!("{}data: [DONE]\n\n", sse(json!({ "content": "好了（没有工具）" })))
        }
    }
    let app = axum::Router::new()
        .route("/v1/models", get(|| async { Json(json!({ "data": [{ "id": "mock-model" }] })) }))
        .route("/v1/chat/completions", post(chat));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.map_err(|e| e.to_string())?;
    let port = l.local_addr().map_err(|e| e.to_string())?.port();
    tokio::spawn(async move { let _ = axum::serve(l, app).await; });
    Ok(format!("http://127.0.0.1:{port}/v1"))
}
