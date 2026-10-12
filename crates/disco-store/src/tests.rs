use super::*;

fn rand_vec(rng: &mut fastrand::Rng) -> Vec<f32> {
    let mut v: Vec<f32> = (0..EMBED_DIM).map(|_| rng.f32() - 0.5).collect();
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    v.iter_mut().for_each(|x| *x /= n);
    v
}

#[test]
fn migrates_to_latest_and_reopens() {
    let dir = std::env::temp_dir().join(format!("disco-store-{}", fastrand::u64(..)));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("t.db");
    let s = Store::open(&path).unwrap();
    assert_eq!(s.schema_version().unwrap(), migrations::LATEST);
    assert!(s.vec_version().unwrap().starts_with('v'));
    drop(s);
    let s = Store::open(&path).unwrap(); // idempotent
    assert_eq!(s.schema_version().unwrap(), migrations::LATEST);
}

#[test]
fn rejects_newer_schema() {
    let dir = std::env::temp_dir().join(format!("disco-store-{}", fastrand::u64(..)));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("t.db");
    {
        let c = rusqlite::Connection::open(&path).unwrap();
        c.pragma_update(None, "user_version", 999).unwrap();
    }
    assert!(matches!(Store::open(&path), Err(StoreError::TooNew { .. })));
}

#[test]
fn scopes_are_isolated_and_quarantine_hides() {
    let mut s = Store::open_in_memory().unwrap();
    let a = s.insert_memory(&NewMemory { layer: Layer::Team, scope_id: "guild-a", author: "x", content: "deploy with make release", trust: 2, embedding: None }).unwrap();
    s.insert_memory(&NewMemory { layer: Layer::Team, scope_id: "guild-b", author: "y", content: "deploy secret plan", trust: 2, embedding: None }).unwrap();
    let hits = s.search(Layer::Team, "guild-a", "deploy", None, 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, a);
    s.quarantine(a).unwrap();
    assert!(s.search(Layer::Team, "guild-a", "deploy", None, 10).unwrap().is_empty());
}

#[test]
fn fts_query_is_injection_safe() {
    let s = Store::open_in_memory().unwrap();
    // FTS5 syntax characters must not error.
    for q in ["\"", "a AND (", "NEAR(", "*", "col:foo", "-x"] {
        s.search(Layer::Room, "r", q, None, 5).unwrap();
    }
}

/// M0-5: 10k rows, hybrid search p95 <= 50 ms. Run: cargo test -p disco-store --release -- --ignored --nocapture
#[test]
#[ignore = "benchmark"]
fn bench_hybrid_10k() {
    let mut rng = fastrand::Rng::with_seed(7);
    let mut s = Store::open_in_memory().unwrap();
    let words = ["rust", "tauri", "discord", "agent", "deploy", "test", "memory", "router", "webhook", "budget", "review", "codex", "claude", "build", "release"];
    let t0 = std::time::Instant::now();
    for i in 0..10_000 {
        let content: String = (0..12).map(|_| words[rng.usize(..words.len())]).collect::<Vec<_>>().join(" ");
        let scope = format!("room-{}", i % 10);
        let v = rand_vec(&mut rng);
        s.insert_memory(&NewMemory { layer: Layer::Room, scope_id: &scope, author: "bench", content: &content, trust: 1, embedding: Some(&v) }).unwrap();
    }
    println!("insert 10k: {:?}", t0.elapsed());
    let mut lat = Vec::new();
    for i in 0..200 {
        let q = rand_vec(&mut rng);
        let scope = format!("room-{}", i % 10);
        let t = std::time::Instant::now();
        let hits = s.search(Layer::Room, &scope, "deploy webhook budget", Some(&q), 10).unwrap();
        lat.push(t.elapsed());
        assert!(!hits.is_empty());
    }
    lat.sort();
    let p50 = lat[lat.len() / 2];
    let p95 = lat[lat.len() * 95 / 100];
    println!("hybrid search p50={p50:?} p95={p95:?}");
    assert!(p95 <= std::time::Duration::from_millis(50), "p95 {p95:?} > 50ms");
}

#[test]
fn agents_upsert_list_delete() {
    let mut s = Store::open_in_memory().unwrap();
    let mut a = AgentRow { id: "1".into(), name: "echo".into(), transport: "a2a".into(),
        endpoint: "http://x".into(), credential_ref: None, card_json: None };
    s.upsert_agent(&a).unwrap();
    a.endpoint = "http://y".into();
    s.upsert_agent(&a).unwrap();
    assert_eq!(s.list_agents().unwrap(), vec![a]);
    assert!(s.delete_agent("echo").unwrap());
    assert!(s.list_agents().unwrap().is_empty());
}

#[test]
fn groups_and_messages() {
    let mut s = Store::open_in_memory().unwrap();
    let g = GroupRow { id: "c1".into(), guild_id: "g1".into(), name: "lab".into(), lead_agent: Some("planner".into()),
        webhook_ref: None, members: vec!["coder".into(), "planner".into()] };
    s.upsert_group(&g).unwrap();
    let got = s.list_groups().unwrap();
    assert_eq!(got.len(), 1);
    let mut m = got[0].members.clone(); m.sort();
    assert_eq!(m, vec!["coder", "planner"]);
    for i in 0..5 { s.add_message("group:c1", "me", &format!("m{i}")).unwrap(); }
    s.add_message("dm:coder", "coder", "hi").unwrap();
    let last = s.messages("group:c1", 2).unwrap();
    assert_eq!(last.iter().map(|m| m.body.as_str()).collect::<Vec<_>>(), ["m3", "m4"]);
    let heads = s.thread_heads().unwrap();
    assert_eq!(heads[0].0, "dm:coder");
    assert_eq!(heads.len(), 2);
    s.audit("me", Some("coder"), "prompt", "ok").unwrap();
    assert!(s.delete_group("c1").unwrap());
}

#[test]
fn settings_roundtrip() {
    let s = Store::open_in_memory().unwrap();
    s.set_setting("agent_channel/coder", "1").unwrap();
    s.set_setting("agent_channel/coder-2", "2").unwrap();
    s.set_setting("home_guild", "g").unwrap();
    assert_eq!(s.setting("home_guild").as_deref(), Some("g"));
    let mut v = s.settings_with_prefix("agent_channel/").unwrap();
    v.sort();
    assert_eq!(v, [("coder".to_string(), "1".to_string()), ("coder-2".to_string(), "2".to_string())]);
    s.set_setting("home_guild", "").unwrap();
    assert_eq!(s.setting("home_guild"), None);
}
