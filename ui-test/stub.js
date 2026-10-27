// Tauri stand-in for the UI regression audit (ui-test/audit.py).
// - window.STUB is the backend state; `overview` returns a fresh copy of it on every call, so a test can
//   change STUB (flip a peer's talk, pause, rename) and fire an event to see the UI redraw.
// - window.CALLS logs every invoke as [command, args]; window.UNKNOWN lists commands this stub does not
//   know (the audit fails on them: a new backend command needs a stub entry and a test).
// - window.__L holds the registered event listeners: __L.agents({payload:null}) fires the "agents" event.
// - window.STUB_FAIL = { send: "boom" } makes the next calls to that command reject with that message.
// - window.STUB_INIT (set before this script runs, e.g. from an init script) is merged over the defaults.
"use strict";
(() => {
  const L = {};
  const AV_SVG = "data:image/svg+xml;base64,PHN2ZyB4bWxucz0iaHR0cDovL3d3dy53My5vcmcvMjAwMC9zdmciIHZpZXdCb3g9IjAgMCA2NCA2NCI+PHJlY3Qgd2lkdGg9IjY0IiBoZWlnaHQ9IjY0IiBmaWxsPSIjODE4Y2Y4Ii8+PC9zdmc+";
  const now = Math.floor(Date.now() / 1000);
  const long = Array.from({ length: 40 }, (_, i) => ({ author: i % 2 ? "helper" : "me", body: `第 ${i + 1} 条消息\n${"内容 ".repeat(12)}`, ts: now - 4000 + i }));
  const STUB = {
    agents: [
      { name: "claude", kind: "acp", template: "claude-code", detail: "/Users/x/code", live: true, look: "", branch: "main",
        doing: "写 README 的 MCP 章节，补上 overview 工具的输出示例和注意事项", busy_since: Date.now() - 120000 },
      { name: "codex", kind: "acp", template: "codex", detail: "", live: true, look: "", branch: "disco/codex" },
      { name: "researcher", kind: "a2a", template: "a2a", detail: "A2A research agent", live: false, look: "", avatar: "bloom.candy" },
      { name: "writer", kind: "acp", template: "", detail: "", live: false, look: "", avatar: AV_SVG },
      { name: "helper", kind: "acp", template: "", detail: "", live: false, look: "" },
    ],
    groups: [{ id: "g1", name: "dev", guild_id: "1", lead: "claude", members: ["claude", "codex"], people: ["Amy"], guests: ["reviewer"] }],
    memory_off: [],
    heads: [
      { thread: "dm:codex", author: "system", body: "MCP servers need attention: hug…", ts: now - 300 },
      { thread: "group:g1", author: "claude", body: "hi", ts: now - 500 },
      { thread: "dm:helper", author: "helper", body: "第 40 条消息", ts: now - 600 },
      { thread: "peer:p1", author: "小王", body: "帮我看下这段代码", ts: now - 400 },
      { thread: "peer:p2", author: "阿杰", body: "@claude 在吗", ts: now - 700 },
      { thread: "peer:p3", author: "小李", body: "你好", ts: now - 800 },
    ],
    // p1: friend's Discord group, can talk; p2: friend's group, watch-only (bot not in their server); p3: invite-only, watch-only.
    peers: [
      { thread: "peer:p1", guest: "小王", agent: "claude", talk: true, group: true, missing: "" },
      { thread: "peer:p2", guest: "阿杰", agent: "claude", talk: false, group: true, missing: "还不能说话：对方还没把你的 Bot 加进他的服务器" },
      { thread: "peer:p3", guest: "小李", agent: "codex", talk: false, group: false, missing: "" },
    ],
    discord_token: true,
    discord_status: "在线 · MyBot#0001",
    paused: false,
    inbox: [
      { id: "a1", kind: "permission", thread: "group:g1", agent: "codex", title: "Run: cargo test -p disco-app",
        options: [{ id: "o1", label: "允许一次", allow: true }, { id: "o2", label: "总是允许", allow: true, always: true }], text: false, ts: now - 100 },
      { id: "a2", kind: "question", thread: "group:g1", agent: "claude", title: "发版前要先跑一遍完整的 macOS CI 吗？",
        options: [{ id: "0", label: "要，跑完再发", allow: false }, { id: "1", label: "不用，直接发", allow: false }], text: true, ts: now - 90 },
      { id: "a3", kind: "question", thread: "dm:claude", agent: "claude", title: "README 用中文还是英文？", options: [], text: true, ts: now - 80 },
    ],
    configs: {
      claude: [{ id: "model", name: "Model", category: "model", current: "sonnet", options: [["sonnet", "Sonnet"], ["opus", "Opus"]] }],
      codex: "需要登录：运行 codex login",
    },
    messages: {
      "group:g1": [
        { author: "me", body: "@claude 帮忙看下登录 bug", ts: now - 600 },
        { author: "claude", body: "@jian 已定位，在 session.rs 第 42 行。@codex 你来写补丁", ts: now - 590 },
        { author: "Amy (Discord)", body: "@reviewer 帮我也看看", ts: now - 580 },
        { author: "reviewer", body: "@Amy 好的，稍等", ts: now - 570 },
        { author: "system", body: "codex 出错：Claude native binary not found\n💡 需要原生版 Claude Code：在终端运行 `curl -fsSL https://claude.ai/install.sh | bash`，然后完全退出并重新打开 Disco", ts: now - 560 },
      ],
      "dm:codex": [
        { author: "me", body: "服务器上复现一下这三个 bug", ts: now - 400 },
        { author: "codex", body: "两个能复现，详情见附件。\n\n📎 results.json（6 KB）\n/Users/x/Library/Application Support/dev.cosmoto.disco/inbox/files/results.json", ts: now - 390 },
        { author: "system", body: "codex 出错：连接超时", ts: now - 300 },
      ],
      "dm:helper": long,
      "peer:p1": [{ author: "小王", body: "帮我看下这段代码", ts: now - 400 }],
      "peer:p2": [{ author: "阿杰", body: "@claude 在吗", ts: now - 700 }],
      "peer:p3": [{ author: "小李", body: "你好", ts: now - 800 }],
      default: [{ author: "me", body: "写个函数", ts: now - 1000 }, { author: "claude", body: "好的：\n```rust\nfn main() {}\n```", ts: now - 990 }],
    },
    team: { "group:g1": { thread: "group:g1", task: "调研 A2A 协议", lead: "claude", stage: "work", dir: "/x",
      roles: [{ agent: "claude", role: "research", task: "查规范", state: "done", last: "" }, { agent: "codex", role: "prototype", task: "写原型", state: "working", last: "Run cargo build" }] } },
    space: {
      todos: [{ id: "123456", text: "整理 ACP 会话泄漏问题", assignee: "codex", by: "claude", done: false },
        { id: "223456", text: "写 README 的 MCP 章节", assignee: "me", by: "me", done: false },
        { id: "323456", text: "确认 Discord READY", by: "claude", done: true }],
      doc: "# 目标\n给 Disco 加内置 MCP。\n\n## 分工\n- claude：服务端\n- codex：UI",
      doc_path: "/Users/x/Library/Application Support/dev.cosmoto.disco/spaces/group-g1/group.md",
    },
    detect: [{ id: "claude-code", name: "Claude Code", found: "/usr/local/bin/claude" }, { id: "codex", name: "Codex", found: "/opt/homebrew/bin/codex" },
      { id: "hermes", name: "Hermes", found: "/usr/local/bin/hermes" }, { id: "pi", name: "pi", found: "/usr/local/bin/pi-acp" },
      { id: "gemini", name: "Gemini CLI", found: null }, { id: "qwen", name: "Qwen Code", found: null }, { id: "goose", name: "Goose", found: null }],
    envs: [{ name: "base", path: "/opt/conda" }, { name: "ml", path: "/opt/conda/envs/ml" }],
    shares: [{ id: "s1", agent: "claude", guest: "小王", mode: "ask", limit: 50, used_today: 3 }],
    memories: [{ key: "k1", scope: "all", topic: "偏好", text: "回答用中文，代码注释用英文" }, { key: "k2", scope: "group-g1", topic: "项目", text: "Disco 用 Tauri 2 + 原生 JS" }],
    update: { current: "1.5.14", version: null },
    policy: { mode: "tailnet", allow: [], deny: ["old-laptop"], quiet_start: "23:00", quiet_end: "08:00" },
    commands: { claude: [["review", "Review code"], ["compact", "Compact context"]] },
    choosePath: "/Users/x/bin/my-agent",
  };
  Object.assign(STUB, window.STUB_INIT || {});
  const clone = (v) => (v === undefined ? null : JSON.parse(JSON.stringify(v)));
  const OV_KEYS = ["agents", "groups", "memory_off", "heads", "peers", "discord_token", "discord_status", "paused"];
  const H = {
    overview: () => Object.fromEntries(OV_KEYS.map((k) => [k, clone(STUB[k])])),
    inbox: () => clone(STUB.inbox),
    inbox_answer: (a) => { STUB.inbox = STUB.inbox.filter((i) => i.id !== a.id); return null; },
    messages: (a) => clone(STUB.messages[a.thread] ?? STUB.messages.default),
    send: () => null,
    stop_reply: () => true,
    send_file: () => "已放进共享空间",
    choose_path: () => STUB.choosePath,
    agent_commands: (a) => clone(STUB.commands[a.agent] ?? []),
    agent_configs: (a) => { const c = STUB.configs[a.agent]; if (typeof c === "string") throw c; return clone(c ?? []); },
    set_agent_config: () => null,
    set_memory: (a) => { STUB.memory_off = a.on ? STUB.memory_off.filter((t) => t !== a.thread) : [...STUB.memory_off, a.thread]; return null; },
    team_state: (a) => clone(STUB.team[a.thread] ?? null),
    team_files: () => [["context.md", "# 任务\n\n调研 A2A 协议"], ["findings/research.md", "# research · claude\n\nA2A 使用 JSON-RPC…"]],
    space: () => clone(STUB.space),
    todo_set: () => null, todo_new: () => null, todo_remove: () => null, doc_save: () => null, reveal_doc: () => null, reveal_path: () => null,
    peer_check: (a) => STUB.peers.find((p) => p.thread === `peer:${a.id}`)?.missing ?? null,
    restart_agent: () => null,
    rename_agent: (a) => { const ag = STUB.agents.find((x) => x.name === a.name); if (!ag) throw "no such agent"; ag.name = a.newName; return null; },
    remove_agent: (a) => { STUB.agents = STUB.agents.filter((x) => x.name !== a.name); return null; },
    set_avatar: (a) => { const ag = STUB.agents.find((x) => x.name === a.agent); if (ag) ag.avatar = a.data; return null; },
    stop_all: () => { STUB.paused = true; return 2; },
    resume: () => { STUB.paused = false; return null; },
    detect_agents: () => clone(STUB.detect),
    conda_envs: () => clone(STUB.envs),
    add_agent: (a) => { const s = a.spec; const names = s.count > 1 ? Array.from({ length: s.count }, (_, i) => `${s.name}-${i + 1}`) : [s.name];
      for (const n of names) STUB.agents.push({ name: n, kind: s.url && s.template === "a2a" ? "a2a" : s.template === "openai" ? "openai" : "acp", template: s.template, detail: s.cwd ?? "", live: false, look: "" });
      return names; },
    accept_invite: (a) => { const n = a.name || "agent-friend"; STUB.agents.push({ name: n, kind: "a2a", template: "a2a", detail: "invite", live: false, look: "" }); return n; },
    share_status: () => ({ base: "http://mac.tail1.ts.net:7480", problem: null, shares: clone(STUB.shares) }),
    create_share: () => "disco1:eyJ1cmwiOiJodHRwOi8vbWFjLnRhaWwxLnRzLm5ldDo3NDgwL2EvYWJjIiwidG9rZW4iOiIxMjMifQ",
    share_invite: () => "disco1:invite",
    revoke_share: (a) => { STUB.shares = STUB.shares.filter((s) => s.id !== a.id); return null; },
    save_group: (a) => { const id = "g" + (STUB.groups.length + 1); STUB.groups.push({ id: `local-${id}`, name: a.name, guild_id: "", lead: a.lead, members: a.members, people: [], guests: [] }); return `local-${id}`; },
    delete_group: (a) => { STUB.groups = STUB.groups.filter((g) => g.id !== a.id); return null; },
    restore_group: (a) => { STUB.groups.push({ id: a.id, name: a.name, guild_id: a.guildId, lead: a.lead, members: a.members, people: [], guests: [] }); return null; },
    discord_channels: () => [{ guild_id: "1", guild: "My Server", id: "c9", name: "general" }],
    discord_settings: (a) => { if (a?.allow !== undefined) STUB.allow = a.allow; return { guilds: [{ id: "1", guild: "My Server" }], home_guild: "1", allow: STUB.allow ?? "", invite_url: "https://discord.com/oauth2/x", problem: null }; },
    discord_sync: () => 3,
    discord_connect: () => null,
    save_discord_token: () => { STUB.discord_token = true; return null; },
    net_policy: () => clone(STUB.policy),
    net_policy_set: (a) => { STUB.policy = clone(a.policy); return clone(a.policy); },
    memories: () => clone(STUB.memories),
    forget: (a) => { STUB.memories = STUB.memories.filter((m) => m.key !== a.key); return null; },
    memory_restore: () => null,
    diagnose: () => [{ name: "代理", ok: true, detail: "未设置" }, { name: "Discord", ok: false, detail: "token 未设置" }],
    check_update: () => clone(STUB.update),
    install_update: () => null,
    open_url: () => null,
  };
  window.STUB = STUB; window.CALLS = []; window.UNKNOWN = []; window.STUB_FAIL = {};
  window.__TAURI__ = {
    core: { invoke: async (c, a) => {
      window.CALLS.push([c, clone(a)]);
      if (window.STUB_FAIL[c] != null) throw window.STUB_FAIL[c];
      if (!H[c]) { window.UNKNOWN.push(c); return null; }
      return H[c](a);
    } },
    event: { listen: async (n, f) => { L[n] = f; return () => delete L[n]; } },
  };
  window.__L = L;
})();
