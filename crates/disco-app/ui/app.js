"use strict";
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const $ = (s, r = document) => r.querySelector(s);

// ---------- state
// typing: thread -> Set(agent); live: "thread|agent" -> streaming bubble
const S = { tab: "chats", thread: null, ov: null, tpl: [], typing: {}, live: {}, perms: {}, q: "" };
const COLORS = ["#5865f2", "#d97757", "#10a37f", "#7c5cff", "#e5484d", "#3b82f6", "#f59e0b", "#14b8a6"];

// ---------- tiny helpers
function h(tag, attrs = {}, ...kids) {
  const e = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (k === "class") e.className = v;
    else if (k.startsWith("on")) e.addEventListener(k.slice(2), v);
    else if (v !== false && v != null) e.setAttribute(k, v === true ? "" : v);
  }
  for (const k of kids.flat()) if (k != null && k !== false) e.append(k instanceof Node ? k : String(k));
  return e;
}
const esc = (s) => s.replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
function hash(s) { let x = 0; for (const c of s) x = (x * 31 + c.codePointAt(0)) | 0; return Math.abs(x); }
const BRAND = { "claude-code": "#d97757", codex: "#10a37f", gemini: "#4285f4", qwen: "#615ced", goose: "#111111", opencode: "#3b82f6", hermes: "#7c5cff", dsh: "#4d6bfe", pi: "#e5484d" };
function colorOf(name) {
  const a = S.ov?.agents.find((a) => a.name === name);
  return BRAND[a?.template] ?? COLORS[hash(name) % COLORS.length];
}
function avatar(name, { group = false, small = false, live = false } = {}) {
  const el = h("div", { class: `av${small ? " sm" : ""}${group ? " group" : ""}` }, group ? "#" : [...name][0].toUpperCase());
  if (!group) el.style.background = colorOf(name);
  if (live) el.append(h("i", { class: "dot" }));
  return el;
}
function when(ts) {
  if (!ts) return "";
  const d = new Date(ts * 1000), n = new Date();
  return d.toDateString() === n.toDateString()
    ? d.toLocaleTimeString("zh-CN", { hour: "2-digit", minute: "2-digit" })
    : d.toLocaleDateString("zh-CN", { month: "numeric", day: "numeric" });
}
// Minimal, escape-first markdown: fenced code, inline code, bold, lists, paragraphs.
function md(src) {
  const parts = esc(src).split(/```(?:[\w-]*)\n?([\s\S]*?)```/g);
  return parts.map((p, i) => {
    if (i % 2) return `<pre><code>${p.replace(/\n$/, "")}</code></pre>`;
    return p.split(/\n{2,}/).map((blk) => {
      const inline = (t) => t.replace(/`([^`]+)`/g, "<code>$1</code>").replace(/\*\*([^*]+)\*\*/g, "<b>$1</b>");
      const lines = blk.split("\n").filter((l) => l.trim());
      if (!lines.length) return "";
      if (lines.every((l) => /^\s*[-*]\s/.test(l))) return `<ul>${lines.map((l) => `<li>${inline(l.replace(/^\s*[-*]\s/, ""))}</li>`).join("")}</ul>`;
      if (lines.every((l) => /^\s*\d+[.)]\s/.test(l))) return `<ol>${lines.map((l) => `<li>${inline(l.replace(/^\s*\d+[.)]\s/, ""))}</li>`).join("")}</ol>`;
      return `<p>${inline(lines.join("<br>"))}</p>`;
    }).join("");
  }).join("");
}
// In-app toast and confirm: WKWebView may swallow window.alert/confirm, which
// made errors invisible in earlier builds.
function toast(msg, ms = 4000, action) {
  const t = h("div", { class: "toast" }, String(msg), action ? h("button", { onclick: () => { t.remove(); action[1](); } }, action[0]) : null);
  document.body.append(t);
  setTimeout(() => t.remove(), ms);
}
function ask(question, okText = "确定") {
  return new Promise((resolve) => {
    const d = $("#dlg"), form = $("#dlg-form");
    form.replaceChildren(h("h3", {}, question), h("div", { class: "actions" },
      h("button", { class: "btn", type: "button", onclick: () => { d.close(); resolve(false); } }, "取消"),
      h("button", { class: "btn pri", type: "button", onclick: () => { d.close(); resolve(true); } }, okText)));
    form.onsubmit = (e) => e.preventDefault();
    d.showModal();
  });
}

// ---------- data
async function load() {
  S.ov = await invoke("overview");
  document.body.classList.toggle("paused", S.ov.paused);
  $("#paused-bar").hidden = !S.ov.paused;
  renderList();
}
const threadName = (t) => t.startsWith("dm:") ? t.slice(3) : (S.ov.groups.find((g) => `group:${g.id}` === t)?.name ?? "群聊");

// ---------- list column
function renderList() {
  const ul = $("#items"); ul.replaceChildren();
  $("#list-add").hidden = !(S.tab === "groups" || S.tab === "agents");
  const q = S.q.toLowerCase();
  const heads = Object.fromEntries(S.ov.heads.map((x) => [x.thread, x]));
  let rows = [];
  if (S.tab === "chats") {
    const threads = [...S.ov.agents.map((a) => `dm:${a.name}`), ...S.ov.groups.map((g) => `group:${g.id}`)];
    rows = threads.map((t) => ({ t, head: heads[t] })).sort((a, b) => (b.head?.ts ?? 0) - (a.head?.ts ?? 0))
      .map(({ t, head }) => threadRow(t, head));
  } else if (S.tab === "groups") {
    rows = S.ov.groups.map((g) => threadRow(`group:${g.id}`, heads[`group:${g.id}`], g.members.join("、")));
  } else if (S.tab === "agents") {
    rows = S.ov.agents.map((a) => h("li", { class: S.thread === `dm:${a.name}` ? "on" : "", onclick: () => openThread(`dm:${a.name}`) },
      avatar(a.name, { live: a.live }),
      h("div", { class: "meta" }, h("div", { class: "name" }, a.name), h("div", { class: "sub" }, a.detail)),
      h("button", { class: "more", title: "更多", onclick: (e) => { e.stopPropagation(); agentMenu(e.currentTarget, a.name); } }, "⋯")));
  } else {
    rows = [h("li", { class: "on" }, avatar("Discord", { small: true }), h("div", { class: "meta" }, h("div", { class: "name" }, "Discord"), h("div", { class: "sub" }, S.ov.discord_status)))];
  }
  rows = rows.filter((li) => !q || li.textContent.toLowerCase().includes(q));
  if (!rows.length) rows = [h("li", { class: "empty" }, S.tab === "agents" ? "还没有 agent，点 ＋ 添加" : S.tab === "groups" ? "还没有群聊，点 ＋ 新建" : "没有会话")];
  ul.append(...rows);
}
function threadRow(t, head, fallback) {
  const isGroup = t.startsWith("group:"); const name = threadName(t);
  const typing = S.typing[t]?.size ? [...S.typing[t]].join("、") : "";
  const live = !isGroup && S.ov.agents.find((a) => `dm:${a.name}` === t)?.live;
  return h("li", { class: S.thread === t ? "on" : "", onclick: () => openThread(t) },
    avatar(name, { group: isGroup, live }),
    h("div", { class: "meta" },
      h("div", { class: "row" }, h("span", { class: "name" }, name), h("span", { class: "time" }, when(head?.ts))),
      h("div", { class: `sub${typing ? " typing" : ""}` }, typing ? `${typing} 正在输入…` : head ? `${isGroup && head.author !== "me" ? head.author + "：" : ""}${head.body}` : (fallback ?? "")),
    ));
}

// ---------- conversation
async function openThread(t) {
  S.thread = t; renderList();
  const isGroup = t.startsWith("group:"); const name = threadName(t);
  const g = isGroup && S.ov.groups.find((g) => `group:${g.id}` === t);
  const a = !isGroup && S.ov.agents.find((a) => a.name === name);
  const sub = isGroup ? `${g.members.length} 个 agent · 组长 ${g.lead ?? "—"} · Discord 同步` : a ? `${a.kind.toUpperCase()} · ${a.detail}` : "";
  const pane = $("#pane");
  pane.replaceChildren(
    h("div", { class: "head", "data-tauri-drag-region": true }, avatar(name, { group: isGroup }),
      h("div", {}, h("h2", {}, name), h("small", {}, sub)), h("span", { class: "sp" }),
      a && a.kind === "acp" ? h("div", { id: "cfgs" }) : null,
      isGroup ? h("button", { class: "btn", onclick: () => groupDialog(g) }, "成员") : null,
      a ? h("button", { class: "icon-btn", title: "更多", onclick: (e) => agentMenu(e.currentTarget, a.name) }, "⋯") : null),
    h("div", { id: "log" }),
    h("div", { id: "composer" },
      h("textarea", { id: "input", rows: 1, placeholder: isGroup ? "发到群里：@名字 指定 agent，同时 @ 多个或 @all 并行执行" : `给 ${name} 发消息` }),
      h("div", { class: "hint" }, "↩ 发送 · ⇧↩ 换行")));
  const ta = $("#input");
  ta.addEventListener("input", () => { ta.style.height = "auto"; ta.style.height = ta.scrollHeight + "px"; });
  ta.addEventListener("keydown", (e) => {
    if (e.key === "Enter" && !e.shiftKey && !e.isComposing) { e.preventDefault(); sendMsg(); }
  });
  ta.focus();
  if (a && a.kind === "acp") loadConfigs(name, t);
  const msgs = await invoke("messages", { thread: t });
  if (S.thread !== t) return;
  for (const m of msgs) $("#log").append(bubble(m.author, m.body));
  for (const p of Object.values(S.perms)) if (p.thread === t) $("#log").append(permCard(p));
  for (const [k, v] of Object.entries(S.live)) if (k.startsWith(t + "|")) $("#log").append(v.el);
  scrollLog();
}
// Model picker: only the agent's own model list (ACP session config, category "model").
async function loadConfigs(agent, t) {
  const box = $("#cfgs");
  let list = [];
  try { list = await invoke("agent_configs", { agent }); } catch { list = []; }
  if (S.thread !== t || !box?.isConnected) return;
  const c = list.find((x) => x.category === "model") ?? list.find((x) => x.id === "model");
  if (!c || c.options.length < 2) return;
  const label = () => c.options.find(([v]) => v === c.current)?.[1] ?? c.current;
  const pill = h("button", { class: "model-pill", title: "切换模型" }, h("span", { class: "dot" }), h("span", { class: "lbl" }, label()), h("span", { class: "chev" }));
  pill.onclick = () => popover(pill, c.options.map(([v, name]) => ({
    label: name, on: v === c.current,
    run: async () => {
      if (v === c.current) return;
      const prev = c.current; c.current = v; pill.querySelector(".lbl").textContent = label(); pill.classList.add("busy");
      try { await invoke("set_agent_config", { agent, id: c.id, value: v }); }
      catch (e) { c.current = prev; pill.querySelector(".lbl").textContent = label(); toast(e); }
      pill.classList.remove("busy");
    },
  })));
  box.replaceChildren(pill);
}

// A small anchored menu; closes on outside click or Esc.
function popover(anchor, items) {
  document.querySelector(".pop")?.remove();
  const pop = h("div", { class: "pop" }, ...items.map((it) => h("button", {
    class: `pop-item${it.on ? " on" : ""}${it.danger ? " danger" : ""}`,
    onclick: () => { close(); it.run(); },
  }, h("span", {}, it.label), it.on ? h("span", { class: "pop-check" }) : null)));
  document.body.append(pop);
  const r = anchor.getBoundingClientRect();
  pop.style.top = `${r.bottom + 6}px`;
  pop.style.left = `${Math.max(8, Math.min(r.right - pop.offsetWidth, innerWidth - pop.offsetWidth - 8))}px`;
  const close = () => { pop.remove(); removeEventListener("mousedown", out, true); removeEventListener("keydown", esc, true); };
  const out = (e) => { if (!pop.contains(e.target) && !anchor.contains(e.target)) close(); };
  const esc = (e) => { if (e.key === "Escape") close(); };
  addEventListener("mousedown", out, true); addEventListener("keydown", esc, true);
}
function agentMenu(anchor, name) {
  popover(anchor, [{ label: "删除 Agent…", danger: true, run: () => removeAgent(name) }]);
}
function bubble(author, body) {
  if (author === "system") return h("div", { class: "m sys" }, body);
  const me = author === "me";
  const b = h("div", { class: "b" }); b.innerHTML = md(body);
  const col = h("div", {}, !me && S.thread?.startsWith("group:") ? h("div", { class: "who" }, author) : null, b);
  return h("div", { class: `m${me ? " me" : ""}` }, me ? null : avatar(author, { small: true }), col);
}
function scrollLog() { const l = $("#log"); if (l) l.scrollTop = l.scrollHeight; }
async function sendMsg() {
  const ta = $("#input"); const text = ta.value.trim();
  if (!text || !S.thread) return;
  ta.value = ""; ta.style.height = "auto";
  try { await invoke("send", { thread: S.thread, text }); } catch (e) { toast(e); }
}

function permCard(p) {
  return h("div", { class: "perm", id: `perm-${p.request_id}` },
    h("b", {}, `${p.agent} 请求执行：${p.title || "工具调用"}`),
    h("div", { class: "opts" }, ...p.options.map((o) => h("button", { class: o.allow ? "allow" : "", onclick: () => decide(p, o.id) }, o.label)),
      h("button", { onclick: () => decide(p, null) }, "取消")));
}
async function decide(p, optionId) {
  // request ids contain "/" etc., so look them up by id, not with a CSS selector.
  const card = document.getElementById(`perm-${p.request_id}`);
  card?.querySelectorAll("button").forEach((b) => { b.disabled = true; });
  try {
    const ok = await invoke("decide_permission", { agent: p.agent, requestId: p.request_id, optionId });
    delete S.perms[p.request_id]; card?.remove();
    if (!ok) toast("这个请求已经失效（agent 已结束这一轮）");
  } catch (e) { card?.querySelectorAll("button").forEach((b) => { b.disabled = false; }); toast(e); }
}

// ---------- events
const liveKey = (thread, agent) => `${thread}|${agent}`;
listen("message", ({ payload: m }) => {
  const k = liveKey(m.thread, m.author);
  if (S.live[k]) { S.live[k].el.remove(); delete S.live[k]; }
  const head = S.ov.heads.find((x) => x.thread === m.thread);
  if (head) Object.assign(head, m); else S.ov.heads.unshift(m);
  if (S.thread === m.thread) { $("#log").append(bubble(m.author, m.body)); scrollLog(); }
  renderList();
});
listen("typing", ({ payload: { thread, agent, on } }) => {
  const set = (S.typing[thread] ??= new Set());
  on ? set.add(agent) : set.delete(agent);
  const k = liveKey(thread, agent);
  if (on && !S.live[k]) {
    const b = h("div", { class: "b" }, h("span", { class: "typing-dots" }, h("i"), h("i"), h("i")));
    S.live[k] = { text: "", b, el: h("div", { class: "m" }, avatar(agent, { small: true }), h("div", {}, h("div", { class: "who" }, agent), b)) };
    if (S.thread === thread) { $("#log").append(S.live[k].el); scrollLog(); }
  }
  if (!on && S.live[k] && !S.live[k].text) { S.live[k].el.remove(); delete S.live[k]; }
  renderList();
});
listen("stream", ({ payload: { thread, agent, event } }) => {
  const live = S.live[liveKey(thread, agent)];
  if (event.type === "text_chunk" && live) {
    live.text += event.text;
    // Re-render at most once per frame: long answers arrive in hundreds of chunks.
    if (!live.raf) live.raf = requestAnimationFrame(() => {
      live.raf = 0; live.b.innerHTML = md(live.text);
      if (S.thread === thread) scrollLog();
    });
  } else if (event.type === "tool" && S.thread === thread && event.title) {
    const chip = h("div", { class: "chip" }, `🔧 ${agent} · ${event.title}`);
    live ? live.el.before(chip) : $("#log").append(chip);
  } else if (event.type === "permission_asked") {
    const p = { ...event, agent, thread }; S.perms[event.request_id] = p;
    if (S.thread === thread) { $("#log").append(permCard(p)); scrollLog(); }
  }
});
async function installUpdate() {
  $("#update-bar")?.replaceChildren("正在下载更新…");
  try { await invoke("install_update"); } catch (e) { toast(`更新失败：${e}`); }
}
listen("update-available", ({ payload: u }) => {
  if ($("#update-bar")) return;
  $("#items").before(h("div", { id: "update-bar" }, `新版本 ${u.version} 可用 `, h("button", { onclick: installUpdate }, "更新并重启")));
});
listen("discord-status", ({ payload }) => { if (S.ov) { S.ov.discord_status = payload; if (S.tab === "settings") renderSettings(); renderList(); } });

// ---------- agents page
const shortPath = (p) => (p || "").replace(/^\/Users\/[^/]+/, "~");
const baseName = (p) => (p ? p.replace(/\/$/, "").split("/").pop() : "");
async function renderGallery() {
  S.thread = null; renderList();
  const page = h("div", { class: "page add" }, h("h1", {}, "添加 Agent"), h("p", {}, "正在扫描这台 Mac…"));
  $("#pane").replaceChildren(page);
  const [found, envs] = await Promise.all([invoke("detect_agents").catch(() => []), invoke("conda_envs").catch(() => [])]);
  S.envs = envs;
  const have = found.filter((t) => t.found), miss = found.filter((t) => !t.found);
  const list = h("div", { class: "alist" },
    ...have.map((t) => agentRow({ ...t, kind: "acp" }, shortPath(t.found))),
    agentRow({ id: "command", name: "其他本地 agent", kind: "command", color: "#8a8a8e", letter: "›" }, "任意支持 ACP 的程序"),
    agentRow({ id: "a2a", name: "远程 A2A agent", kind: "a2a", color: "#5865f2", letter: "A" }, "通过 URL 接入"));
  page.replaceChildren(h("h1", {}, "添加 Agent"),
    h("p", {}, have.length ? `在这台 Mac 上找到 ${have.length} 个，点一下就能导入。` : "没在这台 Mac 上找到已安装的 agent，可以手动添加。"),
    list,
    miss.length ? h("p", { class: "muted" }, `装好后会自动出现：${miss.map((t) => t.name).join(" · ")}`) : null);
}
function agentRow(t, sub) {
  const row = h("div", { class: "arow" });
  const head = h("button", { class: "ahead", onclick: () => {
    const open = row.classList.toggle("open");
    for (const o of document.querySelectorAll(".arow.open")) if (o !== row) o.classList.remove("open");
    if (open && !row.querySelector(".aform")) row.append(agentForm(t));
    if (open) row.querySelector(".aform input")?.focus();
  } },
    Object.assign(h("div", { class: "av" }, t.letter ?? t.name[0].toUpperCase()), { style: `background:${t.color}` }),
    h("div", { class: "ameta" }, h("b", {}, t.name), h("small", {}, sub)),
    h("span", { class: "aplus" }, t.kind === "acp" ? "导入" : "添加"));
  row.append(head);
  return row;
}
function agentForm(t) {
  const st = { cwd: "", conda: "", count: 1 };
  const err = h("div", { class: "err" });
  const name = h("input", { class: "fld", value: t.kind === "acp" ? t.id : "", placeholder: "名称，如 coder", spellcheck: false });
  const parts = [h("div", { class: "frow" }, h("span", {}, "名称"), name)];
  let cmd, url, hn, hv;
  if (t.kind === "command") {
    cmd = h("input", { class: "fld", placeholder: "/path/to/agent --acp", spellcheck: false });
    parts.push(h("div", { class: "frow" }, h("span", {}, "程序"), cmd, h("button", { type: "button", class: "ghost", onclick: async () => {
      const p = await invoke("choose_path", { file: true }); if (p) cmd.value = /\s/.test(p) ? `"${p}"` : p;
    } }, "选择…")));
  }
  if (t.kind === "a2a") {
    url = h("input", { class: "fld", placeholder: "https://agent.example.com", spellcheck: false });
    hn = h("input", { class: "fld", placeholder: "鉴权头（可选），如 authorization" });
    hv = h("input", { class: "fld", type: "password", placeholder: "鉴权值（可选，存钥匙串）" });
    parts.push(h("div", { class: "frow" }, h("span", {}, "URL"), url), h("div", { class: "frow" }, h("span", {}, "鉴权"), hn, hv));
  }
  if (t.kind !== "a2a") {
    const folder = h("button", { type: "button", class: "pill" }, "主目录");
    folder.onclick = async () => { const p = await invoke("choose_path", {}); if (p) { st.cwd = p; folder.textContent = baseName(p); folder.title = p; folder.classList.add("set"); } };
    const chips = h("div", { class: "chips" });
    const opts = [{ name: "系统", path: "" }, ...(S.envs || [])];
    const draw = () => chips.replaceChildren(...opts.map((e) => h("button", { type: "button", class: "chip2" + (st.conda === e.path ? " on" : ""), title: e.path, onclick: () => { st.conda = e.path; draw(); } }, e.name)));
    draw();
    const count = h("div", { class: "stepper" },
      h("button", { type: "button", onclick: () => { st.count = Math.max(1, st.count - 1); n.textContent = st.count; } }, "−"),
      h("span", {}, "1"),
      h("button", { type: "button", onclick: () => { st.count = Math.min(8, st.count + 1); n.textContent = st.count; } }, "+"));
    const n = count.querySelector("span");
    parts.push(
      h("div", { class: "frow" }, h("span", {}, "目录"), folder),
      h("div", { class: "frow" }, h("span", {}, "环境"), chips),
      h("div", { class: "frow" }, h("span", {}, "数量"), count, h("small", { class: "hint2" }, "多个实例各用独立 git worktree")));
  }
  const go = h("button", { type: "button", class: "btn pri" }, t.kind === "acp" ? "导入" : "添加");
  go.onclick = async () => {
    go.disabled = true; err.textContent = "";
    try {
      await invoke("add_agent", { spec: { template: t.id, name: name.value, url: url?.value ?? null, headerName: hn?.value || null, headerValue: hv?.value || null,
        command: cmd?.value ?? null, cwd: st.cwd || null, count: st.count, worktree: st.count > 1, conda: st.conda || null } });
      await load(); setTab("chats"); openThread(`dm:${name.value.trim()}`);
    } catch (e) { err.textContent = String(e); go.disabled = false; }
  };
  return h("div", { class: "aform" }, ...parts, h("div", { class: "factions" }, err, go));
}
async function removeAgent(name) {
  const d = $("#dlg"), form = $("#dlg-form");
  const purge = h("input", { type: "checkbox" }); purge.checked = true;
  const go = h("button", { class: "btn pri danger-fill", type: "button" }, "删除");
  form.replaceChildren(
    h("div", { class: "del-head" }, avatar(name), h("div", {}, h("h3", {}, `删除 ${name}？`),
      h("p", { class: "del-sub" }, "会停止它的进程，把它移出所有群聊，并删除它的 Discord 频道。"))),
    h("label", { class: "del-opt" }, purge, h("span", {}, "同时清空和它的聊天记录")),
    h("div", { class: "actions" }, h("button", { class: "btn", type: "button", onclick: () => d.close() }, "取消"), go));
  form.onsubmit = (e) => e.preventDefault();
  go.onclick = async () => {
    go.disabled = true;
    try {
      await invoke("remove_agent", { name, purge: purge.checked });
      d.close(); if (S.thread === `dm:${name}`) blank(); await load(); toast(`已删除 ${name}`);
    } catch (e) { go.disabled = false; toast(e); }
  };
  d.showModal();
}

// ---------- groups
async function groupDialog(g) {
  if (!S.ov.agents.length) return toast("先添加至少一个 agent", 6000, ["去添加", () => setTab("agents")]);
  S.thread = null; renderList();
  const st = { members: [...(g?.members ?? [])], lead: g?.lead ?? null, channel: g ? `${g.guild_id}|${g.id}` : "" };
  const name = h("input", { class: "gname", value: g?.name ?? "", placeholder: "群名", spellcheck: false });
  const grid = h("div", { class: "pick" });
  const count = h("span", { class: "muted2" });
  const draw = () => {
    if (st.lead && !st.members.includes(st.lead)) st.lead = null;
    const lead = st.lead ?? st.members[0];
    grid.replaceChildren(...S.ov.agents.map((a) => {
      const on = st.members.includes(a.name);
      const tile = h("button", { type: "button", class: "ptile" + (on ? " on" : ""), onclick: () => {
        st.members = on ? st.members.filter((m) => m !== a.name) : [...st.members, a.name]; draw();
      } }, avatar(a.name), h("span", {}, a.name), on ? h("i", { class: "tick" }, "✓") : null,
        on && a.name === lead ? h("em", { class: "crown" }, "组长") : null);
      if (on && a.name !== lead) tile.append(h("em", { class: "mklead", onclick: (e) => { e.stopPropagation(); st.lead = a.name; draw(); } }, "设为组长"));
      return tile;
    }));
    count.textContent = st.members.length ? `已选 ${st.members.length} 个` : "点选成员";
  };
  draw();
  const where = h("div", { class: "gwhere" });
  const discordOn = /^在线/.test(S.ov.discord_status || "");
  if (g) where.append(h("small", {}, g.id.startsWith("local-") ? "仅在 Disco 内" : "已绑定 Discord 频道"));
  else if (discordOn) {
    const sel = h("select", { class: "fld", onchange: () => { st.channel = sel.value; } }, h("option", { value: "" }, "自动新建同名频道"));
    where.append(h("small", {}, "Discord"), sel);
    invoke("discord_channels").then((cs) => sel.append(...cs.map((c) => h("option", { value: `${c.guild_id}|${c.id}` }, `用已有 #${c.name} · ${c.guild}`)))).catch(() => {});
  } else where.append(h("small", {}, "Discord 未连接，群聊先只在 Disco 内进行"));
  const err = h("div", { class: "err" });
  const go = h("button", { class: "btn pri" }, g ? "保存" : "创建群聊");
  go.onclick = async () => {
    go.disabled = true; err.textContent = "";
    try {
      const [guildId, channelId] = st.channel ? st.channel.split("|") : [null, null];
      const id = await invoke("save_group", { channelId, guildId, name: name.value, members: st.members, lead: st.lead ?? st.members[0] ?? null });
      await load(); setTab("groups"); openThread(`group:${id}`);
    } catch (e) { err.textContent = String(e); go.disabled = false; }
  };
  const del = g ? h("button", { class: "btn danger", onclick: async () => {
    if (await ask("删除这个群聊？聊天记录保留，Discord 频道不受影响。", "删除")) { await invoke("delete_group", { id: g.id }); blank(); await load(); }
  } }, "删除群聊") : null;
  $("#pane").replaceChildren(h("div", { class: "page gpage" },
    h("h1", {}, g ? "群设置" : "新建群聊"), name,
    h("div", { class: "glabel" }, h("b", {}, "成员"), count), grid, where,
    h("div", { class: "factions" }, del, err, go)));
  if (!g) name.focus();
}

// ---------- settings
async function loadMemories(box) {
  const ms = await invoke("memories").catch(() => []);
  box.replaceChildren(...(ms.length ? ms.map((m) => h("li", {}, h("b", {}, m.scope === "all" ? "所有 agent" : m.scope), h("span", { style: "flex:1" }, m.text),
    h("button", { class: "btn", onclick: async () => { await invoke("forget", { key: m.key }); loadMemories(box); } }, "删除"))) : [h("li", {}, h("span", {}, "还没有记忆"))]));
}
const inputStyle = "flex:1;height:32px;border:1px solid var(--line);border-radius:8px;padding:0 10px;background:var(--list);color:var(--fg)";
function renderSettings() {
  S.thread = null;
  const ok = /^在线/.test(S.ov.discord_status);
  const discordBox = h("div", {});
  if (S.ov.discord_token) loadDiscordSettings(discordBox);
  $("#pane").replaceChildren(h("div", { class: "page" },
    h("h1", {}, "设置"), h("p", {}, "密钥只存在 macOS 钥匙串里。"),
    h("div", { class: "section" }, h("h3", {}, "Discord"),
      h("div", { class: `status${ok ? " ok" : ""}` }, h("i"), S.ov.discord_status),
      h("form", { style: "display:flex;gap:8px;margin-top:12px", onsubmit: async (e) => {
        e.preventDefault(); const tok = e.target.token.value; const b = e.target.querySelector("button");
        b.disabled = true; b.textContent = "验证中…";
        try { await invoke("save_discord_token", { token: tok }); e.target.reset(); toast("token 有效，已保存"); await load(); renderSettings(); }
        catch (er) { toast(er, 8000); b.disabled = false; b.textContent = "保存"; }
      } }, h("input", { name: "token", type: "password", required: true, placeholder: S.ov.discord_token ? "已保存 · 输入新 token 替换" : "粘贴 Bot token", style: inputStyle }),
        h("button", { class: "btn pri" }, "保存")),
      S.ov.discord_token ? h("button", { class: "btn", style: "margin-top:8px", onclick: async () => { try { await invoke("discord_connect"); } catch (e) { toast(e); } } }, "重新连接") : null,
      discordBox),
    h("div", { class: "section" }, h("h3", {}, "记忆"),
      h("p", { style: "margin:0 0 8px;color:var(--mute)" }, "在私聊里说「记住 …」存给这个 agent，在群聊里说存给所有 agent；agent 自己也能用「【记住】」开头的一行记下东西。每次新会话自动带上。"),
      (() => { const box = h("ul", { class: "checks-list" }); loadMemories(box); return box; })()),
    h("div", { class: "section" }, h("h3", {}, "诊断"),
      (() => {
        const list = h("ul", { class: "checks-list" });
        const btn = h("button", { class: "btn", onclick: async () => {
          btn.disabled = true; btn.textContent = "检查中…（约 15 秒）"; list.replaceChildren();
          try {
            for (const c of await invoke("diagnose")) list.append(h("li", { class: c.ok ? "ok" : "bad" }, h("b", {}, `${c.ok ? "✓" : "✗"} ${c.name}`), h("span", {}, c.detail)));
          } catch (e) { list.append(h("li", { class: "bad" }, String(e))); }
          btn.disabled = false; btn.textContent = "重新检查";
        } }, "一键诊断");
        return h("div", {}, h("p", { style: "margin:0 0 8px;color:var(--mute)" }, "逐项检查代理、钥匙串、Discord 连接与权限、Node.js 和每个 agent 的启动命令。"), btn, list);
      })()),
    h("div", { class: "section" }, h("h3", {}, "更新"),
      (() => {
        const line = h("p", { style: "margin:0 0 8px;color:var(--mute)" }, "正在检查…");
        const btn = h("button", { class: "btn pri", hidden: true, onclick: installUpdate }, "更新并重启");
        invoke("check_update").then((u) => {
          line.textContent = u.version ? `当前 ${u.current}，新版本 ${u.version} 可用` : `当前 ${u.current}，已是最新版本`;
          btn.hidden = !u.version;
        }).catch((e) => { line.textContent = `检查更新失败：${e}`; });
        return h("div", {}, line, btn);
      })()),
    h("div", { class: "section" }, h("h3", {}, "安全"),
      h("p", { style: "margin:0;color:var(--mute)" }, "左下角电源键是一键停止：立刻结束所有本地 agent 进程并暂停所有路由，所有操作写入审计日志。"))));
}

async function loadDiscordSettings(box) {
  let d;
  try { d = await invoke("discord_settings", {}); } catch (e) { box.replaceChildren(h("p", { class: "err" }, String(e))); return; }
  const guild = h("select", { style: inputStyle, onchange: async () => {
    try { await invoke("discord_settings", { homeGuild: guild.value }); toast("已切换主服务器，正在为每个 agent 建频道"); } catch (e) { toast(e); }
  } }, h("option", { value: "" }, d.guilds.length ? "选择服务器" : "Bot 还没加入任何服务器"),
    ...d.guilds.map((g) => h("option", { value: g.id, selected: g.id === d.home_guild }, g.guild)));
  const allow = h("input", { value: d.allow, placeholder: "其他可指挥 agent 的 Discord 用户 ID，逗号分隔", style: inputStyle });
  box.replaceChildren(
    h("p", { style: "margin:14px 0 6px;color:var(--mute)" }, "主服务器：每个 agent 会在「Disco Agents」分类下有自己的频道，在频道里直接对它说话。"),
    h("div", { style: "display:flex;gap:8px" }, guild,
      h("button", { class: "btn", onclick: async (e) => {
        e.target.disabled = true;
        try { const n = await invoke("discord_sync"); toast(`已同步 ${n} 个 agent 频道`); } catch (er) { toast(er, 8000); }
        e.target.disabled = false;
      } }, "同步频道")),
    d.invite_url ? h("button", { class: "btn", style: "margin-top:8px", onclick: () => invoke("open_url", { url: d.invite_url }).catch(toast) }, "重新授权 Bot（补齐管理频道等权限）") : null,
    h("p", { style: "margin:14px 0 6px;color:var(--mute)" }, "只有 Bot 所有者能在 Discord 里指挥 agent。要允许其他人，填他们的用户 ID："),
    h("div", { style: "display:flex;gap:8px" }, allow,
      h("button", { class: "btn", onclick: async () => {
        try { const r = await invoke("discord_settings", { allow: allow.value }); allow.value = r.allow; toast("已保存"); } catch (e) { toast(e); }
      } }, "保存")));
}

// ---------- shell
function dialog(fields, okText, onOk, extra) {
  const d = $("#dlg"), form = $("#dlg-form");
  const errEl = h("div", { class: "err" });
  const ok = h("button", { class: "btn pri", value: "ok" }, okText);
  form.replaceChildren(...fields, errEl, h("div", { class: "actions" },
    extra ? h("button", { class: "btn", type: "button", style: "margin-right:auto;color:var(--red)", onclick: async () => { d.close(); await extra[1](); } }, extra[0]) : null,
    h("button", { class: "btn", value: "cancel", formnovalidate: true }, "取消"), ok));
  form.onsubmit = async (e) => {
    if (e.submitter?.value !== "ok") return;
    e.preventDefault(); ok.disabled = true; errEl.textContent = "";
    try { await onOk(Object.fromEntries(new FormData(form)), form); d.close(); } catch (er) { errEl.textContent = String(er); }
    ok.disabled = false;
  };
  d.showModal();
}
function blank() {
  S.thread = null;
  $("#pane").replaceChildren(h("div", { class: "blank" }, h("b", {}, "Disco"), "选一个会话开始，或去 Agent 页添加第一个 agent"));
  renderList();
}
function setTab(tab) {
  S.tab = tab;
  document.querySelectorAll(".tab").forEach((b) => b.classList.toggle("on", b.dataset.tab === tab));
  if (tab === "agents") renderGallery();
  else if (tab === "settings") renderSettings();
  else if (!S.thread) blank();
  renderList();
}
document.querySelectorAll(".tab").forEach((b) => b.addEventListener("click", () => setTab(b.dataset.tab)));
$("#list-add").onclick = () => (S.tab === "agents" ? renderGallery() : groupDialog(null));
$("#search").oninput = (e) => { S.q = e.target.value; renderList(); };
$("#kill").onclick = async () => {
  if (S.ov.paused) return;
  if (!(await ask("立即停止所有 agent 并暂停路由？", "全部停止"))) return;
  const n = await invoke("stop_all"); await load(); toast(`已停止 ${n} 个运行中的 agent`);
};
$("#resume").onclick = async () => { await invoke("resume"); await load(); };

(async () => { await load(); blank(); })();
