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
function colorOf(name) {
  const a = S.ov?.agents.find((a) => a.name === name);
  const t = a && S.tpl.find((t) => t.id === a.template);
  return t && t.id !== "command" && t.id !== "a2a" ? t.color : COLORS[hash(name) % COLORS.length];
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
      h("button", { class: "more", title: "移除", onclick: (e) => { e.stopPropagation(); removeAgent(a.name); } }, "移除")));
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
      isGroup ? h("button", { class: "btn", onclick: () => groupDialog(g) }, "成员") : null),
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
  const msgs = await invoke("messages", { thread: t });
  if (S.thread !== t) return;
  for (const m of msgs) $("#log").append(bubble(m.author, m.body));
  for (const p of Object.values(S.perms)) if (p.thread === t) $("#log").append(permCard(p));
  for (const [k, v] of Object.entries(S.live)) if (k.startsWith(t + "|")) $("#log").append(v.el);
  scrollLog();
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
  delete S.perms[p.request_id];
  $(`#perm-${p.request_id}`)?.remove();
  await invoke("decide_permission", { agent: p.agent, requestId: p.request_id, optionId });
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
    live.text += event.text; live.b.innerHTML = md(live.text);
    if (S.thread === thread) scrollLog();
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
function renderGallery() {
  S.thread = null; renderList();
  $("#pane").replaceChildren(h("div", { class: "page" },
    h("h1", {}, "添加 Agent"),
    h("p", {}, "本地 agent 通过 ACP 运行，沿用你在这台 Mac 上已登录的账户；远程 agent 通过 A2A 接入。"),
    h("div", { class: "grid" }, ...S.tpl.map((t) => h("button", { class: "card", onclick: () => agentDialog(t) },
      Object.assign(h("div", { class: "av" }, t.kind === "a2a" ? "A" : t.kind === "command" ? ">" : t.name[0].toUpperCase()), { style: `background:${t.color}` }),
      h("div", {}, h("b", {}, t.name), h("small", {}, t.blurb)))))));
}
function agentDialog(t) {
  const fields = [h("h3", {}, `添加 ${t.name}`),
    h("label", {}, "名称", h("input", { name: "name", required: true, value: t.kind === "acp" ? t.id : "", placeholder: "如 coder" }))];
  if (t.kind === "a2a") fields.push(
    h("label", {}, "Agent URL", h("input", { name: "url", required: true, placeholder: "https://agent.example.com" })),
    h("label", {}, "鉴权头（可选）", h("input", { name: "header_name", placeholder: "authorization" })),
    h("label", {}, "鉴权值（可选，存入钥匙串）", h("input", { name: "header_value", type: "password", placeholder: "Bearer …" })));
  if (t.kind === "command") fields.push(h("label", {}, "启动命令", h("input", { name: "command", required: true, placeholder: "my-agent --acp" })));
  if (t.kind !== "a2a") {
    const wt = h("input", { type: "checkbox", name: "worktree" });
    const count = h("input", { name: "count", type: "number", min: 1, max: 8, value: 1, oninput: () => { wt.checked = Number(count.value) > 1; } });
    fields.push(
      h("label", {}, "工作目录（可选，默认主目录）", h("input", { name: "cwd", placeholder: "~/code/project" })),
      h("label", {}, "实例数量（同时拉多个，命名为 名称、名称-2 …）", count),
      h("div", { class: "checks" }, h("label", {}, wt, "每个实例使用独立 git worktree（多实例改同一仓库时推荐）")));
  }
  dialog(fields, "添加", async (f) => {
    await invoke("add_agent", { spec: { template: t.id, name: f.name, url: f.url ?? null, headerName: f.header_name || null, headerValue: f.header_value || null, command: f.command ?? null, cwd: f.cwd || null, count: Number(f.count || 1), worktree: f.worktree === "on" } });
    await load(); setTab("chats"); openThread(`dm:${f.name.trim()}`);
  });
}
async function removeAgent(name) {
  if (!(await ask(`移除 ${name}？聊天记录会保留，它的 Discord 频道会被删除。`, "移除"))) return;
  try { await invoke("remove_agent", { name }); if (S.thread === `dm:${name}`) blank(); await load(); } catch (e) { toast(e); }
}

// ---------- groups
async function groupDialog(g) {
  if (!S.ov.agents.length) return toast("先添加至少一个 agent");
  let channels = [];
  try { channels = await invoke("discord_channels"); } catch (e) { return toast(`${e}：需要先连接 Discord`, 8000, ["去设置", () => setTab("settings")]); }
  const sel = h("select", { name: "channel", required: true, disabled: !!g },
    ...channels.map((c) => h("option", { value: `${c.guild_id}|${c.id}`, selected: g?.id === c.id }, `${c.guild} / #${c.name}`)));
  const members = g?.members ?? [];
  dialog([
    h("h3", {}, g ? `群聊「${g.name}」` : "新建群聊"),
    h("label", {}, "绑定的 Discord 频道", sel),
    h("label", {}, "群名", h("input", { name: "name", required: true, value: g?.name ?? "" })),
    h("label", {}, "成员", h("div", { class: "checks" }, ...S.ov.agents.map((a) => h("label", {}, h("input", { type: "checkbox", name: "m", value: a.name, checked: members.includes(a.name) }), a.name)))),
    h("label", {}, "组长（没人被 @ 时回答）", h("select", { name: "lead" }, h("option", { value: "" }, "第一个成员"), ...S.ov.agents.map((a) => h("option", { value: a.name, selected: g?.lead === a.name }, a.name)))),
  ], g ? "保存" : "创建", async (f, form) => {
    const [guildId, channelId] = (g ? `${g.guild_id}|${g.id}` : f.channel).split("|");
    const picked = [...form.querySelectorAll("input[name=m]:checked")].map((i) => i.value);
    await invoke("save_group", { channelId, guildId, name: f.name.trim(), members: picked, lead: f.lead || null });
    await load(); openThread(`group:${channelId}`);
  }, g ? ["删除群聊", async () => { if (await ask("删除这个群聊？Discord 频道不受影响。", "删除")) { await invoke("delete_group", { id: g.id }); blank(); await load(); } }] : null);
}

// ---------- settings
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

(async () => { S.tpl = await invoke("templates"); await load(); blank(); })();
