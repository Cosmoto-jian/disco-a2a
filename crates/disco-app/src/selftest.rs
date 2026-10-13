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
        // Approve every permission the mock asks for, like a user would.
        let c2 = core.clone();
        tauri::async_runtime::spawn(async move {
            loop {
                for it in c2.inbox.list() {
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
            command: Some(format!("\"{mock}\"")), cwd: None, count: None, worktree: None, conda: None };
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

        // the rest of the surface
        let _ = check!("messages", crate::messages(st(), gt.clone()).map(|v| format!("{} 条", v.len())));
        let _ = check!("todo", { let id = crate::todo_new(st(), gt.clone(), "写测试".into())?; crate::todo_set(st(), gt.clone(), id.clone(), true)?; crate::todo_remove(st(), gt.clone(), id)?; Ok(String::new()) });
        let _ = check!("space", crate::space(st(), gt.clone()).map(|s| format!("{} todos", s.todos.len())));
        let _ = check!("agent_commands", within(5, crate::agent_commands(st(), "m1".into())).await.and_then(|r| r.map(|v| format!("{} 个", v.len()))));
        let _ = check!("rename", within(20, crate::rename_agent(st(), "m2".into(), "m3".into())).await.and_then(|r| r).map(|_| String::new()));
        let _ = check!("改名后群聊", { let n = count(&core, &gt, "m3"); crate::send(st(), gt.clone(), "@m3 改名了".into())?; wait_msg(&core, &gt, "m3", n, 30).await });
        let _ = check!("stop_all", within(10, crate::stop_all(st())).await.and_then(|r| r).map(|n| format!("{n}")));
        crate::resume(st());
        let _ = check!("恢复后私聊", { let n = count(&core, "dm:m1", "m1"); crate::send(st(), "dm:m1".into(), "回来了".into())?; wait_msg(&core, "dm:m1", "m1", n, 30).await });
        let bad = crate::NewAgent { command: Some("/nonexistent/agent --acp".into()), ..spec("bad") };
        let _ = check!("自检提示", { within(10, crate::add_agent(st(), bad)).await??; wait_msg(&core, "dm:bad", "system", 0, 10).await });
        let _ = check!("remove_agent", within(10, crate::remove_agent(st(), "bad".into(), Some(true))).await.and_then(|r| r).map(|_| String::new()));
        let _ = check!("overview(结束)", within(5, crate::overview(st())).await.and_then(|r| r.map(|o| format!("{} agents, {} groups", o.agents.len(), o.groups.len()))));
        println!("SELFTEST {}", if fails == 0 { "OK".to_string() } else { format!("{fails} FAILED") });
        std::process::exit(if fails == 0 { 0 } else { 1 });
    });
}
