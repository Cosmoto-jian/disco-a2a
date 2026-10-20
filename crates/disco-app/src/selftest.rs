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
            if pc("group:local-x").is_some() || pc("dm:x").is_some() { return Err("本地群/私聊不该能回话".into()); }
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
            if list["result"]["tools"].as_array().is_none_or(|a| !a.is_empty()) { return Err(format!("朋友会话有工具：{list}")); }
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
