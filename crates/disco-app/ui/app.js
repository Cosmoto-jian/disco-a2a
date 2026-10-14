"use strict";
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const $ = (s, r = document) => r.querySelector(s);

// ---------- state
// typing: thread -> Set(agent); live: "thread|agent" -> streaming bubble
// msgs: thread -> cached messages, so switching chats renders instantly.
// status: "thread|agent" -> { st: "think" | "tool", tool }; "wait" comes from the inbox.
const S = { teamMode: {}, tab: "chats", thread: null, ov: null, tpl: [], typing: {}, live: {}, inbox: [], team: {}, q: "", msgs: {}, found: null, status: {}, cmds: {}, unread: {} };
const REDUCED = matchMedia("(prefers-reduced-motion: reduce)").matches;
const COLORS = ["#5865f2", "#d97757", "#10a37f", "#7c5cff", "#e5484d", "#3b82f6", "#f59e0b", "#14b8a6"];

// ---------- tiny helpers
function h(tag, attrs = {}, ...kids) {
  const e = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (k === "class") e.className = v;
    else if (k === "html") e.innerHTML = v;
    else if (k.startsWith("on")) e.addEventListener(k.slice(2), v);
    else if (v !== false && v != null) e.setAttribute(k, v === true ? "" : v);
  }
  for (const k of kids.flat()) if (k != null && k !== false) e.append(k instanceof Node ? k : String(k));
  return e;
}
const esc = (s) => s.replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
function hash(s) { let x = 0; for (const c of s) x = (x * 31 + c.codePointAt(0)) | 0; return Math.abs(x); }
const IS_WIN = navigator.userAgent.includes("Windows");
if (IS_WIN) document.documentElement.classList.add("win");
const BRAND = { "claude-code": "#d97757", codex: "#10a37f", gemini: "#4285f4", qwen: "#615ced", goose: "#111111", opencode: "#3b82f6", hermes: "#7c5cff", dsh: "#4d6bfe", pi: "#e5484d" };
function colorOf(name) {
  const a = S.ov?.agents.find((a) => a.name === name);
  return BRAND[a?.template] ?? COLORS[hash(name) % COLORS.length];
}
// ---------- avatars: small geometric creatures, white shapes on a two-tone gradient.
// Stored per agent as "<form>.<palette>"; empty = form from the name, palette from the brand.
const PALETTES = { brand: null, ember: ["#ff8a4c", "#e5484d"], sea: ["#22d3ee", "#3b82f6"], moss: ["#34d399", "#0f9f6e"], dusk: ["#a78bfa", "#6d28d9"],
  candy: ["#f472b6", "#a855f7"], sun: ["#fbbf24", "#f97316"], ink: ["#475569", "#0f172a"], mint: ["#5eead4", "#0d9488"], rose: ["#fda4af", "#e11d48"] };
const PAD = '<circle cx="20" cy="20" r="15" fill="none"/>';
const FORMS = {
  orbit: { label: "轨道", svg: `<circle cx="20" cy="20" r="7"/><g class="spin">${PAD}<circle cx="20" cy="7.5" r="3" opacity=".9"/></g><circle cx="20" cy="20" r="12.5" fill="none" stroke="#fff" stroke-opacity=".28" stroke-width="1.2"/>` },
  bloom: { label: "花瓣", svg: `<g class="spin slow">${PAD}` + [0, 60, 120].map((d) => `<ellipse cx="20" cy="20" rx="4.6" ry="13" opacity=".5" transform="rotate(${d} 20 20)"/>`).join("") + '</g><circle cx="20" cy="20" r="3.4"/>' },
  ripple: { label: "涟漪", svg: [0, 1, 2].map((i) => `<circle class="ring" style="animation-delay:${-i}s" cx="20" cy="20" r="12" fill="none" stroke="#fff" stroke-width="2"/>`).join("") + '<circle cx="20" cy="20" r="4"/>' },
  prism: { label: "棱镜", svg: `<g class="spin slow">${PAD}<path d="M20 7 31.3 26.5H8.7Z" opacity=".55"/></g><g class="spin rev">${PAD}<path d="M20 33 8.7 13.5H31.3Z" opacity=".55"/></g>` },
  dots: { label: "星点", svg: [8, 20, 32].flatMap((y, r) => [8, 20, 32].map((x, c) => `<circle class="tw" style="animation-delay:${-((r * 3 + c) * 0.37) % 2.2}s" cx="${x}" cy="${y}" r="3.2"/>`)).join("") },
  wave: { label: "声波", svg: [9, 16, 23, 30].map((x, i) => `<rect class="bar" style="animation-delay:${-i * 0.28}s" x="${x - 2}" y="9" width="4" height="22" rx="2"/>`).join("") },
  duo: { label: "双子", svg: '<circle class="drift" cx="15" cy="20" r="8" opacity=".7"/><circle class="drift rev" cx="25" cy="20" r="8" opacity=".7"/>' },
  tile: { label: "方块", svg: `<g class="spin slow">${PAD}<rect x="10" y="10" width="20" height="20" rx="4" opacity=".45"/></g><g class="spin rev">${PAD}<rect x="13.5" y="13.5" width="13" height="13" rx="3" transform="rotate(45 20 20)"/></g>` },
};
const FORM_KEYS = Object.keys(FORMS);
function avatarSpec(name) {
  const a = S.ov?.agents.find((a) => a.name === name);
  const [f, p] = (a?.avatar || "").split(".");
  return { form: FORMS[f] ? f : FORM_KEYS[hash(name) % FORM_KEYS.length], palette: p in PALETTES ? p : "brand" };
}
function paint(el, name, form, palette, base = colorOf(name)) {
  const [c1, c2] = PALETTES[palette] ?? [`color-mix(in srgb, ${base} 70%, #fff)`, base];
  el.style.background = `linear-gradient(140deg, ${c1}, ${c2})`;
  el.insertAdjacentHTML("afterbegin", `<svg viewBox="0 0 40 40" fill="#fff" aria-hidden="true">${(FORMS[form] ?? FORMS.orbit).svg}</svg>`);
  el.dataset.form = form;
  return el;
}
function avatar(name, { group = false, small = false, live = false, spec, status } = {}) {
  const el = h("div", { class: `av${small ? " sm" : ""}${group ? " group" : ""}${live ? " live" : ""}` }, group ? "#" : null);
  if (!group) { const sp = spec ?? avatarSpec(name); paint(el, name, sp.form, sp.palette, sp.base); }
  if (live) el.append(h("i", { class: "dot" }));
  if (status) { el.dataset.st = status.st; el.title = status.label; el.append(h("i", { class: "st" })); }
  return el;
}
// What an agent is doing in a thread right now: thinking, running a tool, or waiting on you.
const ST_LABEL = { think: "思考中", tool: "调工具", wait: "等你确认" };
function statusOf(thread, agent) {
  if (S.inbox.some((it) => it.thread === thread && it.agent === agent)) return { st: "wait", label: `${agent} · 等你确认` };
  const s = S.status[`${thread}|${agent}`];
  return s ? { st: s.st, label: `${agent} · ${ST_LABEL[s.st]}${s.tool ? "：" + s.tool : ""}` } : null;
}
// Agents replying right now, across every chat (for the rail count and busy rows).
const busyAgents = () => new Set(Object.values(S.typing).flatMap((set) => [...set]));
const OFFICIAL = new Set(["claude-code", "codex", "gemini", "qwen", "goose", "opencode", "hermes", "pi"]);
function setStatus(thread, agent, st, tool) {
  const k = `${thread}|${agent}`;
  if (st) S.status[k] = { st, tool }; else delete S.status[k];
  if (S.thread === thread) { drawMembers(); syncComposer(); }
  renderList();
}
// Form × palette picker; calls onPick("form.palette") on every change.
function avatarPicker(name, value, onPick) {
  const st = { ...value }, base = value.base ?? colorOf(name);
  const forms = h("div", { class: "avp-forms" }), cols = h("div", { class: "avp-cols" });
  const draw = () => {
    forms.replaceChildren(...FORM_KEYS.map((f) => h("button", { type: "button", class: "avp" + (st.form === f ? " on" : ""), title: FORMS[f].label,
      onclick: () => { st.form = f; draw(); onPick(`${st.form}.${st.palette}`); } }, avatar(name, { spec: { form: f, palette: st.palette, base } }), h("small", {}, FORMS[f].label))));
    cols.replaceChildren(...Object.keys(PALETTES).map((p) => {
      const sw = h("button", { type: "button", class: "sw" + (st.palette === p ? " on" : ""), title: p === "brand" ? "默认配色" : p,
        onclick: () => { st.palette = p; draw(); onPick(`${st.form}.${st.palette}`); } });
      const [c1, c2] = PALETTES[p] ?? [`color-mix(in srgb, ${base} 70%, #fff)`, base];
      sw.style.background = `linear-gradient(140deg, ${c1}, ${c2})`;
      return sw;
    }));
  };
  draw();
  return h("div", { class: "avpick" }, forms, cols);
}
function avatarDialog(name) {
  let pick = null;
  const cur = avatarSpec(name);
  const preview = h("div", { class: "avp-hero" }, avatar(name, { spec: cur }));
  const picker = avatarPicker(name, cur, (v) => { pick = v; const [form, palette] = v.split("."); preview.replaceChildren(avatar(name, { spec: { form, palette } })); });
  dialog([h("h3", {}, `${name} 的形象`), preview, picker], "保存", async () => {
    if (pick) { await invoke("set_avatar", { name, avatar: pick }); await load(); if (S.thread === `dm:${name}`) { S.thread = null; openThread(`dm:${name}`); } }
  });
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
    if (i % 2) return `<pre><button class="copy" type="button">复制</button><code>${p.replace(/\n$/, "")}</code></pre>`;
    return p.replace(/^(#{1,6} .*)$/gm, "\n$1\n").split(/\n{2,}/).map((blk) => {
      const inline = (t) => t.replace(/`([^`]+)`/g, "<code>$1</code>").replace(/\*\*([^*]+)\*\*/g, "<b>$1</b>");
      const lines = blk.split("\n").filter((l) => l.trim());
      if (!lines.length) return "";
      if (lines.length === 1 && /^#{1,6} /.test(lines[0])) return `<h4>${inline(lines[0].replace(/^#+ /, ""))}</h4>`;
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
  document.querySelector(".toast")?.remove();
  document.body.append(t);
  setTimeout(() => { t.classList.add("out"); setTimeout(() => t.remove(), 200); }, ms);
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
  drawKill();
  renderList();
}
const peerOf = (t) => S.ov.peers.find((p) => p.thread === t);
const threadName = (t) => t.startsWith("dm:") ? t.slice(3)
  : t.startsWith("peer:") ? (peerOf(t) ? `${peerOf(t).guest} → ${peerOf(t).agent}` : "共享")
  : (S.ov.groups.find((g) => `group:${g.id}` === t)?.name ?? "群聊");

// ---------- list column
// Coalesce list redraws: typing/stream events can fire many times per frame.
let listRaf = 0;
function renderList() {
  if (!listRaf) listRaf = requestAnimationFrame(() => { listRaf = 0; drawList(); });
}
function drawList() {
  const ul = $("#items"); ul.replaceChildren();
  // Same header on every tab: ＋ keeps its place (just goes quiet) so the search box never jumps.
  const add = $("#list-add"); add.classList.toggle("off", S.tab === "inbox" || S.tab === "settings");
  add.title = S.tab === "agents" ? `添加 agent（${MOD}N）` : `新建群聊（${MOD}N）`;
  const q = S.q.toLowerCase();
  const heads = Object.fromEntries(S.ov.heads.map((x) => [x.thread, x]));
  let rows = [];
  if (S.tab === "chats") {
    const threads = [...S.ov.agents.map((a) => `dm:${a.name}`), ...S.ov.groups.map((g) => `group:${g.id}`),
      ...S.ov.peers.map((p) => p.thread).filter((t) => heads[t])];
    rows = threads.map((t) => ({ t, head: heads[t] })).sort((a, b) => (b.head?.ts ?? 0) - (a.head?.ts ?? 0))
      .map(({ t, head }) => threadRow(t, head));
  } else if (S.tab === "groups") {
    rows = [...S.ov.groups.map((g) => threadRow(`group:${g.id}`, heads[`group:${g.id}`], g.members.join("、"))),
      // A friend's Discord-synced group your shared agent is in counts as a group too.
      ...S.ov.peers.filter((p) => p.group).map((p) => threadRow(p.thread, heads[p.thread], p.talk ? `${p.guest} 的群 · 可以说话` : `${p.guest} 的群 · ${p.missing.split("：")[0]}`))];
  } else if (S.tab === "inbox") {
    rows = S.inbox.map((it) => h("li", { class: S.thread === it.thread ? "on" : "", onclick: () => openThread(it.thread) },
      avatar(it.agent),
      h("div", { class: "meta" },
        h("div", { class: "row" }, h("span", { class: "name" }, `${it.agent} · ${it.thread.startsWith("dm:") ? "私聊" : threadName(it.thread)}`), h("span", { class: "time" }, when(it.ts))),
        h("div", { class: "sub" }, `${it.kind === "permission" ? "🔐 请求执行" : "❓ 提问"}：${it.title}`))));
  } else if (S.tab === "agents") {
    const busy = busyAgents();
    const act = (title, svg, run) => h("button", { title, html: `<svg viewBox="0 0 24 24">${svg}</svg>`, onclick: (e) => { e.stopPropagation(); run(e.currentTarget); } });
    rows = S.ov.agents.map((a) => h("li", { class: `card${S.thread === `dm:${a.name}` ? " on" : ""}${busy.has(a.name) ? " busy" : ""}`, title: a.detail, onclick: () => openThread(`dm:${a.name}`) },
      avatar(a.name, { live: a.live }),
      h("div", { class: "meta" },
        h("div", { class: "name-row" }, h("span", { class: "name" }, a.name), a.kind === "acp" && OFFICIAL.has(a.template) ? h("span", { class: "tag" }, "官方") : null),
        h("div", { class: "sub" }, a.kind === "acp" ? shortPath(a.detail) : a.detail)),
      busy.has(a.name) ? h("span", { class: "busy-tag" }, "回复中") : h("div", { class: "acts" },
        act("更换形象", '<path d="M12 3.5l2.2 5.3 5.3 2.2-5.3 2.2L12 18.5l-2.2-5.3L4.5 11l5.3-2.2z"/>', () => avatarDialog(a.name)),
        a.kind !== "a2a" ? act("共享给朋友", '<circle cx="6" cy="12" r="2.5"/><circle cx="18" cy="6" r="2.5"/><circle cx="18" cy="18" r="2.5"/><path d="m8.2 10.8 7.6-3.6M8.2 13.2l7.6 3.6"/>', () => shareDialog(a.name)) : null,
        act("更多", '<circle cx="5.5" cy="12" r=".9"/><circle cx="12" cy="12" r=".9"/><circle cx="18.5" cy="12" r=".9"/>', (el) => agentMenu(el, a.name)))));
  } else {
    rows = [h("li", { class: "on" }, avatar("Discord", { small: true }), h("div", { class: "meta" }, h("div", { class: "name" }, "Discord"), h("div", { class: "sub" }, S.ov.discord_status)))];
  }
  rows = rows.filter((li) => !q || li.textContent.toLowerCase().includes(q));
  if (!rows.length) rows = [q ? h("li", { class: "empty" }, `没有和「${S.q}」相关的`)
    : S.tab === "inbox" ? h("li", { class: "empty calm" }, h("span", { class: "okmark", html: '<svg viewBox="0 0 24 24"><path d="m6 12.5 4 4 8-9"/></svg>' }), h("b", {}, "没有等你处理的事"),
        h("small", {}, "agent 想跑终端命令、改文件，或者需要你拍板时，请求会出现在这里，同时在 Discord 里也能点。"))
    : h("li", { class: "empty" }, S.tab === "agents" ? "还没有 agent，点 ＋ 添加" : S.tab === "groups" ? "还没有群聊，点 ＋ 新建" : "没有会话")];
  ul.append(...rows);
}
function threadRow(t, head, fallback) {
  const isGroup = t.startsWith("group:"); const name = threadName(t);
  const typing = S.typing[t]?.size ? [...S.typing[t]].join("、") : "";
  const live = !isGroup && S.ov.agents.find((a) => `dm:${a.name}` === t)?.live;
  const waiting = S.inbox.filter((it) => it.thread === t).length;
  // The last word was a system notice (a failed start, an MCP server needing attention): flag it.
  const warn = !typing && head?.author === "system";
  const unread = S.thread === t ? 0 : S.unread[t] ?? 0;
  const peer = peerOf(t);
  const title = peer ? h("span", { class: "name route" }, peer.guest, h("span", { class: "arrow", html: '<svg viewBox="0 0 24 24"><path d="M5 12h13M13 6.5 18.5 12 13 17.5"/></svg>' }), h("b", {}, peer.agent)) : h("span", { class: "name" }, name);
  const av = peer ? avatar(peer.agent, { small: false }) : avatar(name, { group: isGroup, live: live && !warn, status: isGroup ? null : statusOf(t, name) });
  if (warn) av.append(h("i", { class: "alert", title: "需要留意，点开看看" }, "!"));
  return h("li", { class: `${S.thread === t ? "on" : ""}${unread ? " unread" : ""}${typing ? " busy" : ""}`, onclick: () => openThread(t), onmouseenter: () => prefetch(t) },
    av,
    waiting ? h("i", { class: "badge row-badge", title: `${waiting} 项等你处理` }, String(waiting)) : null,
    h("div", { class: "meta" },
      h("div", { class: "row" }, title, typing ? h("span", { class: "busy-tag" }, "回复中") : h("span", { class: "time" }, when(head?.ts))),
      h("div", { class: "row" },
        h("div", { class: `sub${typing ? " typing" : ""}${warn ? " alert" : ""}` }, typing ? `${typing} ${isGroup ? "正在回复" : ST_LABEL[S.status[`${t}|${name}`]?.st] ?? "正在输入"}…` : head ? `${isGroup && head.author !== "me" && head.author !== "system" ? head.author + "：" : ""}${head.body}` : (fallback ?? "")),
        unread ? h("i", { class: "unread-n" }, unread > 99 ? "99+" : String(unread)) : null),
    ));
}

// ---------- conversation
function sendButton() {
  const b = h("button", { id: "send", class: "send", type: "button", title: "发送（↩）", disabled: true, onclick: () => sendMsg() });
  b.innerHTML = '<svg viewBox="0 0 24 24"><path d="M12 19V5M5.5 11.5 12 5l6.5 6.5"/></svg>';
  return b;
}
function prefetch(t) {
  if (S.msgs[t] || S.msgs[t] === null) return;
  S.msgs[t] = null; // in flight
  invoke("messages", { thread: t }).then((m) => { S.msgs[t] = m; }).catch(() => { delete S.msgs[t]; });
}
async function openThread(t) {
  if (S.thread === t && $("#log")) return;
  S.thread = t; delete S.unread[t]; renderList();
  const isGroup = t.startsWith("group:"); const name = threadName(t);
  const peer = peerOf(t);
  const g = isGroup && S.ov.groups.find((g) => `group:${g.id}` === t);
  const a = !isGroup && S.ov.agents.find((a) => a.name === name);
  const multi = isGroup && (g.people.length || g.guests.length);
  const sub = isGroup ? `${g.members.length} 个 agent${multi ? ` · ${g.people.length + 1} 人 · 多人群：@ 才回答` : ` · 组长 ${g.lead ?? "—"}`}` : peer ? `${peer.guest} 通过共享在用你的 ${peer.agent}` : a ? `${{ acp: "本机", openai: "模型 API" }[a.kind] ?? "A2A"} · ${a.detail}` : "";
  const pane = $("#pane");
  pane.replaceChildren(
    h("div", { class: "head", "data-tauri-drag-region": true }, a ? Object.assign(avatar(name, { live: a.live }), { title: "更换形象", onclick: () => avatarDialog(name), className: `av pickable${a.live ? " live" : ""}` }) : avatar(peer ? peer.agent : name, { group: isGroup }),
      h("div", {}, h("h2", { title: a ? "双击重命名" : "", ondblclick: a ? (e) => inlineRename(e.currentTarget, a.name) : null }, name), h("small", {}, sub)), h("span", { class: "sp" }),
      isGroup ? h("div", { id: "members" }) : null,
      a && a.kind !== "a2a" ? h("div", { id: "cfgs" }) : null,
      !peer ? memorySwitch(t) : null,
      !peer ? h("button", { class: "btn space-btn", title: "待办和群文档（agent 也能读写）", onclick: () => toggleSpace(t) }, "待办", h("span", { class: "count", id: "todo-count" })) : null,
      isGroup ? h("button", { class: "btn primary", onclick: () => groupDialog(g) }, "成员") : null,
      a ? h("button", { class: "icon-btn", title: "更多", onclick: (e) => agentMenu(e.currentTarget, a.name) }, "⋯") : null),
    isGroup ? modeBar(t, g) : null,
    h("div", { id: "log" }),
    peer && !peer.talk ? h("div", { class: "peer-note" }, peer.group ? `对方在同步到 Discord 的群里用你的 agent。${peer.missing}` : "你现在只能旁观。想一起聊：对方建一个同步到 Discord 的群，拉你和你的 Bot 进去，再在群里 @ 一次你的 agent，这里就会变成聊天。") : h("div", { id: "composer" },
      peer ? null : h("button", { class: "attach", type: "button", title: "放一个文件到共享空间，agent 都能读到", onclick: () => attachFile(t) },
        Object.assign(h("span"), { innerHTML: '<svg viewBox="0 0 24 24"><path d="M20.5 11.5 12.4 19.6a5 5 0 0 1-7.1-7.1l8.5-8.5a3.3 3.3 0 0 1 4.7 4.7l-8.5 8.5a1.7 1.7 0 0 1-2.4-2.4l7.8-7.8"/></svg>' })),
      h("textarea", { id: "input", rows: 1, placeholder: peer ? `在 ${peer.guest} 的群里说话（经 Discord 同步）` : isGroup ? (S.teamMode[t] ? "说一个任务，成员会分工处理，组长汇总" : "@名字 叫 agent 回答，@all 一起回答，/help 看群规") : `给 ${name} 发消息，/ 看它的命令` }),
      sendButton()));
  $("#pane").classList.remove("enter"); void $("#pane").offsetWidth; $("#pane").classList.add("enter");
  const ta = $("#input") ?? h("textarea");
  ta.addEventListener("input", () => {
    ta.style.height = "auto"; ta.style.height = ta.scrollHeight + "px";
    const btn = $("#send"); if (btn && !btn.classList.contains("stop")) btn.disabled = !ta.value.trim();
  });
  const mention = isGroup ? mentionPicker(ta, () => [...g.members, ...g.guests, ...g.people]) : null;
  const slash = slashPicker(ta, t);
  ta.addEventListener("keydown", (e) => {
    if (mention?.key(e) || slash.key(e)) return;
    if (e.key === "Enter" && !e.shiftKey && !e.isComposing) { e.preventDefault(); sendMsg(); }
  });
  ta.focus();
  drawMembers(); syncComposer();
  if (a && a.kind !== "a2a") loadConfigs(name, t);
  if (!peer) loadSpace(t);
  // Joining their server takes effect without a restart: check again whenever the chat opens.
  if (peer?.group) invoke("peer_check", { id: t.slice(5) }).then((m) => {
    if (m == null || m === peer.missing || S.thread !== t) return;
    Object.assign(peer, { missing: m, talk: m === "" }); $("#log")?.remove(); openThread(t); renderList();
  }).catch(() => {});
  if (isGroup) invoke("team_state", { thread: t }).then((st) => { if (st) { S.team[t] = st; renderTeam(t); } }).catch(() => {});
  const draw = (msgs) => {
    const log = $("#log"); if (!log || S.thread !== t) return;
    log.replaceChildren(...msgs.map((m) => bubble(m.author, m.body)));
    markResend(log);
    for (const it of S.inbox) if (it.thread === t) log.append(inboxCard(it));
    for (const [k, v] of Object.entries(S.live)) if (k.startsWith(t + "|")) log.append(v.el);
    log.scrollTop = log.scrollHeight;
  };
  if (Array.isArray(S.msgs[t])) draw(S.msgs[t]);
  const fresh = await invoke("messages", { thread: t }).catch(() => null);
  if (!fresh) return;
  const had = S.msgs[t];
  S.msgs[t] = fresh;
  if (!Array.isArray(had) || had.length !== fresh.length) draw(fresh);
}
// Group mode: plain chat or /team (split the task, lead sums up), plus where the group lives.
function modeBar(t, g) {
  const seg = h("div", { class: "pseg", role: "tablist" });
  const draw = () => {
    const team = !!S.teamMode[t];
    seg.replaceChildren(
      h("button", { class: team ? "" : "on", title: "@ 谁谁回答", onclick: () => { delete S.teamMode[t]; draw(); } }, "普通"),
      h("button", { class: team ? "on" : "", title: "成员分工并行处理，最后由组长汇总", onclick: () => { S.teamMode[t] = true; draw(); } }, "团队"));
    const i = $("#input"); if (i) i.placeholder = team ? "说一个任务，成员会分工处理，组长汇总" : "@名字 叫 agent 回答，@all 一起回答，/help 看群规";
  };
  draw();
  const local = g.id.startsWith("local-");
  return h("div", { class: "modebar" }, seg,
    h("span", { class: `state${local ? "" : " on"}` }, h("i"), local ? "仅本地，不同步 Discord" : h("span", {}, "镜像到 ", h("b", {}, "Discord"))));
}
// The chat's memory switch: one row, the whole row toggles; Tab focuses, Space/Enter toggles.
function memorySwitch(t) {
  const on = () => !S.ov.memory_off.includes(t);
  const row = h("div", { class: "memrow", role: "switch", tabindex: 0, "aria-checked": String(on()), "aria-label": "记忆",
    title: "开启时 agent 会读取并保存这个对话的长期记忆；关闭时既不读也不写" }, h("span", {}, "记忆"), h("span", { class: "track" }, h("i")));
  const flip = async () => {
    const next = !on();
    row.setAttribute("aria-checked", String(next));
    try {
      await invoke("set_memory", { thread: t, on: next });
      S.ov.memory_off = next ? S.ov.memory_off.filter((x) => x !== t) : [...S.ov.memory_off, t];
      toast(next ? "记忆已开启" : "记忆已关闭：这个对话不再读取或保存记忆", 2200);
    } catch (e) { row.setAttribute("aria-checked", String(!next)); toast(e); }
  };
  row.onclick = flip;
  row.onkeydown = (e) => { if (e.key === " " || e.key === "Enter") { e.preventDefault(); flip(); } };
  return row;
}
// Group header: every member with what it is doing right now.
function drawMembers() {
  const box = $("#members"); if (!box) return;
  const g = S.ov.groups.find((g) => `group:${g.id}` === S.thread); if (!g) return;
  const busy = S.typing[S.thread] ?? new Set();
  box.replaceChildren(...g.members.map((m) => {
    const el = avatar(m, { small: true, status: statusOf(S.thread, m) ?? { st: "idle", label: m } });
    if (busy.has(m)) { el.classList.add("busy"); el.title = `${m} · 回复中`; }
    return el;
  }));
}
// While any agent is replying here, the send button becomes a stop button.
function syncComposer() {
  const btn = $("#send"); if (!btn) return;
  const busy = Object.keys(S.live).filter((k) => k.startsWith(S.thread + "|") && S.live[k].on);
  btn.classList.toggle("stop", busy.length > 0);
  btn.title = busy.length ? "停止回复" : "发送（↩）";
  if (busy.length) btn.disabled = false;
  else btn.disabled = !$("#input")?.value.trim();
}
// "Again" on the last thing I sent: send the same words once more.
function markResend(log) {
  log.querySelectorAll(".again").forEach((b) => b.remove());
  const mine = [...log.querySelectorAll(".m.me:not(.pending)")].pop();
  if (!mine) return;
  const body = mine.dataset.body; if (!body) return;
  mine.append(h("button", { class: "again", type: "button", title: "再发一遍", onclick: () => resend(body) }, "↻"));
}
async function resend(text) {
  const t = S.thread; if (!t) return;
  const el = bubble("me", text); el.classList.add("new", "pending"); el.dataset.body = text;
  $("#log").append(el); scrollLog(true);
  try { await invoke("send", { thread: t, text }); } catch (e) { failed(el, e); }
}
function failed(el, e) {
  el.classList.add("failed"); el.title = String(e);
  el.append(h("button", { class: "retry", type: "button", onclick: () => { el.remove(); resend(el.dataset.body); } }, "发送失败 · 重发"));
}
// "/" at the start (or after "@agent ") offers that agent's own commands; groups add their own.
function slashPicker(ta, t) {
  let box = null, items = [], idx = 0, start = -1;
  const close = () => { box?.remove(); box = null; };
  const group = t.startsWith("group:");
  const GROUP_CMDS = [["help", "看群规"], ["stop", "停止所有回复"], ["team", "分工协作（群主）"]];
  const pick = (name) => {
    const v = ta.value, end = ta.selectionStart;
    ta.value = `${v.slice(0, start)}/${name} ${v.slice(end)}`;
    const caret = start + name.length + 2; ta.setSelectionRange(caret, caret); close(); ta.focus(); ta.dispatchEvent(new Event("input"));
  };
  const draw = () => box.replaceChildren(...items.map(([n, d], i) => h("button", {
    class: `pop-item${i === idx ? " hl" : ""}`, onmousedown: (e) => { e.preventDefault(); pick(n); },
  }, h("span", { class: "mi" }, h("b", {}, `/${n}`), h("small", { class: "mute" }, d)))));
  ta.addEventListener("input", async () => {
    const before = ta.value.slice(0, ta.selectionStart);
    const m = before.match(/^(?:@([\w-]+)\s+)?\/([\w-]*)$/);
    if (!m) return close();
    const agent = m[1] ?? (group ? null : t.slice(3));
    start = ta.selectionStart - m[2].length - 1;
    let list = group && !agent ? GROUP_CMDS : [];
    if (agent && S.ov.agents.some((a) => a.name === agent && a.kind === "acp")) {
      S.cmds[agent] ??= await invoke("agent_commands", { agent }).catch(() => []);
      if (!S.cmds[agent].length) delete S.cmds[agent]; // not known yet: ask again next time
      list = S.cmds[agent] ?? [];
    }
    const q = m[2].toLowerCase();
    items = list.filter(([n]) => n.toLowerCase().startsWith(q)).slice(0, 12);
    if (!items.length) return close();
    idx = 0;
    if (!box) {
      box = h("div", { class: "pop mention" }); document.body.append(box);
      const r = ta.getBoundingClientRect(); box.style.left = `${r.left}px`; box.style.bottom = `${innerHeight - r.top + 6}px`;
    }
    draw();
  });
  ta.addEventListener("blur", () => setTimeout(close, 100));
  return { key(e) {
    if (!box) return false;
    if (e.key === "ArrowDown" || e.key === "ArrowUp") { idx = (idx + (e.key === "ArrowDown" ? 1 : items.length - 1)) % items.length; draw(); }
    else if ((e.key === "Enter" || e.key === "Tab") && !e.isComposing) pick(items[idx][0]);
    else if (e.key === "Escape") close();
    else return false;
    e.preventDefault(); return true;
  } };
}

// ---------- space: the thread's shared to-dos and doc (agents edit them through Disco tools)
// ---------- team mode: activity strip over the composer + blackboard drawer
const TEAM_STAGE = { plan: "正在拆分任务", work: "成员并行处理中", summary: "组长正在汇总", done: "已完成", failed: "未完成" };
function renderTeam(t) {
  const st = S.team[t]; const old = $("#team");
  if (S.thread !== t || !st || st.dismissed) { old?.remove(); return; }
  const active = ["plan", "work", "summary"].includes(st.stage);
  const roles = st.roles.map((r) => h("span", { class: `tr ${r.state}`, title: `${r.task}${r.last ? `\n正在：${r.last}` : ""}` },
    h("i"), `${r.agent}·${r.role}`, r.state === "working" && r.last ? h("small", {}, r.last) : null));
  const el = h("div", { id: "team", class: active ? "active" : st.stage },
    h("b", {}, "🧩 ", TEAM_STAGE[st.stage] ?? st.stage), h("div", { class: "roles" }, ...roles),
    h("button", { class: "btn sm", onclick: () => toggleBoard(t) }, "黑板"),
    active ? null : h("button", { class: "icon-btn", title: "收起", onclick: () => { st.dismissed = true; renderTeam(t); } }, "×"));
  if (old) old.replaceWith(el); else $("#composer")?.before(el);
}
listen("team", ({ payload: st }) => {
  const prev = S.team[st.thread];
  S.team[st.thread] = st;
  renderTeam(st.thread);
  if ($("#space.board") && S.thread === st.thread && prev?.stage !== st.stage) loadBoard(st.thread);
});
function toggleBoard(t) {
  const open = $("#space");
  if (open) { const was = open.classList.contains("board"); open.remove(); $(".space-btn")?.classList.remove("on"); if (was) return; }
  const box = h("aside", { id: "space", class: "board" });
  box.style.top = `${$("#pane .head")?.offsetHeight ?? 0}px`;
  $("#pane").append(box);
  loadBoard(t);
}
async function loadBoard(t, pick) {
  const files = await invoke("team_files", { thread: t }).catch(() => []);
  const box = $("#space.board"); if (!box || S.thread !== t) return;
  const label = (n) => n === "summary.md" ? "结论" : n === "context.md" ? "简报" : n.replace(/^findings\//, "").replace(/\.md$/, "");
  const cur = files.find(([n]) => n === pick) ?? files[0];
  const doc = h("div", { class: "doc md" }); doc.innerHTML = cur ? md(cur[1]) : '<p class="mute">还没有团队任务。在群里发「/team 任务」开始。</p>';
  box.replaceChildren(
    h("div", { class: "sp-head" }, h("b", {}, "团队黑板"), h("span", { class: "sp" }), h("button", { class: "icon-btn", title: "关闭", onclick: () => box.remove() }, "×")),
    h("div", { class: "bseg" }, ...files.map(([n]) => h("button", { class: cur && cur[0] === n ? "on" : "", onclick: () => loadBoard(t, n) }, label(n)))),
    doc);
}

function toggleSpace(t) {
  const open = $("#space");
  if (open) { const board = open.classList.contains("board"); open.remove(); $(".space-btn")?.classList.remove("on"); if (!board) return; }
  const box = h("aside", { id: "space" });
  box.style.top = `${$("#pane .head")?.offsetHeight ?? 0}px`;
  $("#pane").append(box);
  $(".space-btn")?.classList.add("on");
  loadSpace(t);
}
async function loadSpace(t) {
  let sp; try { sp = await invoke("space", { thread: t }); } catch { return; }
  if (S.thread !== t) return;
  const left = sp.todos.filter((x) => !x.done).length;
  const cnt = $("#todo-count"); if (cnt) cnt.textContent = left ? String(left) : "";
  const box = $("#space"); if (!box) return;
  const who = (a) => (a === "me" ? "我" : a);
  const add = h("input", { placeholder: "加一项待办，回车", class: "todo-in" });
  add.onkeydown = async (e) => {
    if (e.key !== "Enter" || e.isComposing || !add.value.trim()) return;
    try { await invoke("todo_new", { thread: t, text: add.value }); add.value = ""; } catch (er) { toast(er); }
  };
  const todos = h("ul", { class: "todos" }, ...sp.todos.map((x) => h("li", { class: x.done ? "done" : "" },
    h("input", { type: "checkbox", checked: x.done, onchange: (e) => invoke("todo_set", { thread: t, id: x.id, done: e.target.checked }).catch(toast) }),
    h("span", { class: "tx" }, x.text, x.assignee ? h("em", {}, ` → ${who(x.assignee)}`) : null, x.by && x.by !== "me" ? h("small", {}, ` · ${x.by} 添加`) : null),
    h("button", { class: "icon-btn", title: "删除", onclick: async () => {
      try { await invoke("todo_remove", { thread: t, id: x.id }); toast("已删除待办", 5000, ["撤销", () => invoke("todo_new", { thread: t, text: x.text }).catch(toast)]); } catch (e) { toast(e); }
    } }, "×"))));
  const doc = h("div", { class: "doc md" });
  doc.innerHTML = sp.doc.trim() ? md(sp.doc) : '<p class="mute">还没有内容。agent 可以用 doc_write 写入目标、结论和分工，你也可以直接编辑。</p>';
  const edit = h("button", { class: "btn sm" }, "编辑");
  edit.onclick = () => {
    const ta = h("textarea", { class: "doc-edit" }); ta.value = sp.doc;
    const save = h("button", { class: "btn sm primary" }, "保存");
    save.onclick = async () => { try { await invoke("doc_save", { thread: t, content: ta.value }); } catch (er) { toast(er); } };
    doc.replaceWith(ta); edit.replaceWith(save); ta.focus();
  };
  box.replaceChildren(
    h("div", { class: "sp-head" }, h("b", {}, "待办"), h("span", { class: "mute" }, left ? `${left} 项未完成` : "")),
    todos, add,
    h("div", { class: "sp-head" }, h("b", {}, "群文档"), h("span", { class: "sp" }), edit,
      h("button", { class: "btn sm", title: sp.doc_path, onclick: () => invoke("reveal_doc", { thread: t }).catch(toast) }, IS_WIN ? "在资源管理器中显示" : "在访达中显示")),
    doc);
}
listen("space", ({ payload: t }) => { if (S.thread === t && !$("#space .doc-edit")) loadSpace(t); });

// Model picker: only the agent's own model list (ACP session config, category "model").
async function loadConfigs(agent, t) {
  const box = $("#cfgs");
  let list = [], err = "";
  try { list = await invoke("agent_configs", { agent }); } catch (e) { err = String(e); }
  if (S.thread !== t || !box?.isConnected) return;
  if (err) {
    // Say it can't start, and why, instead of silently showing no model menu.
    const login = /登录|authentication/i.test(err);
    const pill = h("button", { class: "model-pill warn", title: err }, h("span", { class: "dot" }), h("span", { class: "lbl" }, login ? "需要登录" : "无法启动"));
    pill.onclick = () => toast(err);
    box.replaceChildren(pill);
    return;
  }
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

// A pill that opens a menu of choices: used instead of native <select>.
function choice(options, value, onPick, placeholder = "选择") {
  const label = () => options.find(([v]) => v === value)?.[1] ?? placeholder;
  const pill = h("button", { type: "button", class: "model-pill choice" }, h("span", { class: "lbl" }, label()), h("span", { class: "chev" }));
  pill.onclick = () => popover(pill, options.map(([v, l]) => ({ label: l, on: v === value, run: () => { value = v; pill.querySelector(".lbl").textContent = label(); onPick(v); } })));
  return pill;
}
// A small anchored menu; closes on outside click or Esc.
function popover(anchor, items) {
  document.querySelector(".pop")?.remove();
  const pop = h("div", { class: "pop" }, ...items.map((it) => h("button", {
    class: `pop-item${it.on ? " on" : ""}${it.danger ? " danger" : ""}`,
    onclick: () => { close(); it.run(); },
  }, h("span", {}, it.label), it.on ? h("span", { class: "pop-check" }) : null)));
  // Long lists (a provider's hundreds of models) get a filter box.
  if (items.length > 12) {
    const f = h("input", { class: "pop-filter", placeholder: "筛选…", spellcheck: false });
    f.oninput = () => { const q = f.value.toLowerCase(); for (const b of pop.querySelectorAll(".pop-item")) b.hidden = !b.textContent.toLowerCase().includes(q); };
    f.onkeydown = (e) => { if (e.key === "Enter") pop.querySelector(".pop-item:not([hidden])")?.click(); };
    pop.prepend(f); setTimeout(() => f.focus());
  }
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
  const local = S.ov.agents.find((a) => a.name === name)?.kind !== "a2a";
  popover(anchor, [
    { label: "更换形象…", run: () => avatarDialog(name) },
    ...(local ? [{ label: "共享给朋友…", run: () => shareDialog(name) }] : []),
    { label: "重命名", run: async () => {
      if (S.thread !== `dm:${name}`) { setTab("chats"); await openThread(`dm:${name}`); }
      const h2 = $(".head h2"); if (h2) inlineRename(h2, name);
    } },
    { label: "删除 Agent…", danger: true, run: () => removeAgent(name) },
  ]);
}
// Rename in place: the title becomes editable; ↩ saves, Esc or blur-without-change cancels.
function inlineRename(el, name) {
  if (el.isContentEditable) return;
  el.contentEditable = "plaintext-only"; el.classList.add("editing"); el.focus();
  getSelection().selectAllChildren(el);
  const done = async (save) => {
    el.removeEventListener("keydown", key); el.removeEventListener("blur", blur);
    el.contentEditable = "false"; el.classList.remove("editing");
    const next = el.textContent.trim();
    if (!save || !next || next === name) { el.textContent = name; return; }
    try {
      await invoke("rename_agent", { name, newName: next });
      delete S.msgs[`dm:${name}`]; await load(); S.thread = null; openThread(`dm:${next}`); toast(`已改名为 ${next}`, 2000);
    } catch (e) { el.textContent = name; toast(e); }
  };
  const key = (e) => { if (e.key === "Enter" && !e.isComposing) { e.preventDefault(); done(true); } else if (e.key === "Escape") { e.preventDefault(); done(false); } };
  const blur = () => done(true);
  el.addEventListener("keydown", key); el.addEventListener("blur", blur);
}
function bubble(author, body) {
  if (author === "system") {
    // Hints carry commands in `code`: click one to copy it.
    const el = h("div", { class: "m sys" }), tx = h("span"); el.append(tx); tx.innerHTML = esc(body).replace(/`([^`]+)`/g, '<code class="cp" title="点击复制">$1</code>').replace(/\n/g, "<br>");
    el.querySelectorAll("code.cp").forEach((c) => { c.onclick = () => copyText(c.textContent, null).then(() => toast("已复制", 1200)); });
    return el;
  }
  const me = author === "me";
  const b = h("div", { class: "b" }); b.innerHTML = md(body);
  // File hand-offs end with the file's path: make it open in Finder.
  if (body.includes("📎")) {
    b.innerHTML = b.innerHTML.replace(/(^|<br>|<p>)(\/[^<]+)/g, '$1<a class="path" href="#">$2</a>');
    b.querySelectorAll("a.path").forEach((a) => { a.onclick = (e) => { e.preventDefault(); invoke("reveal_path", { path: a.textContent }).catch(toast); }; });
  }
  const human = author.endsWith(" (Discord)");
  const col = h("div", {}, !me && S.thread?.startsWith("group:") ? h("div", { class: "who" }, human ? author.slice(0, -10) : author) : null, b);
  const el = h("div", { class: `m${me ? " me" : ""}${human ? " person" : ""}` }, me ? null : human ? h("div", { class: "av sm person" }, author[0].toUpperCase()) : avatar(author, { small: true }), col);
  if (me) el.dataset.body = body;
  return el;
}
// Typing "@" in a group offers its members; ↑↓ to move, ↩/⇥ to pick, Esc to close.
function mentionPicker(ta, members) {
  let box = null, items = [], idx = 0, start = -1;
  const close = () => { box?.remove(); box = null; };
  const pick = (name) => {
    const v = ta.value, end = ta.selectionStart;
    ta.value = `${v.slice(0, start)}@${name} ${v.slice(end)}`;
    const caret = start + name.length + 2; ta.setSelectionRange(caret, caret); close(); ta.focus();
  };
  const draw = () => box.replaceChildren(...items.map((n, i) => h("button", {
    class: `pop-item${i === idx ? " hl" : ""}`, onmousedown: (e) => { e.preventDefault(); pick(n); },
  }, h("span", { class: "mi" }, n === "all" ? h("span", { class: "mi-all" }, "@") : S.ov.agents.some((a) => a.name === n) ? avatar(n, { small: true }) : h("span", { class: "mi-all person" }, n[0].toUpperCase()), n === "all" ? "所有人" : n))));
  ta.addEventListener("input", () => {
    const m = ta.value.slice(0, ta.selectionStart).match(/(^|\s)@([\w-]*)$/);
    if (!m) return close();
    start = ta.selectionStart - m[2].length - 1;
    const q = m[2].toLowerCase();
    items = [...members().filter((n) => n.toLowerCase().startsWith(q)), ...("all".startsWith(q) ? ["all"] : [])];
    if (!items.length) return close();
    idx = 0;
    if (!box) {
      box = h("div", { class: "pop mention" }); document.body.append(box);
      const r = ta.getBoundingClientRect(); box.style.left = `${r.left}px`; box.style.bottom = `${innerHeight - r.top + 6}px`;
    }
    draw();
  });
  ta.addEventListener("blur", () => setTimeout(close, 100));
  return { key(e) {
    if (!box) return false;
    if (e.key === "ArrowDown" || e.key === "ArrowUp") { idx = (idx + (e.key === "ArrowDown" ? 1 : items.length - 1)) % items.length; draw(); }
    else if ((e.key === "Enter" || e.key === "Tab") && !e.isComposing) pick(items[idx]);
    else if (e.key === "Escape") close();
    else return false;
    e.preventDefault(); return true;
  } };
}
// Copy buttons on code blocks.
document.addEventListener("click", async (e) => {
  const btn = e.target.closest?.("pre > .copy"); if (!btn) return;
  try { await navigator.clipboard.writeText(btn.nextElementSibling.textContent); btn.textContent = "已复制"; }
  catch { btn.textContent = "复制失败"; }
  setTimeout(() => { btn.textContent = "复制"; }, 1200);
});
// Follow new content only when the reader is already near the bottom.
function scrollLog(force) {
  const l = $("#log"); if (!l) return;
  const near = l.scrollHeight - l.scrollTop - l.clientHeight < 140;
  if (force || near) { l.scrollTo({ top: l.scrollHeight, behavior: REDUCED || !force ? "auto" : "smooth" }); $("#jump")?.remove(); return; }
  if (!$("#jump")) {
    const j = h("button", { id: "jump", onclick: () => scrollLog(true) }, "↓ 新消息");
    l.after(j);
    l.addEventListener("scroll", function off() { if (l.scrollHeight - l.scrollTop - l.clientHeight < 40) { j.remove(); l.removeEventListener("scroll", off); } });
  }
}
async function attachFile(t) {
  const path = await invoke("choose_path", { file: true }); if (!path) return;
  try { toast(await invoke("send_file", { thread: t, path, note: null }), 3000); } catch (e) { toast(e, 8000); }
}
async function sendMsg() {
  if ($("#send")?.classList.contains("stop")) return stopHere();
  const ta = $("#input"); let text = ta.value.trim();
  if (!text || !S.thread) return;
  if (S.teamMode[S.thread] && !text.startsWith("/")) text = `/team ${text}`;
  ta.value = ""; ta.style.height = "auto"; if ($("#send")) $("#send").disabled = true;
  // Show it now; the backend's echo of the same message confirms it.
  const t = S.thread;
  const el = bubble("me", text); el.classList.add("new", "pending"); el.dataset.body = text;
  $("#log").append(el); scrollLog(true);
  try { await invoke("send", { thread: t, text }); }
  catch (e) { failed(el, e); toast(e); }
}
// Stop every reply running in the open chat.
async function stopHere() {
  const t = S.thread, btn = $("#send");
  const agents = Object.keys(S.live).filter((k) => k.startsWith(t + "|") && S.live[k].on).map((k) => k.slice(t.length + 1));
  btn.disabled = true;
  const ok = await Promise.all(agents.map((agent) => invoke("stop_reply", { thread: t, agent }).catch(() => false)));
  if (!ok.some(Boolean)) { toast("这个 agent 不支持中途停止"); btn.disabled = false; }
}

// ---------- inbox: permission requests and agent questions, answered here or from Discord
function inboxCard(it) {
  const perm = it.kind === "permission";
  const card = h("div", { class: `perm ${perm ? "" : "ask"}`, "data-item": it.id },
    h("b", {}, perm ? `${it.agent} 请求执行：${it.title}` : `${it.agent} 问你：`),
    perm ? null : h("div", { class: "q" }, it.title));
  const answer = async (optionId, text) => {
    card.querySelectorAll("button, textarea").forEach((b) => { b.disabled = true; });
    try { await invoke("inbox_answer", { id: it.id, optionId, text }); }
    catch (e) { card.querySelectorAll("button, textarea").forEach((b) => { b.disabled = false; }); toast(e); }
  };
  // "Always allow" first and marked: one tap and the agent stops asking for this kind of action.
  const order = [...it.options].sort((a, b) => b.always - a.always);
  const opts = h("div", { class: "opts" }, ...order.map((o) => h("button", { class: `${o.allow ? "allow" : ""}${o.always ? " always" : ""}`, title: o.always ? "以后同类操作不再询问" : "", onclick: () => answer(o.id, null) }, o.label)),
    perm ? h("button", { onclick: () => answer(null, null) }, "拒绝") : null);
  if (it.options.length || perm) card.append(opts);
  if (it.text) {
    const ta = h("textarea", { rows: 1, placeholder: "输入回答，↩ 发送" });
    ta.onkeydown = (e) => { if (e.key === "Enter" && !e.shiftKey && !e.isComposing && ta.value.trim()) { e.preventDefault(); answer(null, ta.value); } };
    card.append(h("div", { class: "free" }, ta, h("button", { class: "allow", onclick: () => ta.value.trim() && answer(null, ta.value) }, "回答")));
  }
  return card;
}
function setInbox(items, quiet = false) {
  const before = new Set(S.inbox.map((i) => i.id));
  S.inbox = items;
  const badge = $("#inbox-badge"); badge.textContent = items.length ? String(items.length) : "";
  const log = $("#log");
  if (log) {
    const ids = new Set(items.map((i) => i.id));
    log.querySelectorAll(".perm[data-item]").forEach((c) => {
      if (!ids.has(c.dataset.item)) { c.classList.add("gone"); setTimeout(() => c.remove(), 180); }
    });
    for (const it of items) if (it.thread === S.thread && !before.has(it.id) && !log.querySelector(`[data-item="${it.id}"]`)) {
      const c = inboxCard(it); c.classList.add("new"); log.append(c); scrollLog(true);
    }
  }
  // Somewhere else: don't pull the user away, just say it once.
  if (!quiet) for (const it of items) if (!before.has(it.id) && it.thread !== S.thread) {
    toast(`${it.agent} ${it.kind === "permission" ? "需要你批准" : "有问题问你"}`, 6000, ["查看", () => openThread(it.thread)]);
  }
  if (S.thread) drawMembers();
  renderList();
}
listen("inbox", ({ payload }) => setInbox(payload));

// ---------- events
const liveKey = (thread, agent) => `${thread}|${agent}`;
listen("agents", () => load());
listen("message", ({ payload: m }) => {
  if (m.thread.startsWith("dm:") && !S.ov.agents.some((a) => `dm:${a.name}` === m.thread)) load();
  const k = liveKey(m.thread, m.author);
  if (S.live[k]) { S.live[k].el.remove(); delete S.live[k]; }
  const head = S.ov.heads.find((x) => x.thread === m.thread);
  if (head) Object.assign(head, m); else S.ov.heads.unshift(m);
  if (S.thread !== m.thread && m.author !== "me") S.unread[m.thread] = (S.unread[m.thread] ?? 0) + 1;
  if (Array.isArray(S.msgs[m.thread])) S.msgs[m.thread].push({ author: m.author, body: m.body, ts: m.ts });
  if (S.thread === m.thread) {
    const pending = m.author === "me" && [...document.querySelectorAll("#log .m.pending")].find((e) => e.dataset.body === m.body.trim());
    if (pending) pending.classList.remove("pending");
    else { const el = bubble(m.author, m.body); el.classList.add("new"); $("#log").append(el); scrollLog(m.author === "me"); }
    if (m.author === "me") markResend($("#log"));
  }
  if (m.thread.startsWith("group:") && m.author.endsWith(" (Discord)")) {
    const g = S.ov.groups.find((g) => `group:${g.id}` === m.thread), who = m.author.slice(0, -10);
    if (g && !g.people.includes(who)) g.people.push(who);
  }
  renderList();
});
listen("typing", ({ payload: { thread, agent, on } }) => {
  const set = (S.typing[thread] ??= new Set());
  on ? set.add(agent) : set.delete(agent);
  const k = liveKey(thread, agent);
  if (on && !S.live[k]) {
    const b = h("div", { class: "b" }, h("span", { class: "typing-dots" }, h("i"), h("i"), h("i")));
    b.classList.add("streaming");
    S.live[k] = { text: "", b, on: true, el: h("div", { class: "m new" }, avatar(agent, { small: true }), h("div", {}, h("div", { class: "who" }, agent), b)) };
    if (S.thread === thread) { $("#log").append(S.live[k].el); scrollLog(); }
  }
  if (!on && S.live[k] && !S.live[k].text) { S.live[k].el.remove(); delete S.live[k]; }
  if (!on && S.live[k]) { S.live[k].on = false; S.live[k].b.classList.remove("streaming"); }
  setStatus(thread, agent, on ? "think" : null);
  drawKill();
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
  } else if (event.type === "tool") {
    const done = /completed|failed/.test(event.status);
    if (S.typing[thread]?.has(agent)) setStatus(thread, agent, done ? "think" : "tool", done ? null : event.title);
    if (S.thread === thread && event.title && !done) {
      const chip = h("div", { class: "chip new" }, `🔧 ${agent} · ${event.title}`);
      live ? live.el.before(chip) : $("#log").append(chip);
    }
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
  const page = h("div", { class: "page add" }, h("h1", {}, "添加 Agent"), h("p", {}, `正在扫描这台${IS_WIN ? "电脑" : " Mac "}…`));
  $("#pane").replaceChildren(page);
  $("#pane").classList.remove("enter"); void $("#pane").offsetWidth; $("#pane").classList.add("enter");
  // Show the last scan at once; rescan in the background and redraw if it changed.
  const scan = Promise.all([invoke("detect_agents").catch(() => []), invoke("conda_envs").catch(() => [])]);
  if (!S.found) S.found = await scan;
  else scan.then((r) => { const changed = JSON.stringify(r) !== JSON.stringify(S.found); S.found = r; if (changed && $(".page.add")) renderGallery(); });
  const [found, envs] = S.found;
  S.envs = envs;
  const have = found.filter((t) => t.found), miss = found.filter((t) => !t.found);
  const here = IS_WIN ? "这台电脑" : "这台 Mac";
  const found2 = have.length ? [h("div", { class: "asec" }, `${here}上检测到`, h("i", {}, String(have.length))),
    h("div", { class: "alist" }, ...have.map((t) => agentRow({ ...t, kind: "acp" }, t.found)))] : [];
  const more = h("div", { class: "alist" },
    agentRow({ id: "openai", name: "OpenAI 兼容模型", kind: "openai" }, "OpenRouter、DeepSeek、Ollama 等，填地址和 Key"),
    agentRow({ id: "command", name: "其他本地 agent", kind: "command" }, "任意支持 ACP 的程序"),
    agentRow({ id: "invite", name: "朋友的邀请码", kind: "invite" }, "接入别人通过 Disco 共享的 agent"),
    agentRow({ id: "a2a", name: "远程 A2A agent", kind: "a2a" }, "通过 URL 接入"));
  page.replaceChildren(h("h1", {}, "添加 Agent"),
    h("p", { class: "lead" }, have.length ? "点「导入」一步完成，直接沿用终端里的登录和模型；点整行可以调目录和环境。" : `没在${here}上找到已安装的 agent，可以手动添加。`),
    ...found2, h("div", { class: "asec" }, "更多接入方式"), more,
    miss.length ? h("div", { class: "callout" }, h("span", {}, "装好后会自动出现"),
      h("div", { class: "pills" }, ...miss.map((t) => h("button", { class: "tpill", title: INSTALL[t.id] ? `复制安装命令：${INSTALL[t.id]}` : t.name,
        onclick: () => INSTALL[t.id] && copy(INSTALL[t.id], `已复制：${INSTALL[t.id]}`) }, brandIcon(t.id, true), t.name)))) : "");
}
function agentRow(t, sub) {
  const row = h("div", { class: "arow" });
  const head = h("button", { class: "ahead", onclick: () => {
    const open = row.classList.toggle("open");
    for (const o of document.querySelectorAll(".arow.open")) if (o !== row) o.classList.remove("open");
    if (open && !row.querySelector(".aform")) row.append(agentForm(t));
    if (open) row.querySelector(".aform input")?.focus();
  } },
    brandIcon(t.kind === "acp" ? t.id : t.kind),
    h("div", { class: "ameta" }, h("b", {}, t.name), t.kind === "acp" ? h("code", { class: "path", title: sub }, shortPath(sub)) : h("small", {}, sub)),
    h("span", { class: `aplus${t.kind === "acp" ? " pri" : ""}`, onclick: t.kind === "acp" ? (e) => { e.stopPropagation(); quickImport(t, e.currentTarget); } : null }, t.kind === "acp" ? "导入" : "添加"));
  row.append(head);
  return row;
}
// Install commands for the pills under "更多": click copies one.
const INSTALL = { gemini: "npm i -g @google/gemini-cli", qwen: "npm i -g @qwen-code/qwen-code", goose: "brew install block-goose-cli",
  opencode: "npm i -g opencode-ai", "claude-code": "npm i -g @anthropic-ai/claude-code", codex: "npm i -g @openai/codex", pi: "npm i -g @earendil-works/pi-coding-agent" };
function copy(text, msg) { navigator.clipboard?.writeText(text).then(() => toast(msg, 2000), () => toast(text)); }
// Small drawn marks for known agents (our own glyphs in each tool's colour), line icons for the rest.
const MARKS = {
  "claude-code": ["#d97757", '<g stroke-width="2.2">' + [0, 30, 60, 90, 120, 150].map((d) => `<path d="M12 4.5v15" transform="rotate(${d} 12 12)"/>`).join("") + "</g>"],
  codex: ["#111827", '<path d="m7 9 3 3-3 3M12.5 15.5h4.5"/><rect x="3.5" y="5" width="17" height="14" rx="3.5"/>'],
  hermes: ["#7c5cff", '<path d="M12 19V8M12 8c-2.5-3-6-3.5-8-2.5 1.5 2 4 3.5 8 3.5M12 8c2.5-3 6-3.5 8-2.5-1.5 2-4 3.5-8 3.5"/><circle cx="12" cy="6" r="1.4"/>'],
  pi: ["#0f766e", '<path d="M5.5 8h13M9.5 8v9M14.5 8v6.5c0 1.6.8 2.5 2.4 2.5"/>'],
  gemini: ["#3b82f6", '<path d="M12 3c.8 4.8 4.2 8.2 9 9-4.8.8-8.2 4.2-9 9-.8-4.8-4.2-8.2-9-9 4.8-.8 8.2-4.2 9-9z"/>'],
  qwen: ["#6d28d9", '<path d="M12 3.5 19.5 8v8L12 20.5 4.5 16V8z"/><path d="M12 12h4"/>'],
  goose: ["#111827", '<path d="M8 19c0-4 1-6 4-8V6.5a2.5 2.5 0 0 1 5 0V8h2.5M12 11c3 1 5 3 5 8"/>'],
  opencode: ["#334155", '<path d="m9 7-5 5 5 5M15 7l5 5-5 5"/>'],
  dsh: ["#475569", '<path d="M5 8l4 4-4 4M11 17h8"/>'],
  command: ["#64748b", '<path d="m6.5 8 4 4-4 4M12.5 16.5h5"/>'],
  openai: ["#0ea5e9", '<path d="M12 3.5 19.4 7.8v8.4L12 20.5 4.6 16.2V7.8z"/><path d="M12 12 19.4 7.8M12 12 4.6 7.8M12 12v8.5"/>'],
  invite: ["#10a37f", '<circle cx="12" cy="12" r="3.6"/><path d="M15.6 12v1.4a2.5 2.5 0 0 0 5 0V12a8.6 8.6 0 1 0-3.4 6.9"/>'],
  a2a: ["#5865f2", '<circle cx="6" cy="12" r="2.5"/><circle cx="18" cy="12" r="2.5"/><path d="M8.5 12h7M12 6.5c2 0 4 1 5.5 3.2M12 17.5c-2 0-4-1-5.5-3.2"/>'],
};
function brandIcon(id, small = false) {
  if (LOGOS[id]) return h("span", { class: `brand logo${small ? " sm" : ""}`, html: `<svg viewBox="0 0 24 24">${LOGOS[id]}</svg>` });
  const [c0, svg] = MARKS[id] ?? ["#8a8a8e", '<path d="M12 4v16M4 12h16"/>'];
  const c = BRAND[id] ?? c0;
  return h("span", { class: `brand${small ? " sm" : ""}`, style: `--b:${c}`, html: `<svg viewBox="0 0 24 24">${svg}</svg>` });
}
// One click: import a detected agent with default settings and open its chat.
async function quickImport(t, btn) {
  let name = t.id, i = 2;
  while (S.ov.agents.some((a) => a.name === name)) name = `${t.id}-${i++}`;
  btn.textContent = "导入中…"; btn.classList.add("busy");
  try {
    await invoke("add_agent", { spec: { template: t.id, name, url: null, headerName: null, headerValue: null, command: null, cwd: null, count: 1, worktree: false, conda: null } });
    await load(); setTab("chats"); openThread(`dm:${name}`); toast(`已导入 ${name}`, 2000);
  } catch (e) { btn.textContent = "导入"; btn.classList.remove("busy"); toast(e); }
}
function inviteForm() {
  const err = h("div", { class: "err" });
  const code = h("textarea", { class: "fld code", rows: 3, placeholder: "粘贴 disco1: 开头的邀请码", spellcheck: false });
  const name = h("input", { class: "fld", placeholder: "名称（可选，默认 agent-主人）", spellcheck: false });
  const go = h("button", { type: "button", class: "btn pri" }, "添加");
  go.onclick = async () => {
    go.disabled = true; err.textContent = "";
    try {
      const n = await invoke("accept_invite", { code: code.value, name: name.value || null });
      await load(); setTab("chats"); openThread(`dm:${n}`);
    } catch (e) { err.textContent = String(e); go.disabled = false; }
  };
  return h("div", { class: "aform" }, h("div", { class: "frow" }, h("span", {}, "邀请码"), code), h("div", { class: "frow" }, h("span", {}, "名称"), name),
    h("p", { class: "hint2" }, "双方都要登录同一个 Tailscale 网络（或对方把你加为共享设备）。"), h("div", { class: "factions" }, err, go));
}
function agentForm(t) {
  if (t.kind === "invite") return inviteForm();
  const st = { cwd: "", conda: "", count: 1 };
  const err = h("div", { class: "err" });
  const name = h("input", { class: "fld", value: t.kind === "acp" ? t.id : "", placeholder: "名称，如 coder", spellcheck: false });
  let look = null;
  const parts = [h("div", { class: "frow" }, h("span", {}, "名称"), name),
    h("div", { class: "frow top" }, h("span", {}, "形象"), avatarPicker(t.letter ?? t.name, { form: FORM_KEYS[hash(t.id) % FORM_KEYS.length], palette: "brand", base: t.color }, (v) => { look = v; }))];
  let cmd, url, hn, hv, model;
  if (t.kind === "openai") {
    url = h("input", { class: "fld", placeholder: "Base URL，如 https://openrouter.ai/api/v1", spellcheck: false });
    hv = h("input", { class: "fld", type: "password", placeholder: IS_WIN ? "API Key（本地模型可不填，存凭据管理器）" : "API Key（本地模型可不填，存钥匙串）" });
    model = h("input", { class: "fld", placeholder: "模型，如 deepseek-chat（不填用第一个）", spellcheck: false });
    const PRESETS = [["OpenRouter", "https://openrouter.ai/api/v1", ""], ["DeepSeek", "https://api.deepseek.com/v1", "deepseek-chat"],
      ["OpenAI", "https://api.openai.com/v1", ""], ["Ollama", "http://localhost:11434/v1", ""], ["LM Studio", "http://localhost:1234/v1", ""]];
    const chips = h("div", { class: "chips" }, ...PRESETS.map(([n, u, m]) => h("button", { type: "button", class: "chip2", onclick: (e) => {
      url.value = u; if (m && !model.value) model.value = m; if (!name.value) name.value = n.toLowerCase().replace(/\s+/g, "-");
      chips.querySelectorAll(".chip2").forEach((c) => c.classList.toggle("on", c === e.currentTarget)); (u.includes("localhost") ? model : hv).focus();
    } }, n)));
    parts.push(h("div", { class: "frow" }, h("span", {}, "常用"), chips), h("div", { class: "frow" }, h("span", {}, "地址"), url),
      h("div", { class: "frow" }, h("span", {}, "Key"), hv), h("div", { class: "frow" }, h("span", {}, "模型"), model),
      h("p", { class: "hint2" }, "Key 只存在这台电脑上。添加后在对话顶部可以随时切换模型；它也能用 Disco 工具记待办、写群文档。"));
  }
  if (t.kind === "command") {
    cmd = h("input", { class: "fld", placeholder: "/path/to/agent --acp", spellcheck: false });
    parts.push(h("div", { class: "frow" }, h("span", {}, "程序"), cmd, h("button", { type: "button", class: "ghost", onclick: async () => {
      const p = await invoke("choose_path", { file: true }); if (p) cmd.value = /\s/.test(p) ? `"${p}"` : p;
    } }, "选择…")));
  }
  if (t.kind === "a2a") {
    url = h("input", { class: "fld", placeholder: "https://agent.example.com", spellcheck: false });
    hn = h("input", { class: "fld", placeholder: "鉴权头（可选），如 authorization" });
    hv = h("input", { class: "fld", type: "password", placeholder: IS_WIN ? "鉴权值（可选，存凭据管理器）" : "鉴权值（可选，存钥匙串）" });
    parts.push(h("div", { class: "frow" }, h("span", {}, "URL"), url), h("div", { class: "frow" }, h("span", {}, "鉴权"), hn, hv));
  }
  if (t.kind !== "a2a" && t.kind !== "openai") {
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
      const added = await invoke("add_agent", { spec: { template: t.id, name: name.value, url: url?.value ?? null, headerName: hn?.value || null, headerValue: hv?.value || null,
        command: cmd?.value ?? null, cwd: st.cwd || null, count: st.count, worktree: st.count > 1, conda: st.conda || null, model: model?.value || null } });
      if (look) for (const n of added) await invoke("set_avatar", { name: n, avatar: look }).catch(() => {});
      await load(); setTab("chats"); openThread(`dm:${name.value.trim()}`);
    } catch (e) { err.textContent = String(e); go.disabled = false; }
  };
  return h("div", { class: "aform" }, ...parts, h("div", { class: "factions" }, err, go));
}
// ---------- sharing over Tailscale
async function copyText(text, btn) {
  try { await navigator.clipboard.writeText(text); if (btn) { btn.textContent = "已复制"; setTimeout(() => { btn.textContent = "复制"; }, 1200); } }
  catch { toast("复制失败，请手动选中复制"); }
}
async function shareDialog(agent) {
  const d = $("#dlg"), form = $("#dlg-form");
  const st = await invoke("share_status").catch((e) => ({ problem: String(e), shares: [] }));
  const mine = st.shares.filter((s) => s.agent === agent);
  const guest = h("input", { placeholder: "对方的名字，如 小王", maxlength: 24 });
  const st2 = { mode: "chat" };
  const modes = h("div", { class: "seg" });
  const drawModes = () => modes.replaceChildren(...[["chat", "只聊天", "需要批准的操作一律拒绝"], ["ask", "需要时问我", "由你在这台 Mac 上批准"]]
    .map(([v, t, sub]) => h("button", { type: "button", class: st2.mode === v ? "on" : "", onclick: () => { st2.mode = v; drawModes(); } }, h("b", {}, t), h("small", {}, sub))));
  drawModes();
  const limit = h("input", { type: "number", min: 0, max: 10000, value: 50 });
  const err = h("div", { class: "err" });
  const out = h("div", { class: "invite", hidden: true });
  const go = h("button", { class: "btn pri", type: "submit" }, "生成邀请码");
  const list = h("div", { class: "share-list" }, ...mine.map((s) => shareRow(s, () => shareDialog(agent))));
  form.replaceChildren(...[
    h("div", { class: "del-head" }, avatar(agent), h("div", {}, h("h3", {}, `共享 ${agent}`),
      h("p", { class: "del-sub" }, "通过 Tailscale 私有网络共享，只有拿到邀请码的人能用，随时可以撤销。"))),
    st.problem ? h("div", { class: "warn" }, st.problem, " ", h("button", { type: "button", class: "linkish", onclick: () => invoke("open_url", { url: "https://tailscale.com/download" }) }, "下载 Tailscale")) : null,
    h("label", {}, "共享给", guest),
    // Not a <label>: a click anywhere in a label also clicks its first button, so the
    // second choice could never stay selected.
    h("div", { class: "flabel" }, "权限", modes),
    h("label", {}, "每天最多几条消息（0 = 不限）", limit),
    err, out,
    mine.length ? h("div", { class: "muted3" }, "已共享") : null, list,
    h("div", { class: "actions" }, h("button", { class: "btn", type: "button", onclick: () => d.close() }, "关闭"), go)].filter(Boolean));
  form.onsubmit = async (e) => {
    e.preventDefault(); err.textContent = ""; go.disabled = true;
    try {
      const code = await invoke("create_share", { agent, guest: guest.value, mode: st2.mode, limit: Number(limit.value) || 0 });
      const cp = h("button", { type: "button", class: "btn", onclick: () => copyText(code, cp) }, "复制");
      out.hidden = false;
      out.replaceChildren(h("div", { class: "muted3" }, "把这串邀请码发给对方，让他在 Disco「添加 Agent → 朋友的邀请码」里粘贴。"),
        h("div", { class: "code-row" }, h("code", {}, code), cp));
      copyText(code); await load();
    } catch (e2) { err.textContent = String(e2); }
    go.disabled = false;
  };
  if (!d.open) d.showModal();
  guest.focus();
}
function shareRow(s, after) {
  const cp = h("button", { type: "button", class: "btn sm" }, "复制邀请");
  cp.onclick = async () => { try { copyText(await invoke("share_invite", { id: s.id }), null); cp.textContent = "已复制"; } catch (e) { toast(e); } };
  return h("div", { class: "share-row" },
    h("div", { class: "grow" }, h("b", {}, s.guest), h("small", {}, `${s.mode === "ask" ? "需要时问我" : "只聊天"} · 今天 ${s.used_today}${s.limit ? "/" + s.limit : ""} 条`)),
    cp, h("button", { type: "button", class: "btn sm danger", onclick: async () => { await invoke("revoke_share", { id: s.id }); toast(`已撤销 ${s.guest} 的访问`); await load(); after(); } }, "撤销"));
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
    const slot = h("span");
    const draw = (cs) => slot.replaceChildren(choice([["", "自动新建同名频道"], ...cs.map((c) => [`${c.guild_id}|${c.id}`, `用已有 #${c.name} · ${c.guild}`])], st.channel, (v) => { st.channel = v; }));
    draw([]);
    where.append(h("small", {}, "Discord"), slot);
    invoke("discord_channels").then(draw).catch(() => {});
    where.after(h("p", { class: "hint2" }, "想把 agent 带进朋友的群？先在 设置 → Discord 复制邀请链接发给朋友，他把你的 Bot 加进他的服务器后，这里就能选他的频道。频道里的每个人都能 @ 你的 agent，需要批准的操作只会问你。"));
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
    if (await ask("删除这个群聊？聊天记录保留，Discord 频道不受影响。", "删除")) {
      await invoke("delete_group", { id: g.id }); blank(); await load();
      toast(`已删除群聊 ${g.name}`, 6000, ["撤销", async () => { await invoke("restore_group", { id: g.id, guildId: g.guild_id, name: g.name, members: g.members, lead: g.lead }); await load(); setTab("groups"); openThread(`group:${g.id}`); }]);
    }
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
  box.replaceChildren(...(ms.length ? ms.map((m) => h("li", {}, h("b", {}, m.scope === "all" ? "所有 agent" : m.scope.startsWith("group-") ? (S.ov.groups.find((g) => `group-${g.id}` === m.scope)?.name ?? "群") : m.scope), h("small", { class: "mute" }, m.topic), h("span", { style: "flex:1" }, m.text),
    h("button", { class: "btn", onclick: async () => {
      await invoke("forget", { key: m.key }); loadMemories(box);
      toast("已删除这条记忆", 5000, ["撤销", async () => { await invoke("memory_restore", { key: m.key }).catch(toast); loadMemories(box); }]);
    } }, "删除"))) : [h("li", {}, h("span", {}, "还没有记忆"))]));
}
const inputStyle = "flex:1;height:32px;border:1px solid var(--line);border-radius:8px;padding:0 10px;background:var(--list);color:var(--fg)";
function renderSettings() {
  S.thread = null;
  const ok = /^在线/.test(S.ov.discord_status);
  const discordBox = h("div", {});
  if (S.ov.discord_token) loadDiscordSettings(discordBox);
  $("#pane").replaceChildren(h("div", { class: "page" },
    h("h1", {}, "设置"), h("p", {}, IS_WIN ? "密钥只存在 Windows 凭据管理器里。" : "密钥只存在 macOS 钥匙串里。"),
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
    h("div", { class: "section" }, h("h3", {}, "Tailscale 访问规则"), (() => { const box = h("div", {}, h("p", { class: "mute2" }, "正在读取…")); loadNet(box); return box; })()),
    h("div", { class: "section" }, h("h3", {}, "记忆"),
      h("p", { style: "margin:0 0 8px;color:var(--mute)" }, `在私聊里说「记住 …」存给这个 agent，在群聊里说存给本群；agent 也能用 remember 工具按主题记下东西。记忆是普通的 Markdown 文件，每次新会话自动带上，只发给这台${IS_WIN ? "电脑" : " Mac "}上的 agent。每个对话顶部的「记忆」开关可以单独关掉。`),
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

async function loadNet(box) {
  let p;
  try { p = await invoke("net_policy"); } catch (e) { box.replaceChildren(h("p", { class: "err" }, String(e))); return; }
  const lines = (v) => v.split(/[\s,，]+/).filter(Boolean);
  const area = (key, ph) => { const t = h("textarea", { class: "fld rules", rows: 2, placeholder: ph, spellcheck: false }); t.value = p[key].join("\n"); t.oninput = () => { p[key] = lines(t.value); }; return t; };
  const allowRow = h("label", { class: "nrow", hidden: p.mode !== "allowlist" }, h("span", {}, "允许名单"), area("allow", "每行一个：设备名、tag:名称 或 user:登录名"));
  const mode = h("div", { class: "seg" });
  const drawMode = () => mode.replaceChildren(...[["tailnet", "整个 Tailscale 网络", "有邀请码的设备都能用"], ["allowlist", "仅允许名单", "还要在名单里"]]
    .map(([v, t, sub]) => h("button", { type: "button", class: p.mode === v ? "on" : "", onclick: () => { p.mode = v; allowRow.hidden = v !== "allowlist"; drawMode(); } }, h("b", {}, t), h("small", {}, sub))));
  drawMode();
  const qs = h("input", { type: "time", class: "fld t", value: p.quiet_start }); qs.oninput = () => { p.quiet_start = qs.value; };
  const qe = h("input", { type: "time", class: "fld t", value: p.quiet_end }); qe.oninput = () => { p.quiet_end = qe.value; };
  const save = h("button", { class: "btn pri" }, "保存规则");
  save.onclick = async () => { save.disabled = true; try { Object.assign(p, await invoke("net_policy_set", { policy: p })); toast("已保存", 1500); } catch (e) { toast(e, 8000); } save.disabled = false; };
  box.replaceChildren(
    h("p", { class: "mute2" }, "朋友通过邀请码使用你共享的 agent 时，Disco 会向 Tailscale 确认对方是哪台设备，再按这里的规则放行。"),
    h("div", { class: "nrow" }, h("span", {}, "谁能用"), mode), allowRow,
    h("label", { class: "nrow" }, h("span", {}, "拒绝"), area("deny", "这些设备一律拒绝（优先于其他规则）")),
    h("div", { class: "nrow" }, h("span", {}, "免打扰"), qs, h("small", {}, "到"), qe, h("small", {}, "这段时间不接共享消息")),
    h("div", { class: "factions" }, save));
}
async function loadDiscordSettings(box) {
  let d;
  try { d = await invoke("discord_settings", {}); } catch (e) { box.replaceChildren(h("p", { class: "err" }, String(e))); return; }
  const guild = choice(d.guilds.map((g) => [g.id, g.guild]), d.home_guild, async (v) => {
    try { await invoke("discord_settings", { homeGuild: v }); toast("已切换主服务器，正在为每个 agent 建频道"); } catch (e) { toast(e); }
  }, d.guilds.length ? "选择服务器" : "Bot 还没加入任何服务器");
  const allow = h("input", { value: d.allow, placeholder: "其他可指挥 agent 的 Discord 用户 ID，逗号分隔", style: inputStyle });
  if (!d.guilds.length) {
    // Not invited yet (or Discord unreachable): one clear next step, then watch for the join.
    const copy = d.invite_url ? h("button", { class: "btn", onclick: (e) => copyText(d.invite_url, e.currentTarget) }, "复制邀请链接") : null;
    box.replaceChildren(h("div", { class: "warn", style: "margin-top:14px" },
      d.problem ? h("p", { style: "margin:0 0 8px" }, d.problem) : null,
      h("p", { style: "margin:0 0 8px" }, d.invite_url ? "Bot 还没加入任何服务器。点下面的按钮，在浏览器里选你的服务器并授权；浏览器没打开的话，复制链接自己打开。" : "暂时连不上 Discord，稍后会自动重试。"),
      d.invite_url ? h("div", { style: "display:flex;gap:8px" },
        h("button", { class: "btn pri", onclick: () => invoke("open_url", { url: d.invite_url }).catch(toast) }, "邀请 Bot 进服务器"), copy) : null));
    setTimeout(() => { if (box.isConnected) loadDiscordSettings(box); }, 4000);
    return;
  }
  box.replaceChildren(...[
    h("p", { style: "margin:14px 0 6px;color:var(--mute)" }, "主服务器：每个 agent 会在「Disco Agents」分类下有自己的频道，在频道里直接对它说话。"),
    h("div", { style: "display:flex;gap:8px" }, guild,
      h("button", { class: "btn", onclick: async (e) => {
        e.target.disabled = true;
        try { const n = await invoke("discord_sync"); toast(`已同步 ${n} 个 agent 频道`); } catch (er) { toast(er, 8000); }
        e.target.disabled = false;
      } }, "同步频道")),
    d.invite_url ? h("div", { style: "display:flex;gap:8px;margin-top:8px" },
      h("button", { class: "btn", onclick: () => invoke("open_url", { url: d.invite_url }).catch(toast) }, "重新授权 Bot"),
      h("button", { class: "btn", title: "发给朋友，他把你的 Bot 加进他的服务器后，你就能把 agent 拉进他的群频道", onclick: (e) => copyText(d.invite_url, e.currentTarget) }, "复制邀请链接（给朋友）")) : null,
    h("p", { style: "margin:14px 0 6px;color:var(--mute)" }, "群频道里每个能发言的人都能 @ agent；agent 自己的频道只有你能用。要让别人也能在 agent 频道里用，填他们的用户 ID："),
    h("div", { style: "display:flex;gap:8px" }, allow,
      h("button", { class: "btn", onclick: async () => {
        try { const r = await invoke("discord_settings", { allow: allow.value }); allow.value = r.allow; toast("已保存"); } catch (e) { toast(e); }
      } }, "保存"))].filter(Boolean));
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
const MOD = IS_WIN ? "Ctrl+" : "⌘";
function blank() {
  S.thread = null;
  const none = !S.ov?.agents.length;
  const box = h("div", { class: "blank" }, h("div", { class: "logo" }, h("img", { src: "logo.png", alt: "" }), h("i"), h("i"), h("i")), h("b", {}, "Disco"),
    none ? h("p", {}, "正在找这台电脑上的 agent…") : h("p", {}, "选一个会话开始"),
    none ? null : h("div", { class: "keys" },
      h("span", {}, h("kbd", {}, `${MOD}K`), " 搜索"), h("span", {}, h("kbd", {}, `${MOD}1–4`), " 切换"), h("span", {}, h("kbd", {}, IS_WIN ? "Ctrl+," : "⌘,"), " 设置"), h("span", {}, h("kbd", {}, `${MOD}N`), " 新建")));
  $("#pane").replaceChildren(box);
  renderList();
  if (none) firstRun(box);
}
// First launch: no setup screen. Found agents import with one click; Discord is optional.
async function firstRun(box) {
  const found = (await invoke("detect_agents").catch(() => [])).filter((t) => t.found);
  if (!box.isConnected) return;
  const rows = found.map((t) => h("div", { class: "frun" }, h("b", {}, t.name), h("small", { class: "mute" }, shortPath(t.found)),
    h("button", { class: "btn pri sm", onclick: (e) => quickImport(t, e.currentTarget) }, "开始聊天")));
  box.querySelector("p").textContent = found.length ? "找到这些 agent，点一下就能开始：" : "没在这台电脑上找到 agent。";
  box.append(...rows, ...(found.length ? [] : [h("button", { class: "btn pri", onclick: () => setTab("agents") }, "手动添加 agent")]),
    h("p", { class: "mute" }, "想在手机上也能用？以后到 设置 → Discord 连接，随时可以。"));
}
function setTab(tab) {
  S.tab = tab;
  if (tab !== "settings" && document.body.classList.contains("collapsed")) setCollapsed(false);
  document.querySelectorAll(".tab").forEach((b) => b.classList.toggle("on", b.dataset.tab === tab));
  if (tab === "agents") renderGallery();
  else if (tab === "settings") renderSettings();
  else if (!S.thread) blank();
  renderList();
}
document.querySelectorAll(".tab").forEach((b) => b.addEventListener("click", () => setTab(b.dataset.tab)));
$("#list-add").onclick = () => (S.tab === "agents" ? renderGallery() : groupDialog(null));
$("#search-kbd").textContent = `${MOD}K`;
$("#search").oninput = (e) => { S.q = e.target.value; renderList(); };
// Emergency stop lives in a status pill and needs two clicks, so reaching for 设置 never stops everything.
let killArm = 0;
function drawKill() {
  const k = $("#kill"), armed = !!killArm, paused = !!S.ov?.paused;
  k.className = paused ? "paused" : armed ? "armed" : "";
  const n = paused || armed ? 0 : busyAgents().size;
  k.querySelector("small").replaceChildren(paused ? "已停止" : armed ? "再点停止" : "运行中", n ? h("b", {}, String(n)) : "");
  k.title = paused ? "已全部停止，点一下恢复" : `${n ? `${n} 个 agent 正在回复。` : "运行中。"}点两下：全部停止所有 agent`;
}
$("#kill").onclick = async () => {
  if (S.ov.paused) { await invoke("resume"); await load(); drawKill(); toast("已恢复", 1500); return; }
  if (!killArm) { killArm = setTimeout(() => { killArm = 0; drawKill(); }, 2500); drawKill(); return; }
  clearTimeout(killArm); killArm = 0;
  const n = await invoke("stop_all"); await load(); drawKill();
  toast(`已停止 ${n} 个 agent，新消息暂不转发`, 6000, ["恢复", async () => { await invoke("resume"); await load(); }]);
};
$("#resume").onclick = async () => { await invoke("resume"); await load(); };
// The list column folds away (remembered); the rail keeps a button to bring it back.
function setCollapsed(on) {
  document.body.classList.toggle("collapsed", on);
  try { localStorage.setItem("disco.collapsed", on ? "1" : ""); } catch {}
}
setCollapsed(!!(() => { try { return localStorage.getItem("disco.collapsed"); } catch { return ""; } })());
$("#collapse").onclick = () => setCollapsed(true);
$("#expand").onclick = () => setCollapsed(false);

// Keyboard: ⌘K search, ⌘1–4 tabs, ⌘N new (group / agent), ↩ in search opens the first hit,
// and typing anywhere in a chat goes to the composer.
addEventListener("keydown", (e) => {
  const mod = e.metaKey || e.ctrlKey;
  const typingInField = /^(INPUT|TEXTAREA|SELECT)$/.test(document.activeElement?.tagName) || document.activeElement?.isContentEditable;
  if (mod && e.key.toLowerCase() === "k") { e.preventDefault(); $("#search").focus(); $("#search").select(); return; }
  if (mod && /^[1-4]$/.test(e.key)) { e.preventDefault(); setTab(["chats", "groups", "agents", "inbox"][e.key - 1]); return; }
  if (mod && e.key === "\\") { e.preventDefault(); setCollapsed(!document.body.classList.contains("collapsed")); return; }
  if (mod && e.key === ",") { e.preventDefault(); setTab("settings"); return; }
  if (mod && e.key.toLowerCase() === "n") { e.preventDefault(); if (S.tab === "agents") renderGallery(); else groupDialog(null); return; }
  if (!mod && !e.altKey && !typingInField && e.key.length === 1 && $("#input") && !$("#dlg").open) $("#input").focus();
});
// Text keeps one size: no page zoom from Ctrl/⌘ + wheel or + / − / 0.
addEventListener("wheel", (e) => { if (e.ctrlKey || e.metaKey) e.preventDefault(); }, { passive: false });
addEventListener("keydown", (e) => { if ((e.ctrlKey || e.metaKey) && ["+", "=", "-", "_", "0"].includes(e.key)) e.preventDefault(); }, true);
$("#search").addEventListener("keydown", (e) => {
  if (e.key === "Escape") { e.target.value = ""; S.q = ""; renderList(); e.target.blur(); }
  if (e.key === "Enter" && !e.isComposing) { e.preventDefault(); $("#items li:not(.empty)")?.click(); e.target.blur(); }
});

(async () => { await load(); blank(); setInbox(await invoke("inbox").catch(() => []), true); })();
