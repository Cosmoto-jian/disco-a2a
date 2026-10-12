const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const $ = (s) => document.querySelector(s);
const contexts = {};          // agent -> context id (one thread per agent for now)
const live = {};              // agent -> bubble currently streaming
let current = null;

const el = (tag, cls, text) => { const e = document.createElement(tag); if (cls) e.className = cls; if (text) e.textContent = text; return e; };
const scroll = () => { const l = $("#log"); l.scrollTop = l.scrollHeight; };
const logs = {};              // agent -> DocumentFragment-like array of nodes

function push(agent, node) {
  (logs[agent] ??= []).push(node);
  if (agent === current) { $("#log").append(node); scroll(); }
}

async function refresh() {
  const s = await invoke("app_status");
  $("#status").textContent = `schema v${s.schema} · vec ${s.vec_version}${s.discord_token_saved ? " · Discord ✓" : ""}`;
  const list = await invoke("list_agents");
  const ul = $("#agents"); ul.replaceChildren();
  for (const a of list) {
    const li = el("li"); if (a.name === current) li.classList.add("on");
    const info = el("div"); info.append(el("span", null, a.name), el("small", null, a.description || a.endpoint));
    const x = el("button", "x", "✕"); x.title = "移除";
    x.onclick = async (ev) => { ev.stopPropagation(); if (confirm(`移除 ${a.name}？`)) { await invoke("remove_agent", { name: a.name }); if (current === a.name) select(null); refresh(); } };
    li.append(info, x); li.onclick = () => select(a.name); ul.append(li);
  }
}

function select(name) {
  current = name;
  $("#empty").hidden = !!name; $("#chat").hidden = !name;
  if (name) { $("#chat-title").textContent = name; $("#log").replaceChildren(...(logs[name] ?? [])); scroll(); }
  refresh();
}

$("#add-form").onsubmit = async (e) => {
  e.preventDefault();
  const f = Object.fromEntries(new FormData(e.target));
  const btn = e.target.querySelector("button"); btn.disabled = true; $("#add-err").textContent = "";
  try {
    const cardName = await invoke("add_a2a_agent", { name: f.name, url: f.url, headerName: f.header_name || null, headerValue: f.header_value || null });
    e.target.reset(); select(f.name); push(f.name, el("div", "meta", `已连接：${cardName}`));
  } catch (err) { $("#add-err").textContent = String(err); }
  btn.disabled = false;
};

$("#send").onsubmit = async (e) => {
  e.preventDefault();
  const ta = e.target.text; const text = ta.value.trim();
  if (!text || !current) return;
  ta.value = "";
  const agent = current;
  contexts[agent] ??= crypto.randomUUID();
  push(agent, el("div", "msg me", text));
  live[agent] = null;
  try { await invoke("prompt_agent", { name: agent, contextId: contexts[agent], text }); }
  catch (err) { push(agent, el("div", "meta err", String(err))); }
};
$("#send textarea").addEventListener("keydown", (e) => { if (e.key === "Enter" && e.metaKey) $("#send").requestSubmit(); });

listen("agent-event", ({ payload: { agent, event } }) => {
  switch (event.type) {
    case "text_chunk":
      if (!live[agent]) { live[agent] = el("div", "msg bot"); push(agent, live[agent]); }
      live[agent].textContent += event.text; if (agent === current) scroll(); break;
    case "tool": push(agent, el("div", "meta", `🔧 ${event.title} · ${event.status}`)); break;
    case "phase": break;
    case "done":
      if (event.phase !== "completed") push(agent, el("div", "meta err", `结束：${event.phase}${event.stop_reason ? " · " + event.stop_reason : ""}`));
      live[agent] = null; break;
  }
});

$("#discord-form").onsubmit = async (e) => {
  e.preventDefault();
  try { await invoke("save_discord_token", { token: e.target.token.value }); e.target.reset(); $("#discord-out").textContent = "已存入钥匙串"; refresh(); }
  catch (err) { $("#discord-out").textContent = String(err); }
};
$("#discord-check").onclick = async (e) => {
  e.target.disabled = true; $("#discord-out").textContent = "连接中…";
  try { $("#discord-out").textContent = await invoke("discord_check"); }
  catch (err) { $("#discord-out").textContent = String(err); }
  e.target.disabled = false;
};

refresh();
