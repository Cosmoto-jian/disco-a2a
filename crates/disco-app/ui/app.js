"use strict";
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const $ = (s, r = document) => r.querySelector(s);

// ---------- state
// typing: thread -> Set(agent); live: "thread|agent" -> streaming bubble
// msgs: thread -> cached messages, so switching chats renders instantly.
// status: "thread|agent" -> { st: "think" | "tool", tool }; "wait" comes from the inbox.
const S = { setSec: null, teamMode: {}, tab: "chats", thread: null, ov: null, tpl: [], typing: {}, live: {}, inbox: [], team: {}, q: "", msgs: {}, found: null, status: {}, cmds: {}, unread: {} };
const REDUCED = matchMedia("(prefers-reduced-motion: reduce)").matches;
// ---------- avatars
// Agents: a translucent rounded square with an initial (two letters when two agents share one,
// the first character for CJK names), or the picture the user uploaded. People: grey circles.
// Groups: the group's first character on a tile with a faint offset tile behind it.
const agentOf = (name) => S.ov?.agents.find((a) => a.name === name);
const CJK = /[\u2e80-\u9fff\uf900-\ufaff\uac00-\ud7af]/;
const letters = (s) => [...String(s ?? "").replace(/[^\p{L}\p{N}]/gu, "")];
function initials(name) {
  const s = String(name ?? "").trim(), chars = letters(s);
  const first = chars[0] ?? [...s][0] ?? "?";
  if (CJK.test(first)) return first;
  const one = first.toUpperCase();
  const clash = (S.ov?.agents ?? []).some((a) => a.name !== s && (letters(a.name)[0] ?? "").toUpperCase() === one);
  return clash && chars[1] && !CJK.test(chars[1]) ? one + chars[1].toLowerCase() : one;
}
// An uploaded picture is a data URL; older builds stored "<form>.<palette>" there, which now means "no picture".
const picture = (name) => { const v = agentOf(name)?.avatar; return typeof v === "string" && v.startsWith("data:image/") ? v : null; };
function avatar(name, { group = false, person = false, small = false, size = "", live = false, status, busy = false, warn = false, discord = false, src } = {}) {
  const el = h("div", { class: ["av", small && "sm", size, group && "group", person && "person"].filter(Boolean).join(" ") });
  const img = group || person ? null : src !== undefined ? src : picture(name);
  if (img) { el.classList.add("pic"); el.append(h("img", { src: img, alt: "", draggable: "false" })); }
  else el.append(h("span", { class: "ini" }, group || person ? ([...String(name ?? "").trim()][0] ?? "#").toUpperCase() : initials(name)));
  if (status) { el.dataset.st = status.st; if (status.label) el.title = status.label; }
  if (status?.st === "wait") warn = true;
  if (status?.st === "think" || status?.st === "tool") busy = true;
  if (busy) el.classList.add("busy");
  if (warn) el.append(h("i", { class: "dot amber" }));
  else if (live) el.append(h("i", { class: "dot" }));
  if (discord) el.append(h("i", { class: "dc", title: "镜像到 Discord" }, ic("discord-logo:fill")));
  return el;
}

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
// macOS WKWebView can deliver the IME-confirming Enter with isComposing=false but keyCode 229.
const composing = (e) => e.isComposing || e.keyCode === 229;
const esc = (s) => s.replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
function hash(s) { let x = 0; for (const c of s) x = (x * 31 + c.codePointAt(0)) | 0; return Math.abs(x); }
const IS_WIN = navigator.userAgent.includes("Windows");
if (IS_WIN) document.documentElement.classList.add("win");
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
// Pick a picture, centre-crop it to a square and scale it to a 256 px PNG data URL (null = cancelled).
function pickImage() {
  return new Promise((resolve) => {
    const inp = h("input", { type: "file", accept: "image/png,image/jpeg,image/webp,image/gif" });
    inp.oncancel = () => resolve(null);
    inp.onchange = () => {
      const f = inp.files?.[0]; if (!f) return resolve(null);
      const url = URL.createObjectURL(f), img = new Image();
      img.onload = () => {
        const s = Math.min(img.naturalWidth, img.naturalHeight), c = document.createElement("canvas");
        c.width = c.height = 256;
        const g = c.getContext("2d"); g.imageSmoothingQuality = "high";
        g.drawImage(img, (img.naturalWidth - s) / 2, (img.naturalHeight - s) / 2, s, s, 0, 0, 256, 256);
        URL.revokeObjectURL(url); resolve(c.toDataURL("image/png"));
      };
      img.onerror = () => { URL.revokeObjectURL(url); toast("无法读取这张图片"); resolve(null); };
      img.src = url;
    };
    inp.click();
  });
}
// 上传图片 / 移除 next to a live preview; onPick(dataURL | null) on every change.
function avatarEditor(nameOf, current, onPick) {
  let pick = current;
  const slot = h("div", { class: "avslot" });
  const rm = h("button", { type: "button", class: "linkbtn", onclick: () => { pick = null; draw(); onPick(null); } }, "移除");
  const up = h("button", { type: "button", class: "linkbtn acc", onclick: async () => { const d = await pickImage(); if (d) { pick = d; draw(); onPick(d); } } }, ic("upload-simple"), "上传图片");
  const draw = () => { slot.replaceChildren(avatar(nameOf(), { size: "lg", src: pick })); rm.hidden = !pick; };
  draw();
  return { el: h("div", { class: "avedit" }, slot, h("div", { class: "avside" }, h("div", { class: "avbtns" }, up, rm), h("small", { class: "foot" }, "默认用名字首字母 · 支持 PNG / JPG，自动裁成方形"))), redraw: draw };
}
function avatarDialog(name) {
  let pick; // undefined = unchanged, null = back to the initial, string = new picture
  const ed = avatarEditor(() => name, picture(name), (v) => { pick = v; });
  dialog([h("h3", {}, `${name} 的头像`), ed.el], "保存", async () => {
    if (pick === undefined) return;
    await invoke("set_avatar", { agent: name, data: pick });
    await load();
    if (S.thread === `dm:${name}`) { S.thread = null; openThread(`dm:${name}`); }
  });
}

function when(ts) {
  if (!ts) return "";
  const d = new Date(ts * 1000), n = new Date();
  return d.toDateString() === n.toDateString()
    ? d.toLocaleTimeString("zh-CN", { hour: "2-digit", minute: "2-digit" })
    : d.toLocaleDateString("zh-CN", { month: "numeric", day: "numeric" });
}
// @name outside code becomes a tinted token.
const mentions = (html) => html.split(/(<code>[\s\S]*?<\/code>)/).map((p, i) => (i % 2 ? p
  : p.replace(/(^|[\s(（，,。>])@([\p{L}\p{N}_.-]*[\p{L}\p{N}_])/gu, '$1<span class="mt">@$2</span>'))).join("");
// Minimal, escape-first markdown: fenced code, inline code, bold, lists, paragraphs.
function md(src) {
  const parts = esc(src).split(/```(?:[\w-]*)\n?([\s\S]*?)```/g);
  return parts.map((p, i) => {
    if (i % 2) return `<pre><button class="copy" type="button" title="复制">${icHTML("copy")}</button><code>${p.replace(/\n$/, "")}</code></pre>`;
    return p.replace(/^(#{1,6} .*)$/gm, "\n$1\n").split(/\n{2,}/).map((blk) => {
      const inline = (t) => mentions(t.replace(/`([^`]+)`/g, "<code>$1</code>").replace(/\*\*([^*]+)\*\*/g, "<b>$1</b>"));
      const lines = blk.split("\n").filter((l) => l.trim());
      if (!lines.length) return "";
      if (lines.length === 1 && /^#{1,6} /.test(lines[0])) return `<h4>${inline(lines[0].replace(/^#+ /, ""))}</h4>`;
      if (lines.every((l) => /^\s*[-*]\s/.test(l))) return `<ul>${lines.map((l) => `<li>${inline(l.replace(/^\s*[-*]\s/, ""))}</li>`).join("")}</ul>`;
      if (lines.every((l) => /^\s*\d+[.)]\s/.test(l))) return `<ol>${lines.map((l) => `<li>${inline(l.replace(/^\s*\d+[.)]\s/, ""))}</li>`).join("")}</ol>`;
      if (lines.every((l) => /^&gt;\s?/.test(l))) return `<blockquote>${inline(lines.map((l) => l.replace(/^&gt;\s?/, "")).join("<br>"))}</blockquote>`;
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
    d.addEventListener("close", () => resolve(false), { once: true }); // Esc
    d.showModal();
  });
}

// ---------- data
// Overlapping loads (bursts of "agents" events, an action's own reload) may resolve out of order:
// only the newest one lands, and a stale caller waits for it, so `await load()` always sees fresh data.
let loadSeq = 0, loading = null;
function load() {
  const seq = ++loadSeq;
  return (loading = invoke("overview").then((ov) => {
    if (seq !== loadSeq) return loading;
    S.ov = ov;
    document.body.classList.toggle("paused", ov.paused);
    $("#paused-bar").hidden = !ov.paused;
    drawKill();
    renderList();
    if (S.thread) { $("#chat-sub")?.replaceChildren(...chatSub(S.thread)); drawMembers(); }
  }));
}
// The first overview: events and opens that arrive before it wait for it (in order) instead of reading S.ov == null.
let ready = null;
const onReady = (fn) => (e) => (S.ov ? fn(e) : ready?.then(() => fn(e)));
const peerOf = (t) => S.ov.peers.find((p) => p.thread === t);
const threadName = (t) => t.startsWith("dm:") ? t.slice(3)
  : t.startsWith("peer:") ? (peerOf(t) ? `${peerOf(t).guest} → ${peerOf(t).agent}` : "共享")
  : (S.ov.groups.find((g) => `group:${g.id}` === t)?.name ?? "群聊");

// ---------- list column
// Coalesce list redraws: typing/stream events can fire many times per frame.
// While a press is down on the list, hold redraws: swapping the row under the pointer between
// mousedown and mouseup would swallow the click. The held redraw runs right after the click.
let listRaf = 0, listHeld = false, listDirty = false;
function renderList() {
  if (!listRaf) listRaf = requestAnimationFrame(() => { listRaf = 0; if (listHeld) listDirty = true; else drawList(); });
}
$("#items").addEventListener("pointerdown", () => { listHeld = true; });
const releaseList = () => {
  if (!listHeld) return;
  listHeld = false;
  if (listDirty) { listDirty = false; setTimeout(renderList); }
};
addEventListener("pointerup", releaseList, true);
addEventListener("pointercancel", releaseList, true);
addEventListener("blur", releaseList); // the window losing focus, not the composer (no capture)
const KIND = { acp: "本机", openai: "模型 API", a2a: "A2A" };
const agentSub = (a) => [...new Set([KIND[a.kind] ?? "A2A", a.kind === "acp" ? shortPath(a.detail) : a.detail].filter(Boolean))].join(" · ");
// The git branch of a local agent's folder, as a tiny grey tag after a subtitle (nothing when there is none).
const branchTag = (a) => a?.branch ? h("span", { class: "branch", title: `git 分支 ${a.branch}` }, ic("git-branch"), a.branch) : null;
const withBranch = (sub, a) => a?.branch ? [sub, sub ? " · " : "", branchTag(a)] : sub;
const SETTINGS_SECS = [["dc", "Discord", "discord-logo:fill"], ["net", "共享与 Tailscale", "share-network"], ["mem", "记忆", "brain"], ["run", "运行", "power"], ["gen", "通用与更新", "gear-six"]];
const runLabel = () => (S.ov?.paused ? "已全部停止" : busyAgents().size ? `${busyAgents().size} 个 agent 正在回复` : "运行中");
function drawList() {
  if (!S.ov) return;
  const ul = $("#items"); ul.replaceChildren();
  // Same header on every tab: the compose button keeps its place (just goes quiet) so the search box never jumps.
  const add = $("#list-add"); add.classList.toggle("off", S.tab === "inbox" || S.tab === "settings");
  add.title = S.tab === "agents" ? `添加 agent（${MOD}N）` : `新建群聊（${MOD}N）`;
  const want = S.tab === "agents" ? "plus" : "note-pencil";
  if (add.dataset.ic !== want) { add.dataset.ic = want; add.replaceChildren(ic(want)); }
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
    rows = S.inbox.map((it) => h("li", { class: `note${S.thread === it.thread ? " on" : ""}`,
      onclick: (e) => { if (!e.target.closest("button, textarea")) openThread(it.thread); } }, inboxCard(it, true)));
  } else if (S.tab === "agents") {
    const busy = busyAgents();
    const act = (title, icon, run) => h("button", { class: "ib", title, onclick: (e) => { e.stopPropagation(); run(e.currentTarget); } }, ic(icon));
    rows = S.ov.agents.map((a) => h("li", { class: `card${S.thread === `dm:${a.name}` ? " on" : ""}${busy.has(a.name) ? " busy" : ""}`, title: a.detail, onclick: () => openThread(`dm:${a.name}`) },
      avatar(a.name, { live: a.live, busy: busy.has(a.name) }),
      h("div", { class: "meta" },
        h("div", { class: "row" }, h("span", { class: "name" }, a.name), a.kind === "acp" && OFFICIAL.has(a.template) ? h("span", { class: "tag" }, "官方") : null),
        h("div", { class: "sub" }, busy.has(a.name) ? "正在回复…" : withBranch(agentSub(a), a))),
      h("div", { class: "acts" },
        act("重启", "arrow-counter-clockwise", () => restartAgent(a.name)),
        a.kind !== "a2a" ? act("共享给朋友", "share-network", () => shareDialog(a.name)) : null,
        act("更多", "dots-three", (el) => agentMenu(el, a.name)))));
  } else {
    rows = SETTINGS_SECS.map(([id, label, icon]) => h("li", { class: `nav${S.setSec === id ? " on" : ""}`, onclick: () => gotoSetting(id) },
      h("span", { class: "navic" }, ic(icon)),
      h("div", { class: "meta" }, h("div", { class: "name" }, label),
        id === "dc" ? h("div", { class: "sub" }, S.ov.discord_status) : id === "run" ? h("div", { class: "sub" }, runLabel()) : null)));
  }
  rows = rows.filter((li) => !q || li.textContent.toLowerCase().includes(q));
  if (!rows.length) rows = [q ? h("li", { class: "empty" }, `没有和「${S.q}」相关的`)
    : S.tab === "inbox" ? h("li", { class: "empty calm" }, h("span", { class: "okmark" }, ic("check-circle")), h("b", {}, "没有等你处理的事"),
        h("small", {}, "agent 想跑终端命令、改文件，或者需要你拍板时，请求会出现在这里，同时在 Discord 里也能点。"))
    : h("li", { class: "empty" }, S.tab === "agents" ? "还没有 agent，点 ＋ 添加" : S.tab === "groups" ? "还没有群聊，点右上角新建" : "没有会话")];
  ul.append(...rows);
}
function threadRow(t, head, fallback) {
  const isGroup = t.startsWith("group:"); const name = threadName(t);
  const typing = S.typing[t]?.size ? [...S.typing[t]].join("、") : "";
  const me = !isGroup && S.ov.agents.find((a) => `dm:${a.name}` === t), live = me?.live;
  // Replying with a plan: the step it is on says more than the last message.
  const doing = me?.busy_since && me.doing ? me.doing : "";
  const waiting = S.inbox.filter((it) => it.thread === t).length;
  // The last word was a system notice (a failed start, an MCP server needing attention): flag it.
  const warn = !typing && head?.author === "system";
  const unread = S.thread === t ? 0 : S.unread[t] ?? 0;
  const peer = peerOf(t);
  const g = isGroup && S.ov.groups.find((g) => `group:${g.id}` === t);
  const title = peer ? h("span", { class: "name route" }, peer.guest, ic("caret-right", "arrow"), h("b", {}, peer.agent)) : h("span", { class: "name" }, name);
  const av = peer ? avatar(peer.agent, { busy: !!typing })
    : isGroup ? avatar(name, { group: true, discord: g && !g.id.startsWith("local-"), busy: !!typing, warn })
    : avatar(name, { live, warn, busy: !!typing, status: statusOf(t, name) });
  if (warn) av.title = "需要留意，点开看看";
  return h("li", { class: `${S.thread === t ? "on" : ""}${unread ? " unread" : ""}${typing ? " busy" : ""}`, onclick: () => openThread(t), onmouseenter: () => prefetch(t) },
    av,
    waiting ? h("i", { class: "badge row-badge", title: `${waiting} 项等你处理` }, String(waiting)) : null,
    h("div", { class: "meta" },
      h("div", { class: "row" }, title, h("span", { class: "time" }, when(head?.ts))),
      h("div", { class: "row" },
        doing ? h("div", { class: "sub", title: doing }, doing) : h("div", { class: `sub${typing ? " typing" : ""}` }, typing ? `${typing} ${isGroup ? "正在回复" : ST_LABEL[S.status[`${t}|${name}`]?.st] ?? "正在输入"}…` : head ? `${isGroup && head.author !== "me" && head.author !== "system" ? head.author.replace(/ \(Discord\)$/, "") + "：" : ""}${head.body}` : (fallback ?? "")),
        unread ? h("i", { class: "unread-n" }, unread > 99 ? "99+" : String(unread)) : null),
    ));
}

// ---------- conversation
function sendButton() {
  return h("button", { id: "send", class: "send", type: "button", title: "发送（↩）", disabled: true, onclick: () => sendMsg() }, ic("arrow-up:bold", "i-up"), ic("stop:fill", "i-stop"));
}
function prefetch(t) {
  if (S.msgs[t] || S.msgs[t] === null) return;
  S.msgs[t] = null; // in flight
  invoke("messages", { thread: t }).then((m) => { S.msgs[t] = m; }).catch(() => { delete S.msgs[t]; });
}
const groupOf = (t) => S.ov.groups.find((g) => `group:${g.id}` === t);
function placeholderFor(t) {
  const peer = peerOf(t);
  if (peer) return `在 ${peer.guest} 的群里说话`;
  if (t.startsWith("group:")) return S.teamMode[t] ? "说一个任务，成员分工、组长汇总" : `发消息到 ${threadName(t)}`;
  return `发消息给 ${threadName(t)}`;
}
// Grey line under the title: where the chat lives, who is in it, how it answers.
function chatSub(t) {
  const peer = peerOf(t);
  if (peer) return [peer.group ? `${peer.guest} 的群 · 在用你的 ${peer.agent}` : `${peer.guest} 通过共享在用你的 ${peer.agent}`];
  if (t.startsWith("group:")) {
    const g = groupOf(t); if (!g) return [];
    const local = g.id.startsWith("local-"), multi = g.people.length || g.guests.length, ch = g.channel_name ?? g.channel;
    return [h("i", { class: `mirror${local ? "" : " on"}` }), local ? "仅本地" : `镜像到 Discord${ch ? ` #${ch}` : ""}`,
      ` · ${g.members.length} 个 agent`, multi ? ` · ${g.people.length + 1} 人 · @ 才回答` : ` · 组长 ${g.lead ?? g.members[0] ?? "—"}`,
      S.teamMode[t] ? " · 团队模式" : ""];
  }
  const a = agentOf(t.slice(3));
  return a ? [withBranch(agentSub(a), a)].flat() : [];
}
async function openThread(t) {
  if (!S.ov) await ready;
  if (S.thread === t && $("#log")) return;
  const isGroup = t.startsWith("group:"); const name = threadName(t);
  // Opened from a toast on 设置: leave settings so a status event can't repaint over the chat.
  if (S.tab === "settings") { S.tab = isGroup ? "groups" : "chats"; markTabs(); }
  S.thread = t; delete S.unread[t]; renderList();
  const peer = peerOf(t);
  const g = isGroup && groupOf(t);
  if (isGroup && !g) { blank(); renderList(); return; } // group was deleted
  const a = !isGroup && agentOf(name);
  const headAv = a ? Object.assign(avatar(name, { live: a.live }), { title: "更换头像", onclick: () => avatarDialog(name) })
    : peer ? avatar(peer.agent) : g ? avatar(name, { group: true, discord: !g.id.startsWith("local-") }) : avatar(name);
  if (a) headAv.classList.add("pickable");
  const head = h("div", { class: "head", "data-tauri-drag-region": true }, headAv,
    h("div", { class: "tt" }, h("h2", { title: a ? "双击重命名" : null, ondblclick: a ? (e) => inlineRename(e.currentTarget, a.name) : null }, name), h("small", { id: "chat-sub" }, ...chatSub(t))),
    a && a.kind !== "a2a" ? h("div", { id: "cfgs" }) : null,
    h("div", { class: "hbtns" },
      peer ? null : h("button", { class: "ib mem-btn", type: "button", onclick: () => flipMemory(t) }, ic("brain")),
      peer ? null : h("button", { class: "ib space-btn", type: "button", title: "待办", onclick: () => openInspector(t, "todo") }, ic("list-checks"), h("i", { class: "badge acc", id: "todo-count" })),
      h("button", { class: "ib insp-btn", type: "button", title: isGroup ? "成员、待办和群文档" : "信息、待办和文档", onclick: () => toggleInspector(t) }, ic("sidebar-simple")),
      a ? h("button", { class: "ib more-btn", type: "button", title: "更多", onclick: (e) => agentMenu(e.currentTarget, a.name) }, ic("dots-three")) : null));
  const bottom = peer && !peer.talk
    ? h("div", { class: "peer-note" }, peer.group ? `对方在同步到 Discord 的群里用你的 agent。${peer.missing}` : "你现在只能旁观。想一起聊：对方建一个同步到 Discord 的群，拉你和你的 Bot 进去，再在群里 @ 一次你的 agent，这里就会变成聊天。")
    : h("div", { id: "composer", class: S.teamMode[t] ? "team" : "" },
      peer ? null : h("button", { class: "ib attach", type: "button", title: "放一个文件到共享空间，agent 都能读到", onclick: () => attachFile(t) }, ic("paperclip")),
      isGroup ? h("button", { class: `team-chip${S.teamMode[t] ? " on" : ""}`, type: "button", title: "团队模式：成员分工并行处理，最后由组长汇总", onclick: () => setTeamMode(t, !S.teamMode[t]) }, ic("squares-four"), "团队") : null,
      h("textarea", { id: "input", rows: 1, placeholder: placeholderFor(t), title: isGroup ? "@名字 叫 agent 回答，@all 一起回答，/help 看群规" : peer ? null : "/ 看它的命令" }),
      sendButton());
  const pane = $("#pane");
  pane.className = "chat";
  pane.replaceChildren(h("div", { class: "chatcol" }, head, h("div", { id: "log" }), bottom));
  void pane.offsetWidth; pane.classList.add("enter");
  syncMemory(t);
  if (S.insp) drawInspector(t);
  const ta = $("#input") ?? h("textarea");
  ta.addEventListener("input", () => {
    ta.style.height = "auto"; ta.style.height = ta.scrollHeight + "px";
    const btn = $("#send"); if (btn && !btn.classList.contains("stop")) btn.disabled = !ta.value.trim();
  });
  const mention = isGroup ? mentionPicker(ta, () => [...g.members, ...g.guests, ...g.people]) : null;
  const slash = slashPicker(ta, t);
  ta.addEventListener("keydown", (e) => {
    if (mention?.key(e) || slash.key(e)) return;
    if (e.key === "Enter" && !e.shiftKey && !composing(e)) { e.preventDefault(); sendMsg(); }
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
  if (isGroup) invoke("team_state", { thread: t }).then((st) => { if (st) { const had = S.team[t]; S.team[t] = st; renderTeam(t); if (!had && S.insp) drawInspector(t); } }).catch(() => {});
  const draw = (msgs) => {
    const log = $("#log"); if (!log || S.thread !== t) return;
    log.replaceChildren(...msgs.map((m) => bubble(m.author, m.body, m.ts)));
    for (const el of log.children) runify(el);
    markResend(log);
    for (const it of S.inbox) if (it.thread === t) log.append(inboxCard(it));
    for (const [k, v] of Object.entries(S.live)) if (k.startsWith(t + "|") && v.on) { log.append(v.el); runify(v.el); }
    log.scrollTop = log.scrollHeight;
  };
  if (Array.isArray(S.msgs[t])) draw(S.msgs[t]);
  const fresh = await invoke("messages", { thread: t }).catch(() => null);
  if (!fresh) return;
  const had = S.msgs[t];
  S.msgs[t] = fresh;
  if (!Array.isArray(had) || had.length !== fresh.length) draw(fresh);
}
// Group mode: plain chat (@ who answers) or team (/team: split the task, lead sums up) — a chip in the composer.
function setTeamMode(t, on) {
  if (on) S.teamMode[t] = true; else delete S.teamMode[t];
  if (S.thread !== t) return;
  $("#composer")?.classList.toggle("team", on);
  $(".team-chip")?.classList.toggle("on", on);
  const i = $("#input"); if (i) { i.placeholder = placeholderFor(t); i.focus(); }
  $("#chat-sub")?.replaceChildren(...chatSub(t));
}
// The chat's memory switch: the header brain and the inspector row flip the same setting.
const memOn = (t) => !S.ov.memory_off.includes(t);
async function flipMemory(t) {
  const prev = S.ov.memory_off, next = !memOn(t);
  S.ov.memory_off = next ? prev.filter((x) => x !== t) : [...prev, t];
  syncMemory(t);
  try {
    await invoke("set_memory", { thread: t, on: next });
    toast(next ? "记忆已开启" : "记忆已关闭：这个对话不再读取或保存记忆", 2200);
  } catch (e) { S.ov.memory_off = prev; syncMemory(t); toast(e); }
}
function syncMemory(t) {
  if (S.thread !== t) return;
  const on = memOn(t), b = $(".mem-btn");
  if (b) { b.classList.toggle("on", on); b.setAttribute("aria-pressed", String(on)); b.title = on ? "记忆：开 · agent 会读取并保存这个对话的长期记忆" : "记忆：关 · 这个对话既不读也不写记忆"; }
  document.querySelectorAll(".memrow").forEach((r) => r.setAttribute("aria-checked", String(on)));
}
function memorySwitch(t) {
  const row = h("div", { class: "li memrow", role: "switch", tabindex: 0, "aria-checked": String(memOn(t)), "aria-label": "记忆",
    title: "开启时 agent 会读取并保存这个对话的长期记忆；关闭时既不读也不写" },
    h("div", { class: "t" }, "记忆", h("small", {}, "读取并保存这个对话的长期记忆")), h("span", { class: "track" }, h("i")));
  row.onclick = () => flipMemory(t);
  row.onkeydown = (e) => { if (e.key === " " || e.key === "Enter") { e.preventDefault(); flipMemory(t); } };
  return row;
}
// Who is doing what right now: the status line over the composer and the inspector's member list.
function drawMembers() {
  const t = S.thread; if (!t) return;
  if (t.startsWith("group:")) renderTeam(t);
  if (S.insp && S.inspTab === "members" && $("#insp-body")) drawInfo(t);
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
  (mine.querySelector(".tools") ?? mine).append(h("button", { class: "ib again", type: "button", title: "再发一遍", onclick: () => resend(body) }, ic("arrow-clockwise")));
}
async function resend(text) {
  const t = S.thread; if (!t || !text) return;
  const el = bubble("me", text); el.classList.add("new", "pending"); el.dataset.body = text;
  $("#log").append(el); scrollLog(true);
  try { await invoke("send", { thread: t, text }); } catch (e) { failed(el, e); }
}
function failed(el, e) {
  el.classList.add("failed"); el.title = String(e);
  el.append(h("button", { class: "retry", type: "button", onclick: () => { el.remove(); resend(el.dataset.body); } }, "发送失败 · 重发"));
}

// "/" at the start (or after "@agent ") offers that agent's own commands; groups add their own.
const CMD_MISS = {}; // agent → retry-after ms: don't re-ask an agent with no commands on every keystroke
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
    const m = before.match(/^(?:@([\p{L}\p{N}_-]+)\s+)?\/([\p{L}\p{N}_-]*)$/u);
    if (!m) return close();
    const agent = m[1] ?? (group ? null : t.slice(3));
    start = ta.selectionStart - m[2].length - 1;
    let list = group && !agent ? GROUP_CMDS : [];
    if (agent && S.ov.agents.some((a) => a.name === agent && a.kind === "acp")) {
      if (!S.cmds[agent] && !(CMD_MISS[agent] > Date.now())) {
        const c = await invoke("agent_commands", { agent }).catch(() => []);
        if (c.length) S.cmds[agent] = c; else CMD_MISS[agent] = Date.now() + 10000; // not known yet: ask again in 10s
        // The user moved on while we waited (other chat, blur, more typing): don't pop a stale menu.
        if (!ta.isConnected || document.activeElement !== ta || ta.value.slice(0, ta.selectionStart) !== before) return;
      }
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
    if (!box || composing(e)) return false;
    if (e.key === "ArrowDown" || e.key === "ArrowUp") { idx = (idx + (e.key === "ArrowDown" ? 1 : items.length - 1)) % items.length; draw(); }
    else if (e.key === "Enter" || e.key === "Tab") pick(items[idx][0]);
    else if (e.key === "Escape") close();
    else return false;
    e.preventDefault(); return true;
  } };
}

// ---------- team mode: a slim status line over the composer; the blackboard lives in the inspector
const TEAM_STAGE = { plan: "正在拆分任务", work: "成员并行处理中", summary: "组长正在汇总", done: "已完成", failed: "未完成" };
const ROLE_STATE = { working: "在", done: "完成了", failed: "没完成", pending: "等待" };
const dots = () => h("span", { class: "dots" }, h("i"), h("i"), h("i"));
function renderTeam(t) {
  if (S.thread !== t) return;
  const old = $("#team"), st = S.team[t];
  const team = st && !st.dismissed ? st : null;
  const busy = t.startsWith("group:") ? [...(S.typing[t] ?? [])] : [];
  if (!team && !busy.length) { old?.remove(); return; }
  let el;
  if (team) {
    const active = ["plan", "work", "summary"].includes(st.stage);
    const words = st.stage === "work" && st.roles.length
      ? st.roles.flatMap((r, i) => [i ? " · " : "", h("b", {}, r.agent), ` ${ROLE_STATE[r.state] ?? ""} ${r.role}`.replace(/\s+/g, " ")])
      : [TEAM_STAGE[st.stage] ?? st.stage];
    const tip = [st.task, ...st.roles.map((r) => `${r.agent}·${r.role}：${r.task}${r.last ? `\n  正在：${r.last}` : ""}`)].filter(Boolean).join("\n");
    el = h("div", { id: "team", class: active ? "active" : st.stage, title: tip },
      active ? dots() : ic(st.stage === "done" ? "check-circle" : "warning", "stage"),
      h("span", { class: "words" }, ...words),
      h("button", { class: "ib board-btn", type: "button", title: "团队黑板", onclick: () => toggleBoard(t) }, ic("chalkboard")),
      active ? null : h("button", { class: "ib", type: "button", title: "收起", onclick: () => { st.dismissed = true; renderTeam(t); } }, ic("x")));
  } else {
    el = h("div", { id: "team", class: "active typing" }, dots(), h("span", { class: "words" }, ...busy.flatMap((a, i) => {
      const s = S.status[`${t}|${a}`];
      return [i ? " · " : "", h("b", {}, a), s?.st === "tool" ? ` 调工具${s.tool ? "：" + s.tool : ""}` : " 正在回复"];
    })));
  }
  if (old) old.replaceWith(el); else $("#composer")?.before(el);
}
listen("team", ({ payload: st }) => {
  const prev = S.team[st.thread];
  S.team[st.thread] = st;
  renderTeam(st.thread);
  if (S.thread !== st.thread || !S.insp) return;
  if (!prev) drawInspector(st.thread); // the 黑板 tab appears
  else if (S.inspTab === "board" && prev.stage !== st.stage) loadBoard(st.thread);
});
const toggleBoard = (t) => openInspector(t, "board");
async function loadBoard(t, pick) {
  const files = await invoke("team_files", { thread: t }).catch(() => []);
  const box = $("#insp-body"); if (!box || S.thread !== t || S.inspTab !== "board") return;
  const label = (n) => n === "summary.md" ? "结论" : n === "context.md" ? "简报" : n.replace(/^findings\//, "").replace(/\.md$/, "");
  const cur = files.find(([n]) => n === pick) ?? files[0];
  const doc = h("div", { class: "doc md" }); doc.innerHTML = cur ? md(cur[1]) : '<p class="foot">还没有团队任务。在群里打开「团队」再说一个任务就会开始。</p>';
  box.replaceChildren(
    files.length ? h("div", { class: "bseg" }, ...files.map(([n]) => h("button", { type: "button", class: cur && cur[0] === n ? "on" : "", onclick: () => loadBoard(t, n) }, label(n)))) : null,
    h("div", { class: "icard pad" }, doc));
}

// ---------- inspector: the right sidebar (成员/信息 | 待办 | 文档 | 黑板). It pushes the chat over, never covers it.
S.insp = (() => { try { return localStorage.getItem("disco.insp") === "1"; } catch { return false; } })();
S.inspTab = "members"; S.left = {};
const saveInsp = () => { try { localStorage.setItem("disco.insp", S.insp ? "1" : ""); } catch {} };
function toggleInspector(t) { S.insp = !S.insp; saveInsp(); drawInspector(t, true); }
function openInspector(t, tab) {
  if (S.insp && S.inspTab === tab) S.insp = false; else { S.insp = true; S.inspTab = tab; }
  saveInsp(); drawInspector(t, true);
}
function drawInspector(t, anim = false) {
  if (S.thread !== t) return;
  $(".insp-btn")?.classList.toggle("on", S.insp);
  $(".space-btn")?.classList.toggle("on", S.insp && S.inspTab === "todo");
  let box = $("#insp");
  if (!S.insp) { box?.remove(); return; }
  const peer = peerOf(t), isGroup = t.startsWith("group:");
  const tabs = [["members", isGroup ? "成员" : "信息"], ...(peer ? [] : [["todo", "待办"], ["doc", "文档"]]), ...(isGroup && S.team[t] ? [["board", "黑板"]] : [])];
  if (!tabs.some(([k]) => k === S.inspTab)) S.inspTab = "members";
  $(".space-btn")?.classList.toggle("on", S.inspTab === "todo");
  if (!box) { box = h("aside", { id: "insp", class: anim ? "new" : "" }); $("#pane").append(box); }
  const left = S.left[t];
  const seg = h("div", { class: "seg2", role: "tablist" }, ...tabs.map(([k, label]) => h("button", { type: "button", role: "tab", class: S.inspTab === k ? "on" : "", "aria-selected": String(S.inspTab === k),
    onclick: () => { S.inspTab = k; drawInspector(t); } }, label, k === "todo" ? h("span", { class: "n", id: "seg-todo" }, left ? ` ${left}` : "") : null)));
  box.replaceChildren(h("div", { class: "ih", "data-tauri-drag-region": true }, seg), h("div", { class: "insp-body", id: "insp-body" }));
  if (S.inspTab === "members") drawInfo(t);
  else if (S.inspTab === "board") loadBoard(t);
  else loadSpace(t);
}
// Grouped inset list helpers (System Settings style): one rounded card, hairline separators.
const isec = (title, ...rows) => h("div", { class: "isec" }, title ? h("div", { class: "lab" }, title) : null, h("div", { class: "icard" }, ...rows.flat().filter(Boolean)));
const irow = (left, title, sub, right, onclick, cls = "") => h(onclick ? "button" : "div", { class: `li${cls ? " " + cls : ""}`, type: onclick ? "button" : null, onclick },
  left, h("div", { class: "t" }, title, sub ? h("small", {}, sub) : null), right);
const lic = (name) => h("span", { class: "lic" }, ic(name));
const chev = () => ic("caret-right", "chev");
function drawInfo(t) {
  const body = $("#insp-body"); if (!body) return;
  const peer = peerOf(t), name = threadName(t);
  if (t.startsWith("group:")) {
    const g = groupOf(t); if (!g) return;
    const team = S.team[t], lead = g.lead ?? g.members[0];
    const local = g.id.startsWith("local-"), ch = g.channel_name ?? g.channel;
    const agents = g.members.map((m) => {
      const s = statusOf(t, m), busy = S.typing[t]?.has(m), mine = agentOf(m);
      const sub = busy ? (s?.st === "tool" ? `调工具${s.tool ? "：" + s.tool : ""}` : "正在回复…") : s?.st === "wait" ? "等你确认"
        : [m === lead ? "组长" : "", mine ? KIND[mine.kind] ?? "" : "", team?.roles.find((r) => r.agent === m)?.role ?? ""].filter(Boolean).join(" · ");
      return irow(avatar(m, { small: true, live: mine?.live, status: s, busy }), m, sub,
        mine ? h("button", { class: "ib", type: "button", title: "更多", onclick: (e) => agentMenu(e.currentTarget, m) }, ic("dots-three")) : null);
    });
    body.replaceChildren(
      isec("AGENT", agents, g.guests.map((m) => irow(avatar(m, { small: true }), m, "朋友的 agent")),
        irow(lic("plus"), "添加 agent", null, null, () => groupDialog(g), "add")),
      isec("人", irow(avatar("我", { person: true, small: true }), "我", "你 · 群主"), g.people.map((p) => irow(avatar(p, { person: true, small: true }), p, "Discord"))),
      isec("本群", memorySwitch(t),
        irow(null, local ? "仅本地" : "镜像到 Discord", local ? "不同步到 Discord" : ch ? `#${ch}` : "已绑定 Discord 频道", chev(), () => groupDialog(g)),
        irow(null, "群设置", "群名、成员、组长、删除", chev(), () => groupDialog(g))));
    return;
  }
  if (peer) {
    body.replaceChildren(
      isec(null, irow(avatar(peer.agent, { small: true }), peer.agent, "你共享出去的 agent"),
        irow(avatar(peer.guest, { person: true, small: true }), peer.guest, peer.group ? "在 Discord 群里使用" : "通过邀请码使用")),
      isec(null, irow(null, peer.talk ? "可以说话" : "只能旁观", peer.talk ? null : peer.missing || null)));
    return;
  }
  const a = agentOf(name); if (!a) return;
  const val = (v, cls = "") => h("span", { class: `val${cls}`, title: v }, v);
  body.replaceChildren(
    h("div", { class: "ihero" }, avatar(name, { size: "lg", live: a.live, status: statusOf(t, name), busy: S.typing[t]?.has(name) }), h("b", {}, name), h("small", {}, withBranch(agentSub(a), a))),
    isec(null, irow(null, "类型", null, val(KIND[a.kind] ?? "A2A")),
      a.detail ? irow(null, a.kind === "acp" ? "目录" : "地址", null, val(a.kind === "acp" ? shortPath(a.detail) : a.detail, " mono")) : null,
      irow(null, "状态", null, val(a.live ? "在线" : "未启动"))),
    isec("本对话", memorySwitch(t)),
    isec(null, irow(lic("image"), "更换头像…", null, null, () => avatarDialog(name)),
      a.kind !== "a2a" ? irow(lic("share-network"), "共享给朋友…", null, null, () => shareDialog(name)) : null,
      irow(lic("pencil-simple"), "重命名", null, null, () => { const h2 = $(".head h2"); if (h2) inlineRename(h2, name); }),
      irow(lic("arrow-counter-clockwise"), "重启", null, null, () => restartAgent(name)),
      irow(lic("trash"), "删除 Agent…", null, null, () => removeAgent(name), "danger")));
}
// ---------- space: the thread's shared to-dos and doc (agents edit them through Disco tools)
async function loadSpace(t) {
  let sp; try { sp = await invoke("space", { thread: t }); } catch { return; }
  if (S.thread !== t) return;
  const left = sp.todos.filter((x) => !x.done).length; S.left[t] = left;
  const cnt = $("#todo-count"); if (cnt) cnt.textContent = left ? String(left) : "";
  const sc = $("#seg-todo"); if (sc) sc.textContent = left ? ` ${left}` : "";
  const box = $("#insp-body"); if (!box || !S.insp || (S.inspTab !== "todo" && S.inspTab !== "doc")) return;
  if (S.inspTab === "todo") {
    const who = (a) => (a === "me" ? "我" : a);
    const add = h("input", { placeholder: "加一项待办，回车", class: "todo-in" });
    add.onkeydown = async (e) => {
      if (e.key !== "Enter" || composing(e) || !add.value.trim()) return;
      const text = add.value; add.value = "";
      try { await invoke("todo_new", { thread: t, text }); } catch (er) { if (!add.value) add.value = text; toast(er); }
    };
    const todos = h("ul", { class: "todos icard" }, ...sp.todos.map((x) => h("li", { class: x.done ? "done" : "" },
      h("button", { type: "button", class: `chk${x.done ? " on" : ""}`, role: "checkbox", "aria-checked": String(x.done), title: x.done ? "标记为未完成" : "完成",
        onclick: (e) => { x.done = !x.done; e.currentTarget.classList.toggle("on", x.done); e.currentTarget.closest("li").classList.toggle("done", x.done); invoke("todo_set", { thread: t, id: x.id, done: x.done }).catch(toast); } }, ic("check:bold")),
      h("div", { class: "tx" }, h("span", {}, x.text), x.by && x.by !== "me" ? h("small", {}, `${x.by} 添加`) : null),
      x.assignee ? h("span", { class: "who", title: `负责：${who(x.assignee)}` }, x.assignee === "me" ? avatar("我", { person: true, size: "xs" }) : avatar(x.assignee, { size: "xs", person: !agentOf(x.assignee) && x.assignee.length > 0 && !S.ov.groups.some((g) => g.members.includes(x.assignee) || g.guests.includes(x.assignee)) })) : null,
      h("button", { class: "ib del", type: "button", title: "删除", onclick: async () => {
        try { await invoke("todo_remove", { thread: t, id: x.id }); toast("已删除待办", 5000, ["撤销", () => invoke("todo_new", { thread: t, text: x.text }).catch(toast)]); } catch (e) { toast(e); }
      } }, ic("x")))));
    box.replaceChildren(sp.todos.length ? todos : h("p", { class: "foot" }, "还没有待办。agent 也能用 Disco 工具添加和勾选。"), add);
    return;
  }
  const doc = h("div", { class: "doc md" });
  doc.innerHTML = sp.doc.trim() ? md(sp.doc) : '<p class="foot">还没有内容。agent 可以用 doc_write 写入目标、结论和分工，你也可以直接编辑。</p>';
  const edit = h("button", { class: "btn sm", type: "button" }, "编辑");
  edit.onclick = () => {
    const ta = h("textarea", { class: "doc-edit" }); ta.value = sp.doc;
    const save = h("button", { class: "btn sm pri", type: "button" }, "保存");
    save.onclick = async () => { try { await invoke("doc_save", { thread: t, content: ta.value }); } catch (er) { toast(er); } };
    doc.closest(".icard").replaceWith(ta); edit.replaceWith(save); ta.focus();
  };
  box.replaceChildren(
    h("div", { class: "dhead" }, h("span", { class: "lab" }, t.startsWith("group:") ? "群文档" : "文档"), h("span", { class: "sp" }), edit,
      h("button", { class: "btn sm", type: "button", title: sp.doc_path, onclick: () => invoke("reveal_doc", { thread: t }).catch(toast) }, IS_WIN ? "在资源管理器中显示" : "在访达中显示")),
    h("div", { class: "icard pad" }, doc));
}
listen("space", ({ payload: t }) => {
  const ti = $("#insp .todo-in"); // don't wipe a to-do being typed
  if (S.thread === t && !$("#insp .doc-edit") && !(ti && (ti.value || ti === document.activeElement))) loadSpace(t);
});

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
  const pill = h("button", { class: "model-pill", title: "切换模型" }, h("span", { class: "dot" }), h("span", { class: "lbl" }, label()), ic("caret-down", "chev"));
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
  const pill = h("button", { type: "button", class: "model-pill choice" }, h("span", { class: "lbl" }, label()), ic("caret-down", "chev"));
  pill.onclick = () => popover(pill, options.map(([v, l]) => ({ label: l, on: v === value, run: () => { value = v; pill.querySelector(".lbl").textContent = label(); onPick(v); } })));
  return pill;
}
// A small anchored menu; closes on outside click or Esc. An item with `arm` needs a second click
// (the label changes to `arm` for 2.5 s), so a stray click never runs it.
function popover(anchor, items) {
  document.querySelector(".pop")?.remove();
  const pop = h("div", { class: "pop" }, ...items.map((it) => h("button", {
    type: "button", class: `pop-item${it.on ? " on" : ""}${it.danger ? " danger" : ""}`,
    onclick: (e) => {
      const b = e.currentTarget;
      if (it.arm && !b.dataset.armed) {
        b.dataset.armed = "1"; b.classList.add("armed"); b.querySelector(".lt").textContent = it.arm;
        setTimeout(() => { if (b.isConnected) { delete b.dataset.armed; b.classList.remove("armed"); b.querySelector(".lt").textContent = it.label; } }, 2500);
        return;
      }
      close(); it.run();
    },
  }, h("span", { class: "pl" }, it.icon ? ic(it.icon) : null, h("span", { class: "lt" }, it.label)), it.on ? ic("check", "pop-check") : null)));
  // Long lists (a provider's hundreds of models) get a filter box.
  if (items.length > 12) {
    const f = h("input", { class: "pop-filter", placeholder: "筛选…", spellcheck: false });
    f.oninput = () => { const q = f.value.toLowerCase(); for (const b of pop.querySelectorAll(".pop-item")) b.hidden = !b.textContent.toLowerCase().includes(q); };
    f.onkeydown = (e) => { if (e.key === "Enter") pop.querySelector(".pop-item:not([hidden])")?.click(); };
    pop.prepend(f); setTimeout(() => f.focus());
  }
  document.body.append(pop);
  const r = anchor.getBoundingClientRect();
  const below = r.bottom + 6 + pop.offsetHeight < innerHeight - 8;
  pop.style.top = `${below ? r.bottom + 6 : Math.max(8, r.top - 6 - pop.offsetHeight)}px`;
  pop.style.left = `${Math.max(8, Math.min(r.right - pop.offsetWidth, innerWidth - pop.offsetWidth - 8))}px`;
  pop.querySelector(".pop-item.on")?.scrollIntoView({ block: "nearest" });
  const close = () => { pop.remove(); removeEventListener("mousedown", out, true); removeEventListener("keydown", esc, true); };
  const out = (e) => { if (!pop.contains(e.target) && !anchor.contains(e.target)) close(); };
  const esc = (e) => { if (e.key === "Escape") close(); };
  addEventListener("mousedown", out, true); addEventListener("keydown", esc, true);
}

// Restart: a fresh process re-reads login, model and MCP setup; replies in flight end.
async function restartAgent(name) {
  try {
    await invoke("restart_agent", { name });
    for (const [t, set] of Object.entries(S.typing)) if (set.delete(name)) setStatus(t, name, null);
    for (const k of Object.keys(S.live)) if (k.endsWith(`|${name}`)) { S.live[k].on = false; S.live[k].b.classList.remove("streaming"); if (!S.live[k].text) S.live[k].el.remove(); delete S.live[k]; }
    if (S.thread) syncComposer();
    drawKill(); toast(`${name} 正在重启`, 2000);
    setTimeout(load, 1500);
  } catch (e) { toast(e); }
}
function agentMenu(anchor, name) {
  const local = agentOf(name)?.kind !== "a2a";
  popover(anchor, [
    { label: "更换头像…", icon: "image", run: () => avatarDialog(name) },
    ...(local ? [{ label: "共享给朋友…", icon: "share-network", run: () => shareDialog(name) }] : []),
    { label: "重命名", icon: "pencil-simple", run: async () => {
      if (S.thread !== `dm:${name}`) { setTab("chats"); await openThread(`dm:${name}`); }
      const h2 = $(".head h2"); if (h2) inlineRename(h2, name);
    } },
    { label: "重启", icon: "arrow-counter-clockwise", run: () => restartAgent(name) },
    S.ov.paused ? { label: "恢复所有 agent", icon: "play", run: resumeAll }
      : { label: "全部停止…", icon: "power", arm: "再点一次：停止所有 agent", run: stopAll },
    { label: "删除 Agent…", icon: "trash", danger: true, run: () => removeAgent(name) },
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
  const key = (e) => { if (composing(e)) return; if (e.key === "Enter") { e.preventDefault(); done(true); } else if (e.key === "Escape") { e.preventDefault(); done(false); } };
  const blur = () => done(true);
  el.addEventListener("keydown", key); el.addEventListener("blur", blur);
}
// Consecutive messages from the same agent or person show the name and avatar once.
function runify(el) {
  if (!el?.matches?.(".m:not(.me):not(.sys)")) return;
  const p = el.previousElementSibling;
  el.classList.toggle("cont", !!p?.matches(".m:not(.me):not(.sys)") && p.dataset.author === el.dataset.author);
}
function quote(body) {
  const ta = $("#input"); if (!ta) return;
  const q = body.trim().split("\n").filter((l) => l.trim()).slice(0, 6).map((l) => `> ${l}`).join("\n");
  ta.value = `${q}\n\n${ta.value}`; ta.focus(); ta.setSelectionRange(ta.value.length, ta.value.length);
  ta.dispatchEvent(new Event("input"));
}
const ERRORISH = /出错|失败|无法|错误|未能|找不到|需要|注意|拒绝|超时|error|fail|not found|denied|attention|timeout|unable|cannot/i;
// System lines: a centred notice. Problems get the warning icon and, where there is one, an action.
function notice(body) {
  const clean = body.replace(/^[\s\p{Extended_Pictographic}\uFE0F]+/gmu, "");
  const bad = ERRORISH.test(clean);
  const tx = h("span", { class: "tx" });
  tx.innerHTML = esc(clean).replace(/`([^`]+)`/g, '<code class="cp" title="点击复制">$1</code>').replace(/\n/g, "<br>");
  tx.querySelectorAll("code.cp").forEach((c) => { c.onclick = () => copyText(c.textContent, null).then(() => toast("已复制", 1200)); });
  const cmd = tx.querySelector("code.cp")?.textContent;
  const act = cmd ? h("button", { type: "button", class: "act", onclick: () => copyText(cmd, null).then(() => toast(`已复制：${cmd}`, 2000)) }, "复制命令")
    : bad && /出错|失败|error|fail|timeout|超时/i.test(clean) ? h("button", { type: "button", class: "act", onclick: () => {
      const last = [...document.querySelectorAll("#log .m.me")].pop()?.dataset.body;
      last ? resend(last) : toast("没有可以重发的消息");
    } }, "重试") : null;
  return h("div", { class: `m sys${bad ? " warn" : ""}` }, bad ? ic("warning", "wi") : null, tx, act);
}
function bubble(author, body, ts) {
  if (author === "system") return notice(body);
  const me = author === "me";
  const b = h("div", { class: "b" }); b.innerHTML = md(body);
  // File hand-offs end with the file's path: make it open in Finder.
  if (body.includes("📎")) {
    b.innerHTML = b.innerHTML.replace(/📎\s?/g, icHTML("paperclip", "clipi")).replace(/(^|<br>|<p>)(\/[^<]+)/g, '$1<a class="path" href="#">$2</a>');
    b.querySelectorAll("a.path").forEach((a) => { a.onclick = (e) => { e.preventDefault(); invoke("reveal_path", { path: a.textContent }).catch(toast); }; });
  }
  const tools = h("div", { class: "tools" },
    h("button", { class: "ib", type: "button", title: "复制", onclick: () => copyText(body, null).then(() => toast("已复制", 1200)) }, ic("copy")),
    me ? null : h("button", { class: "ib", type: "button", title: "引用", onclick: () => quote(body) }, ic("arrow-bend-up-left")));
  if (me) { const el = h("div", { class: "m me" }, b, tools); el.dataset.body = body; return el; }
  const human = author.endsWith(" (Discord)"), who = human ? author.slice(0, -10) : author;
  const el = h("div", { class: `m${human ? " person" : ""}` }, avatar(who, { small: true, person: human }),
    h("div", { class: "col" }, h("div", { class: "who" }, who, ts ? h("time", {}, when(ts)) : null), b), tools);
  el.dataset.author = author;
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
    const m = ta.value.slice(0, ta.selectionStart).match(/(^|\s)@([\p{L}\p{N}_-]*)$/u);
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
    if (!box || composing(e)) return false;
    if (e.key === "ArrowDown" || e.key === "ArrowUp") { idx = (idx + (e.key === "ArrowDown" ? 1 : items.length - 1)) % items.length; draw(); }
    else if (e.key === "Enter" || e.key === "Tab") pick(items[idx]);
    else if (e.key === "Escape") close();
    else return false;
    e.preventDefault(); return true;
  } };
}
// Copy buttons on code blocks.
document.addEventListener("click", async (e) => {
  const btn = e.target.closest?.("pre > .copy"); if (!btn) return;
  try { await navigator.clipboard.writeText(btn.nextElementSibling.textContent); btn.innerHTML = icHTML("check:bold"); btn.classList.add("ok"); }
  catch { toast("复制失败，请手动选中复制"); }
  setTimeout(() => { btn.innerHTML = icHTML("copy"); btn.classList.remove("ok"); }, 1200);
});

// Follow new content only when the reader is already near the bottom.
function scrollLog(force) {
  const l = $("#log"); if (!l) return;
  const near = l.scrollHeight - l.scrollTop - l.clientHeight < 140;
  if (force || near) { l.scrollTo({ top: l.scrollHeight, behavior: REDUCED || !force ? "auto" : "smooth" }); $("#jump")?.remove(); return; }
  if (!$("#jump")) {
    const j = h("button", { id: "jump", type: "button", onclick: () => scrollLog(true) }, ic("arrow-down"), "新消息");
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
// A notification card: who, one line of what, 拒绝 (secondary) / 允许 (primary); every other choice sits in ⋯.
function inboxCard(it, inList = false) {
  const perm = it.kind === "permission";
  const place = it.thread.startsWith("dm:") ? "私聊" : threadName(it.thread);
  const card = h("div", { class: `perm${perm ? "" : " ask"}${inList ? " in-list" : ""}`, "data-item": it.id });
  const answer = async (optionId, text) => {
    card.querySelectorAll("button, textarea").forEach((b) => { b.disabled = true; });
    try { await invoke("inbox_answer", { id: it.id, optionId, text }); }
    catch (e) { card.querySelectorAll("button, textarea").forEach((b) => { b.disabled = false; }); toast(e); }
  };
  const allow = it.options.filter((o) => o.allow);
  const primary = perm ? allow.find((o) => !o.always) ?? allow[0] ?? null : null;
  const shown = perm ? (primary ? [primary] : []) : it.options.slice(0, inList ? 2 : 3);
  const rest = it.options.filter((o) => !shown.includes(o));
  const btn = (o, pri) => h("button", { type: "button", class: `btn sm${pri ? " pri" : ""}`, title: o.always ? "以后同类操作不再询问" : null,
    onclick: (e) => { e.stopPropagation(); answer(o.id, null); } }, o.label);
  const more = rest.length ? h("button", { type: "button", class: "ib more", title: "更多选项", onclick: (e) => {
    e.stopPropagation();
    popover(e.currentTarget, rest.map((o) => ({ label: o.always ? `${o.label}（以后不再询问）` : o.label, run: () => answer(o.id, null) })));
  } }, ic("dots-three")) : null;
  card.append(h("div", { class: "ph" }, avatar(it.agent, { small: true }),
    h("div", { class: "pt" },
      h("div", { class: "pl" }, ic(perm ? "lock-simple" : "question", "k-ic"), h("b", {}, it.agent),
        h("span", { class: "k" }, inList ? ` · ${place} · ${perm ? "请求执行" : "问你"}` : perm ? " 请求执行" : " 问你"),
        inList && it.ts ? h("time", {}, when(it.ts)) : null),
      h("div", { class: "q", title: it.title }, it.title))));
  if (it.options.length || perm) card.append(h("div", { class: "opts" }, more,
    perm ? h("button", { type: "button", class: "btn sm", onclick: (e) => { e.stopPropagation(); answer(null, null); } }, "拒绝") : null,
    ...shown.map((o) => btn(o, perm))));
  if (it.text) {
    const ta = h("textarea", { rows: 1, placeholder: "输入回答，↩ 发送" });
    ta.onkeydown = (e) => { if (e.key === "Enter" && !e.shiftKey && !composing(e) && ta.value.trim()) { e.preventDefault(); answer(null, ta.value); } };
    card.append(h("div", { class: "free" }, ta, h("button", { type: "button", class: "btn sm pri", onclick: (e) => { e.stopPropagation(); if (ta.value.trim()) answer(null, ta.value); } }, "回答")));
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

let inboxLive = false; // a pushed inbox beats the startup fetch, which may be older
listen("inbox", onReady(({ payload }) => { inboxLive = true; setInbox(payload); }));

// ---------- events
const liveKey = (thread, agent) => `${thread}|${agent}`;
// Added / removed / renamed / restarted (payload may be null): refresh the overview; a chat whose agent is gone closes.
listen("agents", onReady(async () => {
  await load();
  if (S.thread?.startsWith("dm:") && !S.ov.agents.some((a) => `dm:${a.name}` === S.thread)) blank();
}));
listen("message", onReady(({ payload: m }) => {
  if (m.thread.startsWith("dm:") && !S.ov.agents.some((a) => `dm:${a.name}` === m.thread)) load();
  const k = liveKey(m.thread, m.author);
  if (S.live[k]) { S.live[k].el.remove(); delete S.live[k]; }
  // A failed reply ends in a system line: bubbles already off in this thread are stale.
  if (m.author === "system") for (const [lk, v] of Object.entries(S.live)) if (lk.startsWith(m.thread + "|") && !v.on) { v.el.remove(); delete S.live[lk]; }
  const head = S.ov.heads.find((x) => x.thread === m.thread);
  if (head) Object.assign(head, m); else S.ov.heads.unshift(m);
  if (S.thread !== m.thread && m.author !== "me") S.unread[m.thread] = (S.unread[m.thread] ?? 0) + 1;
  if (Array.isArray(S.msgs[m.thread])) S.msgs[m.thread].push({ author: m.author, body: m.body, ts: m.ts });
  if (S.thread === m.thread) {
    const pending = m.author === "me" && [...document.querySelectorAll("#log .m.pending")].find((e) => e.dataset.body === m.body.trim());
    if (pending) pending.classList.remove("pending");
    else { const el = bubble(m.author, m.body, m.ts); el.classList.add("new"); $("#log").append(el); runify(el); scrollLog(m.author === "me"); }
    if (m.author === "me") markResend($("#log"));
  }
  if (m.thread.startsWith("group:") && m.author.endsWith(" (Discord)")) {
    const g = S.ov.groups.find((g) => `group:${g.id}` === m.thread), who = m.author.slice(0, -10);
    if (g && !g.people.includes(who)) g.people.push(who);
  }
  renderList();
}));
listen("typing", onReady(({ payload: { thread, agent, on } }) => {
  const set = (S.typing[thread] ??= new Set());
  on ? set.add(agent) : set.delete(agent);
  const k = liveKey(thread, agent);
  // A finished reply that never got its final message must not swallow the next one.
  if (on && S.live[k] && !S.live[k].on) { S.live[k].el.remove(); delete S.live[k]; }
  if (on && !S.live[k]) {
    const b = h("div", { class: "b" }, h("span", { class: "typing-dots" }, h("i"), h("i"), h("i")));
    b.classList.add("streaming");
    const el = h("div", { class: "m new" }, avatar(agent, { small: true }), h("div", { class: "col" }, h("div", { class: "who" }, agent), b));
    el.dataset.author = agent;
    S.live[k] = { text: "", b, on: true, el };
    if (S.thread === thread) { $("#log").append(el); runify(el); scrollLog(); }
  }
  if (!on && S.live[k] && !S.live[k].text) { S.live[k].el.remove(); delete S.live[k]; }
  if (!on && S.live[k]) { S.live[k].on = false; S.live[k].b.classList.remove("streaming"); }
  setStatus(thread, agent, on ? "think" : null);
  drawKill();
}));
listen("stream", onReady(({ payload: { thread, agent, event } }) => {
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
      const chip = h("div", { class: "chip new" }, ic("wrench"), h("span", {}, h("b", {}, agent), ` · ${event.title}`));
      if (live) { live.el.before(chip); runify(live.el); } else $("#log").append(chip);
    }
  }
}));

async function installUpdate() {
  $("#update-bar")?.replaceChildren("正在下载更新…");
  try { await invoke("install_update"); } catch (e) { toast(`更新失败：${e}`); }
}
listen("update-available", ({ payload: u }) => {
  if ($("#update-bar")) return;
  $("#items").before(h("div", { id: "update-bar" }, `新版本 ${u.version} 可用 `, h("button", { onclick: installUpdate }, "更新并重启")));
});
listen("discord-status", ({ payload }) => {
  if (!S.ov) return;
  S.ov.discord_status = payload;
  // Patch the status line in place: a full render would wipe half-typed inputs and rerun checks.
  const el = $("#dc-status");
  if (el) { el.className = `status${/^在线/.test(payload) ? " ok" : ""}`; el.replaceChildren(h("i"), payload); }
  else if (S.tab === "settings") renderSettings();
  renderList();
});

// ---------- agents page
const shortPath = (p) => (p || "").replace(/^\/Users\/[^/]+/, "~");
const baseName = (p) => (p ? p.replace(/\/$/, "").split("/").pop() : "");
// Any page that isn't a chat: drop the chat layout, then show it.
function showPage(page) {
  const pane = $("#pane");
  pane.className = ""; pane.replaceChildren(page);
  void pane.offsetWidth; pane.classList.add("enter");
}
// A System-Settings-style group: title (+ an action on the right), one rounded card of rows, a grey footnote.
const gsec = (title, right, rows, foot, id) => h("section", { class: "gsec", id },
  title || right ? h("div", { class: "gsec-h" }, h("span", {}, title), right) : null,
  h("div", { class: "glist" }, ...rows.flat().filter(Boolean)),
  foot ? h("p", { class: "foot" }, foot) : null);
// label left, control right
const grow = (label, ...ctl) => h("div", { class: "grow-row" }, h("span", { class: "gl" }, label), h("div", { class: "gc" }, ...ctl));
async function renderGallery() {
  S.thread = null; renderList();
  const page = h("div", { class: "page add" }, h("h1", {}, "添加 Agent"), h("p", { class: "lead" }, `正在扫描这台${IS_WIN ? "电脑" : " Mac "}…`));
  showPage(page);
  // Show the last scan at once; rescan in the background and redraw if it changed.
  const scan = Promise.all([invoke("detect_agents").catch(() => []), invoke("conda_envs").catch(() => [])]);
  if (!S.found) S.found = await scan;
  else scan.then((r) => { const changed = JSON.stringify(r) !== JSON.stringify(S.found); S.found = r; if (changed && $(".page.add")) renderGallery(); });
  if (!page.isConnected) return;
  const [found, envs] = S.found;
  S.envs = envs;
  const have = found.filter((t) => t.found), miss = found.filter((t) => !t.found);
  const here = IS_WIN ? "这台电脑" : "这台 Mac";
  const hereOn = IS_WIN ? "这台电脑上" : "这台 Mac 上";
  const todo = have.filter((t) => !S.ov.agents.some((a) => a.template === t.id));
  const all = h("button", { type: "button", class: "btn pri sm", disabled: !todo.length, onclick: () => importAll(todo, all) }, todo.length ? "全部导入" : "已全部导入");
  page.replaceChildren(h("h1", {}, "添加 Agent"),
    h("p", { class: "lead" }, have.length ? "导入后直接沿用终端里的登录和模型；点整行可以调目录和环境。" : `没在${here}上找到已安装的 agent，可以手动添加。`),
    have.length ? gsec(`${hereOn}检测到 ${have.length} 个`, all, have.map((t) => agentRow({ ...t, kind: "acp" }, t.found))) : null,
    gsec("更多接入方式", null, [
      agentRow({ id: "openai", name: "OpenAI 兼容模型", kind: "openai" }, "OpenRouter、DeepSeek、Ollama 等，填地址和 Key"),
      agentRow({ id: "command", name: "其他本地 agent", kind: "command" }, "任意支持 ACP 的程序"),
      agentRow({ id: "invite", name: "朋友的邀请码", kind: "invite" }, "接入别人通过 Disco 共享的 agent"),
      agentRow({ id: "a2a", name: "远程 A2A agent", kind: "a2a" }, "通过 URL 接入")]),
    miss.length ? gsec("还没安装", null, miss.map((t) => h("div", { class: "arow" }, h("div", { class: "ahead static" }, brandIcon(t.id),
      h("div", { class: "ameta" }, h("b", {}, t.name), INSTALL[t.id] ? h("code", { class: "path" }, INSTALL[t.id]) : null),
      INSTALL[t.id] ? h("button", { type: "button", class: "tbtn", title: `复制安装命令：${INSTALL[t.id]}`, onclick: () => copy(INSTALL[t.id], `已复制：${INSTALL[t.id]}`) }, "复制安装命令") : null))),
      "装好后会自动出现在上面。") : null);
}
function agentRow(t, sub) {
  const row = h("div", { class: "arow" });
  const acp = t.kind === "acp";
  const head = h("button", { type: "button", class: "ahead", onclick: () => {
    const open = row.classList.toggle("open");
    for (const o of document.querySelectorAll(".arow.open")) if (o !== row) o.classList.remove("open");
    if (open && !row.querySelector(".aform")) row.append(agentForm(t));
    if (open) row.querySelector(".aform input")?.focus();
  } },
    brandIcon(acp ? t.id : t.kind),
    h("div", { class: "ameta" }, h("b", {}, t.name), acp ? h("code", { class: "path", title: sub }, shortPath(sub)) : h("small", {}, sub)),
    acp ? h("span", { class: "aplus tbtn", role: "button", onclick: (e) => { e.stopPropagation(); quickImport(t, e.currentTarget); } }, "导入") : null,
    ic(acp ? "caret-down" : "caret-right", acp ? "chev down" : "chev"));
  row.append(head);
  return row;
}
// Install commands for agents not found yet: the row's button copies one.
const INSTALL = { gemini: "npm i -g @google/gemini-cli", qwen: "npm i -g @qwen-code/qwen-code", goose: "brew install block-goose-cli",
  opencode: "npm i -g opencode-ai", "claude-code": "npm i -g @anthropic-ai/claude-code", codex: "npm i -g @openai/codex", pi: "npm i -g @earendil-works/pi-coding-agent" };
function copy(text, msg) { navigator.clipboard?.writeText(text).then(() => toast(msg, 2000), () => toast(text)); }
// Official logos for known agents (logos.js); a neutral tile with a Phosphor glyph for the other ways in.
const KIND_IC = { openai: "globe", command: "terminal-window", invite: "user-plus", a2a: "link", dsh: "terminal-window" };
function brandIcon(id, small = false) {
  if (LOGOS[id]) return h("span", { class: `brand logo${small ? " sm" : ""}`, html: `<svg viewBox="0 0 24 24">${LOGOS[id]}</svg>` });
  return h("span", { class: `brand${small ? " sm" : ""}` }, ic(KIND_IC[id] ?? "robot"));
}
const freeName = (base) => { let name = base, i = 2; while (S.ov.agents.some((a) => a.name === name)) name = `${base}-${i++}`; return name; };
const quickSpec = (t, name) => ({ template: t.id, name, url: null, headerName: null, headerValue: null, command: null, cwd: null, count: 1, worktree: false, conda: null });
// One click: import a detected agent with default settings and open its chat.
async function quickImport(t, btn) {
  const name = freeName(t.id);
  btn.textContent = "导入中…"; btn.classList.add("busy");
  try {
    await invoke("add_agent", { spec: quickSpec(t, name) });
    await load(); setTab("chats"); openThread(`dm:${name}`); toast(`已导入 ${name}`, 2000);
  } catch (e) { btn.textContent = "导入"; btn.classList.remove("busy"); toast(e); }
}
// 全部导入: every detected agent that has no chat yet, one after another; stays on the page.
async function importAll(list, btn) {
  btn.disabled = true; btn.textContent = "导入中…";
  let n = 0, first = null;
  for (const t of list) {
    const name = freeName(t.id);
    try { await invoke("add_agent", { spec: quickSpec(t, name) }); n++; first ??= name; await load(); } catch (e) { toast(`${t.name}：${e}`, 6000); }
  }
  await load();
  if (n) toast(`已导入 ${n} 个 agent`, 4000, ["去聊天", () => { setTab("chats"); openThread(`dm:${first}`); }]);
  if ($(".page.add")) renderGallery();
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
    h("p", { class: "foot" }, "双方都要登录同一个 Tailscale 网络（或对方把你加为共享设备）。"), h("div", { class: "factions" }, err, go));
}
// Number stepper: − value +, the value stays typeable.
function stepper(value, { min = 0, max = 99, step = 1, onChange }) {
  const inp = h("input", { class: "sv", inputmode: "numeric", value: String(value) });
  const set = (v) => { value = Math.max(min, Math.min(max, Number.isFinite(v) ? Math.round(v) : value)); inp.value = String(value); onChange?.(value); };
  inp.onchange = () => set(Number(inp.value));
  const el = h("div", { class: "stepper" },
    h("button", { type: "button", title: "减少", onclick: () => set(value - step) }, ic("minus")), inp,
    h("button", { type: "button", title: "增加", onclick: () => set(value + step) }, ic("plus")));
  el.value = () => value;
  return el;
}
function agentForm(t) {
  if (t.kind === "invite") return inviteForm();
  const st = { cwd: "", conda: "", count: 1 };
  const err = h("div", { class: "err" });
  const name = h("input", { class: "fld", value: t.kind === "acp" ? t.id : "", placeholder: "名称，如 coder", spellcheck: false });
  let pic = null;
  const ed = avatarEditor(() => name.value.trim() || t.name || t.id, null, (v) => { pic = v; });
  name.addEventListener("input", () => ed.redraw());
  const parts = [h("div", { class: "frow top" }, h("span", {}, "头像"), ed.el), h("div", { class: "frow" }, h("span", {}, "名称"), name)];
  let cmd, url, hn, hv, model;
  if (t.kind === "openai") {
    url = h("input", { class: "fld", placeholder: "Base URL，如 https://openrouter.ai/api/v1", spellcheck: false });
    hv = h("input", { class: "fld", type: "password", placeholder: IS_WIN ? "API Key（本地模型可不填，存凭据管理器）" : "API Key（本地模型可不填，存钥匙串）" });
    model = h("input", { class: "fld", placeholder: "模型，如 deepseek-chat（不填用第一个）", spellcheck: false });
    const PRESETS = [["OpenRouter", "https://openrouter.ai/api/v1", ""], ["DeepSeek", "https://api.deepseek.com/v1", "deepseek-chat"],
      ["OpenAI", "https://api.openai.com/v1", ""], ["Ollama", "http://localhost:11434/v1", ""], ["LM Studio", "http://localhost:1234/v1", ""]];
    const chips = h("div", { class: "chips" }, ...PRESETS.map(([n, u, m]) => h("button", { type: "button", class: "chip2", onclick: (e) => {
      url.value = u; if (m && !model.value) model.value = m; if (!name.value) { name.value = n.toLowerCase().replace(/\s+/g, "-"); ed.redraw(); }
      chips.querySelectorAll(".chip2").forEach((c) => c.classList.toggle("on", c === e.currentTarget)); (u.includes("localhost") ? model : hv).focus();
    } }, n)));
    parts.push(h("div", { class: "frow" }, h("span", {}, "常用"), chips), h("div", { class: "frow" }, h("span", {}, "地址"), url),
      h("div", { class: "frow" }, h("span", {}, "Key"), hv), h("div", { class: "frow" }, h("span", {}, "模型"), model),
      h("p", { class: "foot" }, "Key 只存在这台电脑上。添加后在对话顶部可以随时切换模型；它也能用 Disco 工具记待办、写群文档。"));
  }
  if (t.kind === "command") {
    cmd = h("input", { class: "fld", placeholder: "/path/to/agent --acp", spellcheck: false });
    parts.push(h("div", { class: "frow" }, h("span", {}, "程序"), cmd, h("button", { type: "button", class: "btn sm", onclick: async () => {
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
    const flabel = h("span", {}, "主目录");
    const folder = h("button", { type: "button", class: "pill" }, ic("folder-simple"), flabel);
    folder.onclick = async () => { const p = await invoke("choose_path", {}); if (p) { st.cwd = p; flabel.textContent = baseName(p); folder.title = p; folder.classList.add("set"); } };
    const chips = h("div", { class: "chips" });
    const opts = [{ name: "系统", path: "" }, ...(S.envs || [])];
    const draw = () => chips.replaceChildren(...opts.map((e) => h("button", { type: "button", class: "chip2" + (st.conda === e.path ? " on" : ""), title: e.path, onclick: () => { st.conda = e.path; draw(); } }, e.name)));
    draw();
    parts.push(
      h("div", { class: "frow" }, h("span", {}, "目录"), folder),
      h("div", { class: "frow" }, h("span", {}, "环境"), chips),
      h("div", { class: "frow" }, h("span", {}, "数量"), stepper(1, { min: 1, max: 8, onChange: (v) => { st.count = v; } }), h("small", { class: "foot" }, "多个实例各用独立 git worktree")));
  }
  const go = h("button", { type: "button", class: "btn pri" }, t.kind === "acp" ? "导入" : "添加");
  go.onclick = async () => {
    go.disabled = true; err.textContent = "";
    try {
      const added = await invoke("add_agent", { spec: { template: t.id, name: name.value, url: url?.value ?? null, headerName: hn?.value || null, headerValue: hv?.value || null,
        command: cmd?.value ?? null, cwd: st.cwd || null, count: st.count, worktree: st.count > 1, conda: st.conda || null, model: model?.value || null } });
      if (pic) for (const n of added ?? []) await invoke("set_avatar", { agent: n, data: pic }).catch(() => {});
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
  const guest = h("input", { class: "fld", placeholder: "对方的名字，如 小王", maxlength: 24 });
  const st2 = { mode: "chat", limit: 50 };
  const modes = h("div", { class: "seg" });
  const drawModes = () => modes.replaceChildren(...[["chat", "只聊天", "需要批准的操作一律拒绝"], ["ask", "需要时问我", `由你在这台${IS_WIN ? "电脑" : " Mac "}上批准`]]
    .map(([v, t, sub]) => h("button", { type: "button", class: st2.mode === v ? "on" : "", onclick: () => { st2.mode = v; drawModes(); } }, h("b", {}, t), h("small", {}, sub))));
  drawModes();
  const limit = stepper(50, { min: 0, max: 10000, step: 10, onChange: (v) => { st2.limit = v; } });
  const err = h("div", { class: "err" });
  const out = h("div", { class: "invite", hidden: true });
  const go = h("button", { class: "btn pri", type: "submit" }, "生成邀请码");
  const list = h("div", { class: "glist" }, ...mine.map((s) => shareRow(s, () => shareDialog(agent))));
  form.replaceChildren(...[
    h("div", { class: "del-head" }, avatar(agent), h("div", {}, h("h3", {}, `共享 ${agent}`),
      h("p", { class: "del-sub" }, "通过 Tailscale 私有网络共享，只有拿到邀请码的人能用，随时可以撤销。"))),
    st.problem ? h("div", { class: "warn" }, ic("warning"), h("span", {}, st.problem, " ", h("button", { type: "button", class: "linkish", onclick: () => invoke("open_url", { url: "https://tailscale.com/download" }) }, "下载 Tailscale"))) : null,
    h("div", { class: "glist" },
      grow("共享给", guest),
      // Not a <label>: a click anywhere in a label also clicks its first button, so the
      // second choice could never stay selected.
      h("div", { class: "grow-row col" }, h("span", { class: "gl" }, "权限"), modes),
      grow("每天上限", limit, h("small", { class: "foot" }, "条，0 = 不限"))),
    err, out,
    mine.length ? h("div", { class: "lab" }, "已共享") : null, mine.length ? list : null,
    h("div", { class: "actions" }, h("button", { class: "btn", type: "button", onclick: () => d.close() }, "关闭"), go)].filter(Boolean));
  form.onsubmit = async (e) => {
    e.preventDefault(); err.textContent = ""; go.disabled = true;
    try {
      const code = await invoke("create_share", { agent, guest: guest.value, mode: st2.mode, limit: Number(limit.value()) || 0 });
      const cp = h("button", { type: "button", class: "btn", onclick: () => copyText(code, cp) }, "复制");
      out.hidden = false;
      out.replaceChildren(h("div", { class: "foot" }, "把这串邀请码发给对方，让他在 Disco「添加 Agent → 朋友的邀请码」里粘贴。"),
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
  return h("div", { class: "share-row" }, avatar(s.guest, { person: true, small: true }),
    h("div", { class: "grow" }, h("b", {}, s.guest), h("small", {}, `${s.mode === "ask" ? "需要时问我" : "只聊天"} · 今天 ${s.used_today}${s.limit ? "/" + s.limit : ""} 条`)),
    cp, h("button", { type: "button", class: "btn sm danger", onclick: async () => { await invoke("revoke_share", { id: s.id }); toast(`已撤销 ${s.guest} 的访问`); await load(); after(); } }, "撤销"));
}
// A styled on/off switch around a real checkbox (keyboard and forms keep working).
const toggle = (checked = false) => { const i = h("input", { type: "checkbox", class: "tgl" }); i.checked = checked; return i; };
async function removeAgent(name) {
  const d = $("#dlg"), form = $("#dlg-form");
  const purge = toggle(true);
  const go = h("button", { class: "btn pri danger-fill", type: "button" }, "删除");
  form.replaceChildren(
    h("div", { class: "del-head" }, avatar(name), h("div", {}, h("h3", {}, `删除 ${name}？`),
      h("p", { class: "del-sub" }, "会停止它的进程，把它移出所有群聊，并删除它的 Discord 频道。"))),
    h("div", { class: "glist" }, h("label", { class: "grow-row del-opt" }, h("span", { class: "gl" }, "同时清空和它的聊天记录"), h("div", { class: "gc" }, purge))),
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
  const name = h("input", { class: "fld gname", value: g?.name ?? "", placeholder: "群名", spellcheck: false });
  const list = h("div", { class: "glist pick" });
  const count = h("span", { class: "cnt" });
  const draw = () => {
    if (st.lead && !st.members.includes(st.lead)) st.lead = null;
    const lead = st.lead ?? st.members[0];
    list.replaceChildren(...S.ov.agents.map((a) => {
      const on = st.members.includes(a.name);
      const flip = () => { st.members = on ? st.members.filter((m) => m !== a.name) : [...st.members, a.name]; draw(); };
      return h("div", { class: `ptile${on ? " on" : ""}`, role: "checkbox", tabindex: 0, "aria-checked": String(on), onclick: flip,
        onkeydown: (e) => { if (e.key === " " || e.key === "Enter") { e.preventDefault(); flip(); } } },
        h("span", { class: `chk${on ? " on" : ""}` }, ic("check:bold")), avatar(a.name, { small: true, live: a.live }),
        h("div", { class: "t" }, h("span", {}, a.name), h("small", {}, agentSub(a))),
        on && a.name === lead ? h("em", { class: "crown" }, "组长")
          : on ? h("button", { type: "button", class: "tbtn mklead", onclick: (e) => { e.stopPropagation(); st.lead = a.name; draw(); } }, "设为组长") : null);
    }));
    count.textContent = st.members.length ? `已选 ${st.members.length} 个` : "点选成员";
  };
  draw();
  const where = [];
  const discordOn = /^在线/.test(S.ov.discord_status || "");
  let foot = null;
  if (g) where.push(grow("Discord", h("span", { class: "val" }, g.id.startsWith("local-") ? "仅在 Disco 内" : "已绑定 Discord 频道")));
  else if (discordOn) {
    const slot = h("span");
    const drawCh = (cs) => slot.replaceChildren(choice([["", "自动新建同名频道"], ...cs.map((c) => [`${c.guild_id}|${c.id}`, `用已有 #${c.name} · ${c.guild}`])], st.channel, (v) => { st.channel = v; }));
    drawCh([]);
    where.push(grow("Discord 频道", slot));
    invoke("discord_channels").then(drawCh).catch(() => {});
    foot = "想把 agent 带进朋友的群？先在 设置 → Discord 复制邀请链接发给朋友，他把你的 Bot 加进他的服务器后，这里就能选他的频道。频道里的每个人都能 @ 你的 agent，需要批准的操作只会问你。";
  } else where.push(grow("Discord", h("span", { class: "val" }, "未连接，群聊先只在 Disco 内进行")));
  const err = h("div", { class: "err" });
  const go = h("button", { type: "button", class: "btn pri" }, g ? "保存" : "创建群聊");
  go.onclick = async () => {
    go.disabled = true; err.textContent = "";
    try {
      const [guildId, channelId] = st.channel ? st.channel.split("|") : [null, null];
      const id = await invoke("save_group", { channelId, guildId, name: name.value, members: st.members, lead: st.lead ?? st.members[0] ?? null });
      await load(); setTab("groups"); openThread(`group:${id}`);
    } catch (e) { err.textContent = String(e); go.disabled = false; }
  };
  const del = g ? h("button", { type: "button", class: "btn danger", onclick: async () => {
    if (await ask("删除这个群聊？聊天记录保留，Discord 频道不受影响。", "删除")) {
      await invoke("delete_group", { id: g.id }); blank(); await load();
      toast(`已删除群聊 ${g.name}`, 6000, ["撤销", async () => { await invoke("restore_group", { id: g.id, guildId: g.guild_id, name: g.name, members: g.members, lead: g.lead }); await load(); setTab("groups"); openThread(`group:${g.id}`); }]);
    }
  } }, "删除群聊") : null;
  showPage(h("div", { class: "page gpage" },
    h("h1", {}, g ? "群设置" : "新建群聊"),
    gsec(null, null, [grow("群名", name)]),
    h("section", { class: "gsec" }, h("div", { class: "gsec-h" }, h("span", {}, "成员"), count), list),
    gsec(null, null, where, foot),
    h("div", { class: "factions" }, del, err, go)));
  if (!g) name.focus();
}

// ---------- settings: grouped inset lists — Discord, 共享与 Tailscale, 记忆, 运行, 通用与更新
async function loadMemories(box) {
  const ms = await invoke("memories").catch(() => []);
  box.replaceChildren(...(ms.length ? ms.map((m) => h("div", { class: "grow-row mem" },
    h("div", { class: "t" }, h("span", {}, m.text),
      h("small", {}, [m.scope === "all" ? "所有 agent" : m.scope.startsWith("group-") ? (S.ov.groups.find((g) => `group-${g.id}` === m.scope)?.name ?? "群") : m.scope, m.topic].filter(Boolean).join(" · "))),
    h("button", { class: "ib", type: "button", title: "删除这条记忆", onclick: async () => {
      await invoke("forget", { key: m.key }); loadMemories(box);
      toast("已删除这条记忆", 5000, ["撤销", async () => { await invoke("memory_restore", { key: m.key }).catch(toast); loadMemories(box); }]);
    } }, ic("trash")))) : [h("div", { class: "grow-row" }, h("span", { class: "gl mute" }, "还没有记忆"))]));
}
function gotoSetting(id) {
  S.setSec = id;
  if (!$("#pane .page.settings")) renderSettings();
  document.getElementById(`set-${id}`)?.scrollIntoView({ block: "start", behavior: REDUCED ? "auto" : "smooth" });
  renderList();
}
// Times on a half-hour grid instead of the native time input ("" = not set).
function timeChoice(value, onPick) {
  const opts = [["", "不设置"]];
  for (let m = 0; m < 1440; m += 30) { const s = `${String((m / 60) | 0).padStart(2, "0")}:${String(m % 60).padStart(2, "0")}`; opts.push([s, s]); }
  if (value && !opts.some(([v]) => v === value)) opts.push([value, value]);
  return choice(opts, value ?? "", onPick, "不设置");
}
function renderSettings() {
  S.thread = null;
  const ok = /^在线/.test(S.ov.discord_status);
  const discordBox = h("section", { class: "gsec" });
  if (S.ov.discord_token) loadDiscordSettings(discordBox);
  const token = h("form", { class: "inline", onsubmit: async (e) => {
    e.preventDefault(); const tok = e.target.token.value; const b = e.target.querySelector("button");
    b.disabled = true; b.textContent = "验证中…";
    try { await invoke("save_discord_token", { token: tok }); e.target.reset(); toast("token 有效，已保存"); await load(); renderSettings(); }
    catch (er) { toast(er, 8000); b.disabled = false; b.textContent = "保存"; }
  } }, h("input", { name: "token", type: "password", required: true, class: "fld", placeholder: S.ov.discord_token ? "已保存 · 输入新 token 替换" : "粘贴 Bot token" }),
    h("button", { class: "btn pri sm" }, "保存"));
  const netBox = h("section", { class: "gsec", id: "set-net" }, h("div", { class: "gsec-h" }, h("span", {}, "共享与 Tailscale")), h("div", { class: "glist" }, h("div", { class: "grow-row" }, h("span", { class: "gl mute" }, "正在读取…"))));
  loadNet(netBox);
  const mems = h("div", { class: "glist" }); loadMemories(mems);
  const upLine = h("span", { class: "val" }, "正在检查…");
  const upBtn = h("button", { class: "btn pri sm", type: "button", hidden: true, onclick: installUpdate }, "更新并重启");
  invoke("check_update").then((u) => {
    upLine.textContent = u.version ? `当前 ${u.current}，新版本 ${u.version} 可用` : `当前 ${u.current}，已是最新版本`;
    upBtn.hidden = !u.version;
  }).catch((e) => { upLine.textContent = `检查更新失败：${e}`; });
  const checks = h("div", { class: "glist checks", hidden: true });
  const diag = h("button", { class: "btn sm", type: "button", onclick: async () => {
    diag.disabled = true; diag.textContent = "检查中…（约 15 秒）"; checks.replaceChildren(); checks.hidden = true;
    try {
      for (const c of await invoke("diagnose")) checks.append(h("div", { class: `grow-row ${c.ok ? "ok" : "bad"}` }, ic(c.ok ? "check-circle:fill" : "x-circle:fill", "ci"), h("b", {}, c.name), h("span", { class: "val" }, c.detail)));
    } catch (e) { checks.append(h("div", { class: "grow-row bad" }, ic("x-circle:fill", "ci"), h("span", { class: "val" }, String(e)))); }
    checks.hidden = !checks.children.length;
    diag.disabled = false; diag.textContent = "重新检查";
  } }, "一键诊断");
  const keys = IS_WIN ? "密钥只存在 Windows 凭据管理器里。" : "密钥只存在 macOS 钥匙串里。";
  showPage(h("div", { class: "page settings" },
    h("h1", {}, "设置"),
    gsec("Discord", null, [
      grow("状态", h("span", { id: "dc-status", class: `status${ok ? " ok" : ""}` }, h("i"), S.ov.discord_status)),
      grow("Bot token", token),
      S.ov.discord_token ? grow("连接", h("button", { class: "btn sm", type: "button", onclick: async () => { try { await invoke("discord_connect"); } catch (e) { toast(e); } } }, "重新连接")) : null,
    ], keys, "set-dc"),
    discordBox,
    netBox,
    gsec("记忆", null, [mems], `在私聊里说「记住 …」存给这个 agent，在群聊里说存给本群；agent 也能用 remember 工具按主题记下东西。记忆是普通的 Markdown 文件，每次新会话自动带上，只发给这台${IS_WIN ? "电脑" : " Mac "}上的 agent。每个对话顶部的记忆按钮可以单独关掉。`, "set-mem"),
    gsec("运行", null, [
      grow("状态", h("span", { id: "run-state", class: "status" }, h("i"), runLabel())),
      grow("全部停止", h("button", { id: "kill", type: "button", class: "btn sm", onclick: killClick }, "全部停止")),
    ], "立刻结束所有本地 agent 进程并暂停所有路由，要点两下才会执行；所有操作写入审计日志。", "set-run"),
    gsec("通用与更新", null, [grow("版本", upLine, upBtn), grow("诊断", h("span", { class: "val" }, "代理、钥匙串、Discord、Node.js 和每个 agent 的启动命令"), diag)], null, "set-gen"),
    checks));
  drawKill();
  if (S.setSec) document.getElementById(`set-${S.setSec}`)?.scrollIntoView({ block: "start" });
}

async function loadNet(sec) {
  let p;
  const head = h("div", { class: "gsec-h" }, h("span", {}, "共享与 Tailscale"));
  try { p = await invoke("net_policy"); } catch (e) { sec.replaceChildren(head, h("div", { class: "glist" }, h("div", { class: "grow-row" }, h("span", { class: "err" }, String(e))))); return; }
  const lines = (v) => v.split(/[\s,，]+/).filter(Boolean);
  const area = (key, ph) => { const t = h("textarea", { class: "fld rules", rows: 2, placeholder: ph, spellcheck: false }); t.value = (p[key] ?? []).join("\n"); t.oninput = () => { p[key] = lines(t.value); }; return t; };
  const allowRow = h("div", { class: "grow-row col", hidden: p.mode !== "allowlist" }, h("span", { class: "gl" }, "允许名单"), area("allow", "每行一个：设备名、tag:名称 或 user:登录名"));
  const mode = h("div", { class: "seg" });
  const drawMode = () => mode.replaceChildren(...[["tailnet", "整个 Tailscale 网络", "有邀请码的设备都能用"], ["allowlist", "仅允许名单", "还要在名单里"]]
    .map(([v, t, sub]) => h("button", { type: "button", class: p.mode === v ? "on" : "", onclick: () => { p.mode = v; allowRow.hidden = v !== "allowlist"; drawMode(); } }, h("b", {}, t), h("small", {}, sub))));
  drawMode();
  const qs = timeChoice(p.quiet_start, (v) => { p.quiet_start = v; });
  const qe = timeChoice(p.quiet_end, (v) => { p.quiet_end = v; });
  const save = h("button", { class: "btn pri sm", type: "button" }, "保存规则");
  save.onclick = async () => { save.disabled = true; try { Object.assign(p, await invoke("net_policy_set", { policy: p })); toast("已保存", 1500); } catch (e) { toast(e, 8000); } save.disabled = false; };
  sec.replaceChildren(head, h("div", { class: "glist" },
    h("div", { class: "grow-row col" }, h("span", { class: "gl" }, "谁能用"), mode), allowRow,
    h("div", { class: "grow-row col" }, h("span", { class: "gl" }, "拒绝"), area("deny", "这些设备一律拒绝（优先于其他规则）")),
    grow("免打扰", qs, h("small", { class: "foot" }, "到"), qe),
    grow("", save)),
    h("p", { class: "foot" }, "朋友通过邀请码使用你共享的 agent 时，Disco 会向 Tailscale 确认对方是哪台设备，再按这里的规则放行；免打扰时段不接共享消息。"));
}
async function loadDiscordSettings(box) {
  let d;
  try { d = await invoke("discord_settings", {}); } catch (e) { box.replaceChildren(h("p", { class: "err" }, String(e))); return; }
  const guild = choice(d.guilds.map((g) => [g.id, g.guild]), d.home_guild, async (v) => {
    try { await invoke("discord_settings", { homeGuild: v }); toast("已切换主服务器，正在为每个 agent 建频道"); } catch (e) { toast(e); }
  }, d.guilds.length ? "选择服务器" : "Bot 还没加入任何服务器");
  const allow = h("input", { class: "fld", value: d.allow, placeholder: "Discord 用户 ID，逗号分隔" });
  if (!d.guilds.length) {
    // Not invited yet (or Discord unreachable): one clear next step, then watch for the join.
    const copyBtn = d.invite_url ? h("button", { class: "btn sm", type: "button", onclick: (e) => copyText(d.invite_url, e.currentTarget) }, "复制邀请链接") : null;
    box.replaceChildren(h("div", { class: "warn" }, ic("warning"),
      h("div", {}, d.problem ? h("p", {}, d.problem) : null,
        h("p", {}, d.invite_url ? "Bot 还没加入任何服务器。点下面的按钮，在浏览器里选你的服务器并授权；浏览器没打开的话，复制链接自己打开。" : "暂时连不上 Discord，稍后会自动重试。"),
        d.invite_url ? h("div", { class: "btns" },
          h("button", { class: "btn pri sm", type: "button", onclick: () => invoke("open_url", { url: d.invite_url }).catch(toast) }, "邀请 Bot 进服务器"), copyBtn) : null)));
    setTimeout(() => { if (box.isConnected) loadDiscordSettings(box); }, 4000);
    return;
  }
  const sync = h("button", { class: "btn sm", type: "button", onclick: async (e) => {
    e.currentTarget.disabled = true; const b = e.currentTarget;
    try { const n = await invoke("discord_sync"); toast(`已同步 ${n} 个 agent 频道`); } catch (er) { toast(er, 8000); }
    b.disabled = false;
  } }, "同步频道");
  box.replaceChildren(h("div", { class: "glist" },
    grow("主服务器", guild, sync),
    d.invite_url ? grow("Bot 授权", h("button", { class: "btn sm", type: "button", onclick: () => invoke("open_url", { url: d.invite_url }).catch(toast) }, "重新授权 Bot"),
      h("button", { class: "btn sm", type: "button", title: "发给朋友，他把你的 Bot 加进他的服务器后，你就能把 agent 拉进他的群频道", onclick: (e) => copyText(d.invite_url, e.currentTarget) }, "复制邀请链接（给朋友）")) : null,
    grow("可指挥的人", allow, h("button", { class: "btn sm", type: "button", onclick: async () => {
      try { const r = await invoke("discord_settings", { allow: allow.value }); allow.value = r.allow; toast("已保存"); } catch (e) { toast(e); }
    } }, "保存"))),
    h("p", { class: "foot" }, "每个 agent 在主服务器的「Disco Agents」分类下有自己的频道。群频道里每个能发言的人都能 @ agent；agent 自己的频道只有你和这里填的用户能用。"));
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
  showPage(box);
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
// Rail: icon-only tabs (outline; filled in the accent colour when selected), tooltips carry the names.
for (const b of document.querySelectorAll("#rail [data-ic], #list header [data-ic]")) b.prepend(ic(b.dataset.ic, "o"), ...(b.dataset.tab ? [ic(`${b.dataset.ic}:fill`, "f")] : []));
function markTabs() { document.querySelectorAll(".tab[data-tab]").forEach((b) => { b.classList.toggle("on", b.dataset.tab === S.tab); b.setAttribute("aria-current", b.dataset.tab === S.tab ? "page" : "false"); }); }
function setTab(tab) {
  S.tab = tab;
  if (tab !== "settings" && document.body.classList.contains("collapsed")) setCollapsed(false);
  markTabs();
  if (tab === "agents") renderGallery();
  else if (tab === "settings") renderSettings();
  else if (!S.thread) blank();
  renderList();
}
document.querySelectorAll(".tab[data-tab]").forEach((b) => b.addEventListener("click", () => setTab(b.dataset.tab)));
$("#list-add").onclick = () => (S.tab === "agents" ? renderGallery() : groupDialog(null));
$("#search-kbd").textContent = `${MOD}K`;
$("#search").oninput = (e) => { S.q = e.target.value; renderList(); };
// Emergency stop: Settings → 运行 and the agent ⋯ menus. It needs two clicks, so it never fires by accident.
let killArm = 0;
function drawKill() {
  const k = $("#kill"), st = $("#run-state");
  const armed = !!killArm, paused = !!S.ov?.paused, n = busyAgents().size;
  if (k) {
    k.className = `btn sm${paused ? " pri" : armed ? " danger-fill" : ""}`;
    k.textContent = paused ? "恢复" : armed ? "再点一次停止" : "全部停止";
    k.title = paused ? "已全部停止，点一下恢复" : `${n ? `${n} 个 agent 正在回复。` : ""}点两下：全部停止所有 agent`;
  }
  if (st) { st.className = `status${paused ? " bad" : " ok"}`; st.replaceChildren(h("i"), runLabel()); }
  if (S.tab === "settings") renderList();
}
async function stopAll() {
  const n = await invoke("stop_all"); await load(); drawKill();
  toast(`已停止 ${n} 个 agent，新消息暂不转发`, 6000, ["恢复", resumeAll]);
}
async function resumeAll() { await invoke("resume"); await load(); drawKill(); toast("已恢复", 1500); }
async function killClick() {
  if (S.ov.paused) return resumeAll();
  if (!killArm) { killArm = setTimeout(() => { killArm = 0; drawKill(); }, 2500); drawKill(); return; }
  clearTimeout(killArm); killArm = 0;
  await stopAll();
}
$("#resume").onclick = async () => { await invoke("resume"); await load(); drawKill(); };
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
const IS_MAC = /Mac|iPhone|iPad/.test(navigator.platform || navigator.userAgent);
addEventListener("keydown", (e) => {
  if ($("#dlg").open) return; // the modal owns the keyboard
  // AltGr arrives as Ctrl+Alt on Windows (\ on German/French layouts); on macOS Ctrl is Emacs editing.
  const altGr = e.altKey || e.getModifierState?.("AltGraph");
  const mod = !altGr && (IS_MAC ? e.metaKey : e.metaKey || e.ctrlKey);
  const key = e.key?.toLowerCase() ?? "";
  if (mod && key === "k") { e.preventDefault(); $("#search").focus(); $("#search").select(); return; }
  if (mod && /^[1-4]$/.test(e.key)) { e.preventDefault(); setTab(["chats", "groups", "agents", "inbox"][e.key - 1]); return; }
  if (mod && e.key === "\\") { e.preventDefault(); setCollapsed(!document.body.classList.contains("collapsed")); return; }
  if (mod && e.key === ",") { e.preventDefault(); setTab("settings"); return; }
  if (mod && key === "n") { e.preventDefault(); if (S.tab === "agents") renderGallery(); else groupDialog(null); return; }
  // Type anywhere → composer, unless a control has focus (Space presses buttons) or someone handled the key.
  const busy = document.activeElement?.closest?.("button,input,select,textarea,[role],[tabindex],[contenteditable]");
  if (!e.metaKey && !e.ctrlKey && !altGr && !e.defaultPrevented && !busy && e.key?.length === 1 && e.key !== " " && $("#input")) $("#input").focus();
});
// Text keeps one size: no page zoom from Ctrl/⌘ + wheel or + / − / 0.
addEventListener("wheel", (e) => { if (e.ctrlKey || e.metaKey) e.preventDefault(); }, { passive: false });
addEventListener("keydown", (e) => { if ((e.ctrlKey || e.metaKey) && ["+", "=", "-", "_", "0"].includes(e.key)) e.preventDefault(); }, true);
$("#search").addEventListener("keydown", (e) => {
  if (composing(e)) return;
  if (e.key === "Escape") { e.target.value = ""; S.q = ""; renderList(); e.target.blur(); }
  if (e.key === "Enter") { e.preventDefault(); $("#items li:not(.empty)")?.click(); e.target.blur(); }
});

ready = load();
(async () => { await ready; blank(); const items = await invoke("inbox").catch(() => []); if (!inboxLive) setInbox(items, true); })();
