#!/usr/bin/env python3
"""Disco UI regression gate.

Drives the real UI (crates/disco-app/ui, loaded through ui-test/index.html with the Tauri stub in
ui-test/stub.js) in headless Chromium, in light + dark and as macOS + Windows, and asserts every row of
notes/ui-feature-inventory.md at least once. Exits non-zero on any failed assertion, any page error or
console error, any stray "null"/"undefined"/"NaN"/"[object Object]" text, any invoke the stub does not
know, or a missing expected invoke.

  pip install playwright && python -m playwright install --with-deps chromium
  python ui-test/audit.py                      # serves the repo itself on a free port
  python ui-test/audit.py --base http://127.0.0.1:8765   # or use a running static server rooted at the repo
  python ui-test/audit.py --chromium /usr/bin/chromium   # use a system Chromium instead of Playwright's
  python ui-test/audit.py --only light-mac -v            # one combination, print every assertion
"""
import argparse
import asyncio
import functools
import json
import os
import sys
import threading
import time
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

from playwright.async_api import async_playwright

ROOT = Path(__file__).resolve().parent.parent
LOGO = str(ROOT / "crates/disco-app/ui/logo.png")
COMBOS = [("light", "mac"), ("dark", "mac"), ("light", "win"), ("dark", "win")]
UA = {
    "mac": "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Safari/605.1.15",
    "win": "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36 Edg/120.0.0.0",
}
PLATFORM = {"mac": "MacIntel", "win": "Win32"}

# Stray text: a text node or a title/placeholder/aria-label that reads null/undefined/NaN/[object Object].
STRAY_JS = r"""(() => {
  const bad = (s) => /^(null|undefined)$/.test(s) || /\bundefined\b|\bNaN\b|\[object Object\]/.test(s);
  const out = [];
  const w = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT);
  while (w.nextNode()) {
    const n = w.currentNode, p = n.parentElement;
    if (!p || p.closest("script,style")) continue;
    const t = n.textContent.trim();
    if (t && bad(t)) out.push(`text "${t.slice(0, 60)}" in <${p.tagName.toLowerCase()} class="${p.className}">`);
  }
  for (const el of document.body.querySelectorAll("[title],[placeholder],[aria-label]"))
    for (const a of ["title", "placeholder", "aria-label"]) {
      const v = el.getAttribute(a);
      if (v != null && bad(v.trim())) out.push(`${a}="${v.slice(0, 60)}" on <${el.tagName.toLowerCase()} class="${el.className}">`);
    }
  return out;
})()"""

INIT_JS = """
Object.defineProperty(Navigator.prototype, 'platform', { get: () => %s, configurable: true });
Object.defineProperty(navigator, 'clipboard', { configurable: true, value: {
  writeText: async (t) => { window.CLIP = t; }, readText: async () => window.CLIP ?? '' } });
"""


def serve(root):
    class Quiet(SimpleHTTPRequestHandler):
        def log_message(self, *a):
            pass
    handler = functools.partial(Quiet, directory=str(root))
    srv = ThreadingHTTPServer(("127.0.0.1", 0), handler)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    return srv, f"http://127.0.0.1:{srv.server_address[1]}"


class Audit:
    def __init__(self, browser, base, theme, plat, verbose):
        self.browser, self.base, self.theme, self.plat, self.verbose = browser, base, theme, plat, verbose
        self.label = f"{theme}-{plat}"
        self.mod = "Meta" if plat == "mac" else "Control"
        self.n = 0
        self.fails = []
        self.pages = []

    # ---------- plumbing
    def ok(self, name, cond, detail=""):
        self.n += 1
        if self.verbose:
            print(f"  {'ok ' if cond else 'FAIL'} {name}{'' if cond else '  ' + str(detail)[:300]}")
        if not cond:
            self.fails.append(f"[{self.label}] {name}: {str(detail)[:400]}")
        return cond

    async def new_page(self, init="", ready="document.querySelector('#pane .blank')"):
        ctx = await self.browser.new_context(viewport={"width": 1240, "height": 760}, color_scheme=self.theme,
                                             user_agent=UA[self.plat], locale="zh-CN", timezone_id="Asia/Shanghai")
        await ctx.add_init_script(INIT_JS % json.dumps(PLATFORM[self.plat]) + init)
        pg = await ctx.new_page()
        pg.set_default_timeout(3000)
        tag = f"page{len(self.pages)}"
        pg.on("pageerror", lambda e: self.fails.append(f"[{self.label}/{tag}] PAGE ERROR {e} {(e.stack or '')[:300]}"))
        pg.on("console", lambda m: m.type == "error" and self.fails.append(f"[{self.label}/{tag}] CONSOLE ERROR {m.text[:300]}"))
        self.pages.append(pg)
        await pg.goto(f"{self.base}/ui-test/index.html")
        await pg.wait_for_function(f"typeof S !== 'undefined' && S.ov && {ready}")
        await self.settle(pg)
        return pg

    async def settle(self, pg=None, ms=80):
        pg = pg or self.pg
        await pg.evaluate("new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r)))")
        await pg.wait_for_timeout(ms)

    async def stray(self, where):
        bad = await self.pg.evaluate(STRAY_JS)
        self.ok(f"no stray null/undefined text after {where}", not bad, bad[:4])

    async def step(self, name, fn):
        """Run one UI action; any exception is a failure; then check for stray text."""
        try:
            await fn()
        except Exception as e:  # noqa: BLE001
            self.fails.append(f"[{self.label}] step '{name}' raised: {str(e).splitlines()[0][:300]}")
        await self.settle()
        await self.stray(name)

    async def js(self, expr, arg=None):
        return await self.pg.evaluate(expr, arg)

    async def mark(self):
        return await self.js("CALLS.length")

    async def calls(self, cmd, since=0):
        return await self.js("([c, s]) => CALLS.slice(s).filter((x) => x[0] === c).map((x) => x[1])", [cmd, since])

    async def expect(self, name, cmd, since, pred=lambda a: True, count=1, wait=2.0):
        """Assert that `cmd` was invoked (at least `count` times) since mark `since` with args matching `pred`."""
        end = time.monotonic() + wait
        hits = []
        while True:
            got = await self.calls(cmd, since)
            hits = [a for a in got if pred(a or {})]
            if len(hits) >= count or time.monotonic() > end:
                break
            await self.pg.wait_for_timeout(50)
        self.ok(f"{name} → invoke {cmd}", len(hits) >= count, f"want {count} matching, got {got}")
        return hits

    async def no_call(self, name, cmd, since):
        await self.settle(ms=120)
        got = await self.calls(cmd, since)
        self.ok(f"{name} → no {cmd}", not got, got)

    async def text(self, sel):
        return (await self.pg.locator(sel).first.inner_text()).strip()

    async def count(self, sel):
        return await self.pg.locator(sel).count()

    async def has(self, sel):
        return await self.pg.locator(sel).count() > 0

    async def wait(self, sel, state="attached", timeout=2500):
        try:
            await self.pg.locator(sel).first.wait_for(state=state, timeout=timeout)
            return True
        except Exception:  # noqa: BLE001
            return False

    async def hover_click(self, hover_sel, click_sel):
        await self.pg.locator(hover_sel).first.hover()
        await self.pg.locator(click_sel).first.click()

    async def press(self, keys):
        await self.pg.keyboard.press(keys.replace("Mod", self.mod))

    async def fire(self, event, payload):
        await self.js("([n, p]) => __L[n]({ payload: p })", [event, payload])
        await self.settle()

    async def open(self, thread):
        await self.js("(t) => openThread(t)", thread)
        await self.pg.wait_for_function("(t) => S.thread === t && document.querySelector('#log')", arg=thread)
        await self.settle(ms=150)

    async def tab(self, name):
        await self.pg.click(f"#rail [data-tab={name}]")
        await self.settle(ms=120)

    async def toast_click(self, label):
        await self.pg.locator(f".toast button:has-text('{label}')").click()

    async def list_names(self):
        return await self.js("[...document.querySelectorAll('#items li')].map((l) => l.textContent)")

    # ---------- suites
    async def boot(self):
        win = self.plat == "win"
        listeners = await self.js("Object.keys(__L).sort()")
        want = sorted(["agents", "message", "typing", "stream", "inbox", "team", "space", "update-available", "discord-status"])
        self.ok("E1 every backend event has a listener", listeners == want, listeners)
        self.ok("K2 html.win only on Windows", await self.js("document.documentElement.classList.contains('win')") == win)
        rail = await self.js("getComputedStyle(document.documentElement).getPropertyValue('--rail-w').trim()")
        self.ok("K2 rail width (traffic-light inset on macOS only)", rail == ("64px" if win else "84px"), rail)
        self.ok("K2 shortcut hint uses the platform modifier", await self.text("#search-kbd") == ("Ctrl+K" if win else "⌘K"))
        bg = await self.js("getComputedStyle(document.body).backgroundColor")
        lum = await self.js("(c) => { const [r, g, b] = c.match(/\\d+/g).map(Number); return (0.2126*r + 0.7152*g + 0.0722*b) / 255; }", bg)
        self.ok(f"{self.theme} colour scheme applied", lum > 0.8 if self.theme == "light" else lum < 0.25, bg)
        self.ok("B1 blank page logo", await self.has("#pane .blank .logo img[src='logo.png']"))
        keys = await self.text("#pane .blank .keys")
        self.ok("B1 blank page key hints", ("Ctrl+K" if win else "⌘K") in keys and "设置" in keys, keys)
        self.ok("R3 inbox badge counts pending items", await self.text("#inbox-badge") == "3")
        logo_ok = await self.js("new Promise((r) => { const i = document.querySelector('.blank .logo img'); i.complete ? r(i.naturalWidth > 0) : (i.onload = () => r(true), i.onerror = () => r(false)); })")
        self.ok("harness serves the real ui/ assets (logo.png loads)", logo_ok)
        await self.stray("boot")

    async def rail(self):
        pg = self.pg
        for t, check in [("groups", "#items li"), ("agents", ".page.add h1"), ("inbox", "#items li.note"), ("settings", ".page.settings"), ("chats", "#items li")]:
            await self.step(f"tab {t}", lambda: self.tab(t))
            on = await self.js("[...document.querySelectorAll('.tab[data-tab].on')].map((b) => b.dataset.tab)")
            self.ok(f"R1 click tab {t}", on == [t] and await self.js("S.tab") == t and await self.has(check), on)
        for key, t in [("Mod+2", "groups"), ("Mod+3", "agents"), ("Mod+4", "inbox"), ("Mod+Comma", "settings"), ("Mod+1", "chats")]:
            await self.step(key, lambda: self.press(key))
            self.ok(f"R1 shortcut {key} → {t}", await self.js("S.tab") == t)
        if self.plat == "mac":
            await self.step("Control+2 on mac", lambda: pg.keyboard.press("Control+2"))
            self.ok("K2 Control+digit is not a tab shortcut on macOS", await self.js("S.tab") == "chats")
        # collapse / expand
        self.ok("R2 expand button hidden while the list is open", not await pg.locator("#expand").is_visible())
        await self.step("collapse", lambda: self.hover_click("#list header", "#collapse"))
        self.ok("L3 collapse hides the list", await self.js("document.body.classList.contains('collapsed')"))
        self.ok("L3 collapsed state remembered", await self.js("localStorage.getItem('disco.collapsed')") == "1")
        self.ok("R2 expand button shown when collapsed", await pg.locator("#expand").is_visible())
        await self.step("expand", lambda: pg.click("#expand"))
        self.ok("R2 expand reopens the list", not await self.js("document.body.classList.contains('collapsed')"))
        await self.step("Mod+\\", lambda: self.press("Mod+Backslash"))
        self.ok("L3 Mod+\\ collapses", await self.js("document.body.classList.contains('collapsed')"))
        await self.step("Mod+\\ again", lambda: self.press("Mod+Backslash"))
        self.ok("L3 Mod+\\ expands", not await self.js("document.body.classList.contains('collapsed')"))
        await self.step("collapse then tab", lambda: (self.hover_click("#list header", "#collapse")))
        await self.step("tab groups while collapsed", lambda: self.press("Mod+2"))
        self.ok("L3 switching tab reopens a collapsed list", not await self.js("document.body.classList.contains('collapsed')"))
        await self.tab("chats")

    async def list_column(self):
        pg = self.pg
        names = await self.list_names()
        self.ok("L6 chats list: 5 DMs + 1 group + 3 peer threads", len(names) == 9, names)
        first = await self.js("document.querySelector('#items li .name').textContent")
        self.ok("L6 rows sorted by last message (codex newest)", first == "codex", first)
        row = lambda name: f"#items li:has(.name:text-is('{name}'))"
        self.ok("L6 system-head warning dot", await self.has(f"{row('codex')} .av .dot.amber"))
        self.ok("L6 waiting on you → amber avatar dot", await self.has(f"{row('claude')} .av[data-st=wait] .dot.amber"))
        self.ok("L6 busy agent shows its plan step", "写 README" in await self.text(row("claude")))
        self.ok("L6 waiting count badge on the group", await self.text(f"{row('dev')} .row-badge") == "2")
        self.ok("L6 group head shows author", "claude：hi" in await self.text(row("dev")))
        self.ok("L6 peer guest → agent route", await self.has("#items li .name.route:has-text('小王'):has(b:text-is('claude'))"))
        self.ok("C1 uploaded picture renders as <img>", await self.has(f"{row('writer')} .av.pic img"))
        self.ok("C1 legacy form.palette avatar renders initials", (await self.text(f"{row('researcher')} .av .ini")) == "R")
        # unread + typing
        await self.step("message elsewhere", lambda: self.fire("message", {"thread": "dm:writer", "author": "writer", "body": "新消息来了", "ts": int(time.time())}))
        self.ok("L6 unread count", await self.text(f"{row('writer')} .unread-n") == "1")
        self.ok("L6 newest message moves the row to the top", await self.js("document.querySelector('#items li .name').textContent") == "writer")
        await self.step("typing", lambda: self.fire("typing", {"thread": "dm:researcher", "agent": "researcher", "on": True}))
        self.ok("L6 typing row busy + status text", await self.has(f"{row('researcher')}.busy") and "思考中" in await self.text(row("researcher")))
        await self.step("typing off", lambda: self.fire("typing", {"thread": "dm:researcher", "agent": "researcher", "on": False}))
        self.ok("L6 typing cleared", not await self.has(f"{row('researcher')}.busy"))
        # search
        await self.step("Mod+K", lambda: self.press("Mod+k"))
        self.ok("L1 Mod+K focuses search", await self.js("document.activeElement.id") == "search")
        await self.step("search codex", lambda: pg.fill("#search", "codex"))
        names = await self.list_names()
        self.ok("L1 search filters rows", names and all("codex" in n.lower() for n in names) and len(names) < 9, names)
        await self.step("search esc", lambda: pg.press("#search", "Escape"))
        self.ok("L1 Esc clears the search", await pg.input_value("#search") == "" and len(await self.list_names()) == 9)
        await self.step("search zzz", lambda: pg.fill("#search", "zzzz"))
        self.ok("L11 no-hit empty state", "没有和「zzzz」相关的" in await self.text("#items li.empty"))
        await self.step("search codex enter", lambda: pg.fill("#search", "codex"))
        await self.step("enter opens first", lambda: pg.press("#search", "Enter"))
        self.ok("L1 ↩ opens the first hit", await self.js("S.thread") == "dm:codex")
        await self.step("search clear", lambda: (pg.fill("#search", "")))
        self.ok("L6 row click target highlighted", await self.has(f"{row('codex')}.on"))
        # list-add per tab
        self.ok("L2 compose button titled 新建群聊 on chats", "新建群聊" in (await pg.get_attribute("#list-add", "title")))
        await self.tab("agents")
        self.ok("L2 compose button becomes 添加 agent on Agent tab", "添加 agent" in (await pg.get_attribute("#list-add", "title")) and await self.has("#list-add[data-ic=plus]"))
        await self.tab("inbox")
        self.ok("L2 compose button quiet on inbox", await self.has("#list-add.off"))
        await self.tab("chats")
        await self.step("list-add", lambda: pg.click("#list-add"))
        self.ok("L2 ＋ on chats opens 新建群聊", await self.text("#pane h1") == "新建群聊")
        await self.step("Mod+N agents", lambda: (self.tab("agents")))
        await self.step("Mod+N", lambda: self.press("Mod+n"))
        self.ok("L2 Mod+N on Agent tab opens 添加 Agent", await self.text("#pane h1") == "添加 Agent")
        await self.tab("chats")

    async def inbox_tab(self):
        pg = self.pg
        await self.js("window.__INBOX0 = JSON.parse(JSON.stringify(STUB.inbox))")
        await self.tab("inbox")
        self.ok("L8 inbox cards", await self.count("#items li.note .perm") == 3)
        a1 = "#items li.note .perm[data-item=a1]"
        self.ok("L8 permission card: 拒绝 + 允许一次 primary + ⋯", await self.has(f"{a1} button:text-is('拒绝')") and await self.has(f"{a1} .btn.pri:text-is('允许一次')") and await self.has(f"{a1} .ib.more"))
        self.ok("L8 question card shows options and free text", await self.count("#items .perm[data-item=a2] .opts .btn") == 2 and await self.has("#items .perm[data-item=a2] textarea"))
        self.ok("L8 card names agent and place", "codex" in await self.text(a1) and "dev" in await self.text(a1))
        m = await self.mark()
        await self.step("inbox more", lambda: pg.click(f"{a1} .ib.more"))
        self.ok("L8 ⋯ holds the always option", await self.has(".pop .pop-item:has-text('总是允许（以后不再询问）')"))
        await self.step("close pop", lambda: pg.keyboard.press("Escape"))
        await self.step("deny", lambda: pg.click(f"{a1} button:text-is('拒绝')"))
        await self.expect("L8 拒绝", "inbox_answer", m, lambda a: a.get("id") == "a1" and a.get("optionId") is None)
        await self.step("open from card", lambda: pg.click("#items li.note:has(.perm[data-item=a3]) .q"))
        self.ok("L8 clicking a card opens its thread", await self.js("S.thread") == "dm:claude")
        # an answered item disappears; empty state; a new one toasts
        await self.step("inbox empty", lambda: self.fire("inbox", []))
        self.ok("L8 calm empty state", await self.has("#items li.empty.calm .okmark") and "没有等你处理的事" in await self.text("#items li.empty"))
        self.ok("R3 badge clears", await self.text("#inbox-badge") == "")
        await self.js("STUB.inbox = JSON.parse(JSON.stringify(window.__INBOX0))")
        await self.step("inbox back", lambda: self.js("__L.inbox({ payload: STUB.inbox })"))
        self.ok("D4 toast for a new request elsewhere, with 查看", await self.has(".toast button:text-is('查看')"))
        self.ok("R3 badge back", await self.text("#inbox-badge") == "3")
        await self.tab("chats")

    async def dm_chat(self):
        pg = self.pg
        await self.open("dm:claude")
        self.ok("C3 DM title", await self.text(".head h2") == "claude")
        sub = await self.text("#chat-sub")
        self.ok("C3 DM subtitle kind · folder · branch", "本机" in sub and "~/code" in sub and await self.has("#chat-sub .branch:has-text('main')"), sub)
        self.ok("C11 code block with copy button", await self.has("#log pre .copy"))
        await self.step("code copy", lambda: pg.click("#log pre .copy"))
        self.ok("C11 code copy", await self.js("window.CLIP") == "fn main() {}" and await self.has("#log pre .copy.ok"))
        self.ok("C11 agent name + avatar + time on first of run", await self.has("#log .m:not(.me) .who time") and await self.has("#log .m:not(.me) > .av"))
        self.ok("C11 my message is a bubble", await self.has("#log .m.me .b"))
        await self.step("msg copy", lambda: self.hover_click("#log .m:not(.me):not(.sys)", "#log .m:not(.me):not(.sys) .tools button[title=复制]"))
        self.ok("C15 copy on a message", (await self.js("window.CLIP") or "").startswith("好的："))
        await self.step("quote", lambda: self.hover_click("#log .m:not(.me):not(.sys)", "#log .m:not(.me):not(.sys) .tools button[title=引用]"))
        self.ok("C15 quote into the composer", (await pg.input_value("#input")).startswith("> 好的："))
        await pg.fill("#input", "")
        # inline question card with free text
        self.ok("C12 inline question card in the chat", await self.has("#log .perm[data-item=a3] textarea"))
        m = await self.mark()
        await pg.fill("#log .perm[data-item=a3] textarea", "中文")
        await self.step("answer enter", lambda: pg.press("#log .perm[data-item=a3] textarea", "Enter"))
        await self.expect("C12 free-text answer", "inbox_answer", m, lambda a: a.get("id") == "a3" and a.get("text") == "中文")
        await self.step("inbox event removes card", lambda: self.js("__L.inbox({ payload: STUB.inbox })"))
        await pg.wait_for_timeout(250)
        self.ok("C12 answered card leaves the chat", not await self.has("#log .perm[data-item=a3]"))
        # model picker
        self.ok("C5 model pill shows the current model", await self.wait("#cfgs .model-pill .lbl") and await self.text("#cfgs .model-pill .lbl") == "Sonnet")
        m = await self.mark()
        await self.step("model pill", lambda: pg.click("#cfgs .model-pill"))
        await self.step("pick opus", lambda: pg.click(".pop .pop-item:has-text('Opus')"))
        await self.expect("C5 switch model", "set_agent_config", m, lambda a: a == {"agent": "claude", "id": "model", "value": "opus"})
        self.ok("C5 pill label follows", await self.text("#cfgs .model-pill .lbl") == "Opus")
        # memory
        self.ok("C6 memory button on", await self.has(".mem-btn.on[aria-pressed=true]"))
        m = await self.mark()
        await self.step("mem off", lambda: pg.click(".mem-btn"))
        await self.expect("C6 memory off", "set_memory", m, lambda a: a == {"thread": "dm:claude", "on": False})
        self.ok("C6 memory button off + toast", not await self.has(".mem-btn.on") and "记忆已关闭" in await self.text(".toast"))
        # to-dos count + inspector
        self.ok("C7 to-do count on header", await self.wait("#todo-count:text-is('2')"))
        if await self.has("#insp"):
            await pg.click(".insp-btn")
        await self.step("inspector", lambda: pg.click(".insp-btn"))
        segs = await self.js("[...document.querySelectorAll('#insp .seg2 button')].map((b) => b.textContent.trim())")
        self.ok("inspector DM tabs 信息/待办/文档", segs == ["信息", "待办 2", "文档"], segs)
        self.ok("inspector 信息 hero + rows", await self.text("#insp .ihero b") == "claude" and "~/code" in await self.text("#insp-body"))
        self.ok("C6 inspector memory row mirrors header (off)", await self.has("#insp .memrow[aria-checked=false]"))
        m = await self.mark()
        await self.step("insp mem on", lambda: pg.click("#insp .memrow"))
        await self.expect("C6 inspector memory on", "set_memory", m, lambda a: a == {"thread": "dm:claude", "on": True})
        self.ok("C6 header follows inspector", await self.has(".mem-btn.on") and await self.has("#insp .memrow[aria-checked=true]"))
        self.ok("C9 inspector agent actions", all([await self.has(f"#insp button.li:has-text('{t}')") for t in ["更换头像", "共享给朋友", "重命名", "重启", "删除 Agent"]]))
        # 待办
        await self.step("seg todo", lambda: pg.click("#insp .seg2 button:has-text('待办')"))
        self.ok("S1 to-dos listed", await self.count("#insp ul.todos li") == 3 and await self.count("#insp ul.todos li.done") == 1)
        self.ok("C7 header to-do button lit", await self.has(".space-btn.on"))
        self.ok("S1 assignee avatar", await self.has("#insp ul.todos li .who .av"))
        m = await self.mark()
        await self.step("todo check", lambda: pg.click("#insp ul.todos li >> nth=0 >> .chk"))
        await self.expect("S1 check a to-do", "todo_set", m, lambda a: a == {"thread": "dm:claude", "id": "123456", "done": True})
        self.ok("S1 checked row", await self.has("#insp ul.todos li.done >> nth=1"))
        await self.step("todo del", lambda: self.hover_click("#insp ul.todos li >> nth=1", "#insp ul.todos li >> nth=1 >> .del"))
        await self.expect("S1 delete a to-do", "todo_remove", m, lambda a: a.get("id") == "223456")
        await self.step("todo undo", lambda: self.toast_click("撤销"))
        await self.expect("S1 undo delete", "todo_new", m, lambda a: a.get("text") == "写 README 的 MCP 章节")
        m = await self.mark()
        await self.step("space event", lambda: self.fire("space", "dm:claude"))
        await self.expect("S3 space event refreshes", "space", m)
        await self.step("todo add", lambda: pg.fill("#insp .todo-in", "新待办"))
        await self.step("todo add enter", lambda: pg.press("#insp .todo-in", "Enter"))
        await self.expect("S1 add a to-do", "todo_new", m, lambda a: a == {"thread": "dm:claude", "text": "新待办"})
        m = await self.mark()
        await pg.fill("#insp .todo-in", "半句")
        await self.fire("space", "dm:claude")
        await self.no_call("S3 a half-typed to-do is not wiped", "space", m)
        self.ok("S3 half-typed to-do kept", await pg.input_value("#insp .todo-in") == "半句")
        # 文档
        await self.step("seg doc", lambda: pg.click("#insp .seg2 button:has-text('文档')"))
        self.ok("S2 doc rendered", await self.text("#insp .doc h4") == "目标")
        reveal = "在资源管理器中显示" if self.plat == "win" else "在访达中显示"
        m = await self.mark()
        await self.step("reveal doc", lambda: pg.click(f"#insp button:text-is('{reveal}')"))
        await self.expect(f"S2 {reveal}", "reveal_doc", m)
        await self.step("doc edit", lambda: pg.click("#insp button:text-is('编辑')"))
        self.ok("S2 doc editor", await self.has("#insp textarea.doc-edit"))
        await pg.fill("#insp textarea.doc-edit", "# 新文档")
        await self.step("doc save", lambda: pg.click("#insp button:text-is('保存')"))
        await self.expect("S2 doc save", "doc_save", m, lambda a: a == {"thread": "dm:claude", "content": "# 新文档"})
        await self.step("close insp", lambda: pg.click(".insp-btn"))
        self.ok("inspector closes", not await self.has("#insp"))
        # ⋯ menu
        await self.step("dm menu", lambda: pg.click(".head .more-btn"))
        items = await self.js("[...document.querySelectorAll('.pop .pop-item')].map((b) => b.textContent.trim())")
        self.ok("C9 agent ⋯ menu", items == ["更换头像…", "共享给朋友…", "重命名", "重启", "全部停止…", "删除 Agent…"], items)
        m = await self.mark()
        await self.step("restart", lambda: pg.click(".pop .pop-item:has-text('重启')"))
        await self.expect("C9 重启", "restart_agent", m, lambda a: a == {"name": "claude"})
        self.ok("C9 restart toast", "claude 正在重启" in await self.text(".toast"))
        await self.step("dm menu 2", lambda: pg.click(".head .more-btn"))
        await self.step("stop arm", lambda: pg.click(".pop .pop-item:has-text('全部停止')"))
        self.ok("R4 全部停止 needs a second click", await self.has(".pop .pop-item.armed:has-text('再点一次')"))
        await self.no_call("R4 first click does not stop", "stop_all", m)
        await self.step("stop fire", lambda: pg.click(".pop .pop-item.armed"))
        await self.expect("R4 second click stops all", "stop_all", m)
        self.ok("L4 paused bar", await self.wait("#paused-bar", "visible") and await self.has(".toast button:text-is('恢复')"))
        await self.step("dm menu 3", lambda: pg.click(".head .more-btn"))
        self.ok("C9 menu offers resume while paused", await self.has(".pop .pop-item:has-text('恢复所有 agent')"))
        await self.step("escape pop", lambda: pg.keyboard.press("Escape"))
        await self.step("paused-bar resume", lambda: pg.click("#resume"))
        await self.expect("L4 恢复", "resume", m)
        self.ok("L4 paused bar hides", await self.wait("#paused-bar", "hidden"))
        # avatar upload from the header
        await self.step("header avatar", lambda: pg.click(".head > .av.pickable"))
        self.ok("C1 avatar dialog", await self.text("#dlg h3") == "claude 的头像")
        async with pg.expect_file_chooser() as fc:
            await pg.click("#dlg .linkbtn.acc")
        await (await fc.value).set_files(LOGO)
        self.ok("C1 preview shows the picked picture", await self.wait("#dlg .avslot img"))
        m = await self.mark()
        await self.step("avatar save", lambda: pg.click("#dlg button.btn.pri"))
        hits = await self.expect("C1 upload → set_avatar", "set_avatar", m, lambda a: a.get("agent") == "claude" and str(a.get("data", "")).startswith("data:image/png"))
        if hits:
            size = await self.js("(d) => new Promise((r) => { const i = new Image(); i.onload = () => r([i.width, i.height]); i.onerror = () => r(null); i.src = d; })", hits[0]["data"])
            self.ok("C1 cropped to 256×256", size == [256, 256], size)
        self.ok("C1 header shows the new picture", await self.wait(".head > .av.pic img"))
        # remove a picture
        await self.step("avatar dialog writer", lambda: self.js("avatarDialog('writer')"))
        m = await self.mark()
        await self.step("avatar remove", lambda: pg.click("#dlg .linkbtn:not(.acc)"))
        await self.step("avatar remove save", lambda: pg.click("#dlg button.btn.pri"))
        await self.expect("C1 移除 → set_avatar null", "set_avatar", m, lambda a: a == {"agent": "writer", "data": None})

    async def dm_codex(self):
        pg = self.pg
        await self.open("dm:codex")
        sub = await self.text("#chat-sub")
        self.ok("C3 no dangling separator (kind · branch)", sub.startswith("本机") and "disco/codex" in sub and "本机 · ·" not in sub, sub)
        self.ok("C5 cannot start → 需要登录 pill", await self.wait("#cfgs .model-pill.warn") and await self.text("#cfgs .model-pill.warn .lbl") == "需要登录")
        self.ok("C11 📎 becomes a paperclip + path link", await self.has("#log .b .clipi") and await self.has("#log a.path"))
        m = await self.mark()
        await self.step("reveal path", lambda: pg.click("#log a.path"))
        await self.expect("C11 path reveals in Finder", "reveal_path", m, lambda a: a.get("path", "").endswith("/results.json"))
        self.ok("C11 error notice with warning icon", await self.has("#log .m.sys.warn .wi"))
        await self.step("retry notice", lambda: pg.click("#log .m.sys.warn button.act:text-is('重试')"))
        await self.expect("C11 重试 resends my last message", "send", m, lambda a: a == {"thread": "dm:codex", "text": "服务器上复现一下这三个 bug"})
        await self.open("dm:helper")
        self.ok("C3 bare kind subtitle has no trailing ·", (await self.text("#chat-sub")) == "本机", await self.text("#chat-sub"))

    async def composer(self):
        pg = self.pg
        await self.open("dm:writer")
        self.ok("C18 send disabled when empty", await pg.locator("#send").is_disabled())
        self.ok("C18 short placeholder", await pg.get_attribute("#input", "placeholder") == "发消息给 writer")
        await pg.fill("#input", "你好")
        self.ok("C18 send enabled with text", await pg.locator("#send").is_enabled())
        m = await self.mark()
        await self.step("enter send", lambda: pg.press("#input", "Enter"))
        await self.expect("C18 ↩ sends", "send", m, lambda a: a == {"thread": "dm:writer", "text": "你好"})
        self.ok("C18 optimistic bubble + cleared input", await self.has("#log .m.me.pending") and await pg.input_value("#input") == "")
        await self.step("echo", lambda: self.fire("message", {"thread": "dm:writer", "author": "me", "body": "你好", "ts": int(time.time())}))
        self.ok("C18 echo confirms the bubble", not await self.has("#log .m.me.pending"))
        self.ok("C15 ↻ on my last message", await self.count("#log .m.me .again") == 1)
        m = await self.mark()
        await self.step("again", lambda: self.hover_click("#log .m.me:has(.again)", "#log .m.me .again"))
        await self.expect("C15 ↻ resends", "send", m, lambda a: a.get("text") == "你好")
        m = await self.mark()
        await pg.fill("#input", "第一行")
        await self.step("shift enter", lambda: pg.press("#input", "Shift+Enter"))
        self.ok("C18 ⇧↩ inserts a newline", "\n" in await pg.input_value("#input"))
        await self.js("""(() => { const t = document.querySelector('#input'); t.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', keyCode: 229, bubbles: true, cancelable: true })); })()""")
        await self.no_call("C18 IME-confirming Enter does not send", "send", m)
        await pg.fill("#input", "")
        # failure → retry
        await self.js("STUB_FAIL.send = '网络断了'")
        await pg.fill("#input", "会失败")
        await self.step("send fail", lambda: pg.press("#input", "Enter"))
        self.ok("C15 failed → 发送失败 · 重发", await self.wait("#log .m.me.failed .retry:text-is('发送失败 · 重发')"))
        await self.js("STUB_FAIL = {}")
        m = await self.mark()
        await self.step("retry", lambda: pg.click("#log .m.me.failed .retry"))
        await self.expect("C15 重发", "send", m, lambda a: a.get("text") == "会失败")
        # streaming reply, tool chip, stop
        await self.step("typing on", lambda: self.fire("typing", {"thread": "dm:writer", "agent": "writer", "on": True}))
        self.ok("C13 typing dots bubble", await self.has("#log .b.streaming .typing-dots"))
        self.ok("C18 send becomes stop while replying", await self.has("#send.stop") and await pg.get_attribute("#send", "title") == "停止回复")
        await self.step("chunk", lambda: self.fire("stream", {"thread": "dm:writer", "agent": "writer", "event": {"type": "text_chunk", "text": "正在写**答案**"}}))
        await pg.wait_for_timeout(60)
        self.ok("C13 streamed text renders markdown", await self.has("#log .b.streaming b:text-is('答案')"))
        await self.step("tool", lambda: self.fire("stream", {"thread": "dm:writer", "agent": "writer", "event": {"type": "tool", "status": "in_progress", "title": "Read src/main.rs"}}))
        self.ok("C14 tool chip", await self.has("#log .chip:has-text('Read src/main.rs') .ic"))
        self.ok("L6 tool status in the list row", "调工具" in await self.text("#items li:has(.name:text-is('writer'))"))
        m = await self.mark()
        await self.step("stop", lambda: pg.click("#send.stop"))
        await self.expect("C18 stop button stops this chat", "stop_reply", m, lambda a: a == {"thread": "dm:writer", "agent": "writer"})
        await self.step("final message", lambda: self.fire("message", {"thread": "dm:writer", "author": "writer", "body": "最终答案", "ts": int(time.time())}))
        await self.step("typing off", lambda: self.fire("typing", {"thread": "dm:writer", "agent": "writer", "on": False}))
        self.ok("C13 live bubble replaced by the final message", not await self.has("#log .b.streaming") and "最终答案" in await self.text("#log .m:last-child"))
        self.ok("C18 stop turns back into send", not await self.has("#send.stop"))
        # type anywhere
        await self.js("document.activeElement.blur()")
        await self.step("type anywhere", lambda: pg.keyboard.press("x"))
        self.ok("C23 typing anywhere focuses the composer", await self.js("document.activeElement.id") == "input")
        await pg.fill("#input", "")
        # ↓ 新消息
        await self.open("dm:helper")
        await self.js("(() => { const l = document.querySelector('#log'); l.scrollTop = 0; })()")
        await self.step("message while scrolled up", lambda: self.fire("message", {"thread": "dm:helper", "author": "helper", "body": "新的一条", "ts": int(time.time())}))
        self.ok("C16 ↓ 新消息 jump appears when scrolled up", await self.has("#jump"))
        await self.step("jump", lambda: pg.click("#jump"))
        self.ok("C16 jump scrolls down and goes away", not await self.has("#jump"))
        # / picker in a DM
        await self.open("dm:claude")
        m = await self.mark()
        await self.step("slash", lambda: pg.fill("#input", "/"))
        await self.expect("C20 / asks the agent for its commands", "agent_commands", m, lambda a: a == {"agent": "claude"})
        self.ok("C20 agent commands listed", await self.wait(".pop.mention .pop-item:has-text('/review')") and await self.count(".pop.mention .pop-item") == 2)
        await self.step("slash pick", lambda: pg.press("#input", "Enter"))
        self.ok("C20 ↩ picks the command", await pg.input_value("#input") == "/review ")
        await pg.fill("#input", "")
        # attach
        m = await self.mark()
        await self.step("attach", lambda: pg.click("#composer .attach"))
        await self.expect("C18 attach picks a file and sends it", "send_file", m, lambda a: a.get("thread") == "dm:claude" and a.get("path") == "/Users/x/bin/my-agent")
        # rename (on helper)
        await self.open("dm:helper")
        await self.step("dblclick title", lambda: pg.dblclick(".head h2"))
        self.ok("C2 double-click makes the title editable", await self.has(".head h2.editing"))
        await self.js("(() => { const h = document.querySelector('.head h2'); h.textContent = 'helper2'; })()")
        m = await self.mark()
        await self.step("rename enter", lambda: pg.press(".head h2", "Enter"))
        await self.expect("C2 rename", "rename_agent", m, lambda a: a == {"name": "helper", "newName": "helper2"})
        await pg.wait_for_function("S.thread === 'dm:helper2'")
        self.ok("C2 renamed chat reopens", await self.text(".head h2") == "helper2")

    async def group_chat(self):
        pg = self.pg
        await self.tab("groups")
        names = await self.list_names()
        self.ok("L7 groups tab: dev + friends' Discord groups", len(names) == 3 and await self.has("#items li .name:text-is('dev')")
                and await self.has("#items li .name.route:has-text('小王')") and await self.has("#items li .name.route:has-text('阿杰')"), names)
        await self.step("open dev", lambda: pg.click("#items li:has(.name:text-is('dev'))"))
        await pg.wait_for_function("S.thread === 'group:g1' && document.querySelector('#log .m')")
        await self.settle(ms=200)
        sub = await self.text("#chat-sub")
        self.ok("C3 group subtitle (mirror, agents, people, @ rule)", "镜像到 Discord" in sub and "2 个 agent" in sub and "@ 才回答" in sub and await self.has("#chat-sub .mirror.on"), sub)
        self.ok("C11 mention tokens", await self.count("#log .mt") >= 4)
        self.ok("C11 Discord person message", await self.has("#log .m.person .av.person"))
        self.ok("C11 system hint with click-to-copy code", await self.has("#log .m.sys.warn code.cp"))
        await self.step("copy cmd", lambda: pg.click("#log .m.sys.warn button.act:text-is('复制命令')"))
        self.ok("C11 复制命令 copies the command", await self.js("window.CLIP") == "curl -fsSL https://claude.ai/install.sh | bash")
        # inline permission card
        a1 = "#log .perm[data-item=a1]"
        self.ok("C12 inline permission card", await self.has(f"{a1} .btn.pri:text-is('允许一次')") and await self.has(f"{a1} button:text-is('拒绝')"))
        self.ok("C12 inline question card keeps free text", await self.has("#log .perm[data-item=a2] textarea"))
        m = await self.mark()
        await self.step("allow once", lambda: pg.click(f"{a1} .btn.pri"))
        await self.expect("C12 允许", "inbox_answer", m, lambda a: a == {"id": "a1", "optionId": "o1", "text": None})
        await self.step("question option", lambda: pg.click("#log .perm[data-item=a2] .opts .btn:text-is('要，跑完再发')"))
        await self.expect("C12 question option", "inbox_answer", m, lambda a: a.get("id") == "a2" and a.get("optionId") == "0")
        await self.step("inbox event after answers", lambda: self.js("__L.inbox({ payload: STUB.inbox })"))
        await pg.wait_for_timeout(250)
        self.ok("C12 answered cards leave the chat", not await self.has("#log .perm[data-item]"))
        self.ok("L6 group waiting badge clears", await self.has("#items li:has(.name:text-is('dev'))") and not await self.has("#items li:has(.name:text-is('dev')) .row-badge"))
        # team strip + blackboard
        self.ok("C21 team status line", await self.wait("#team") and "codex 在 prototype" in await self.text("#team .words"))
        m = await self.mark()
        await self.step("board", lambda: pg.click("#team .board-btn"))
        await self.expect("C22 blackboard loads team files", "team_files", m, lambda a: a == {"thread": "group:g1"})
        segs = await self.js("[...document.querySelectorAll('#insp .seg2 button')].map((b) => b.textContent.trim())")
        self.ok("inspector group tabs 成员/待办/文档/黑板", segs[0] == "成员" and segs[1].startswith("待办") and segs[2:] == ["文档", "黑板"], segs)
        self.ok("C22 blackboard tab selected", await self.has("#insp .seg2 button.on:text-is('黑板')"))
        self.ok("C22 file tabs", await self.wait("#insp .bseg button:text-is('简报')") and await self.has("#insp .bseg button:text-is('research')"))
        await self.step("board file", lambda: pg.click("#insp .bseg button:text-is('research')"))
        self.ok("C22 switching files", "JSON-RPC" in await self.text("#insp .doc"))
        # 成员 tab
        await self.step("members tab", lambda: pg.click("#insp .seg2 button:text-is('成员')"))
        body = await self.text("#insp-body")
        self.ok("C4 member list (agents, guests, people)", all(x in body for x in ["claude", "codex", "reviewer", "朋友的 agent", "Amy", "群主"]), body[:200])
        self.ok("C4 lead + role in member subtitle", "组长 · 本机 · research" in body, body[:300])
        self.ok("C8 member rows: 添加 agent / 镜像 / 群设置", await self.has("#insp button.li.add") and await self.has("#insp button.li:has-text('镜像到 Discord')") and await self.has("#insp button.li:has-text('群设置')"))
        self.ok("C6 group memory row", await self.has("#insp .memrow"))
        # team done → dismiss
        done = {**(await self.js("STUB.team['group:g1']")), "stage": "done"}
        await self.step("team done", lambda: self.fire("team", done))
        self.ok("C21 finished team line can be dismissed", await self.has("#team.done button[title=收起]"))
        await self.step("dismiss team", lambda: pg.click("#team button[title=收起]"))
        self.ok("C21 dismissed", not await self.has("#team"))
        await self.step("close insp", lambda: pg.click(".insp-btn"))
        # team chip
        await self.step("team chip", lambda: pg.click(".team-chip"))
        self.ok("C10 团队 chip on", await self.has(".team-chip.on") and await self.has("#composer.team"))
        self.ok("C10 team placeholder + subtitle", await pg.get_attribute("#input", "placeholder") == "说一个任务，成员分工、组长汇总" and "团队模式" in await self.text("#chat-sub"))
        m = await self.mark()
        await pg.fill("#input", "做个调研")
        await self.step("team send", lambda: pg.press("#input", "Enter"))
        await self.expect("C10 team mode sends /team", "send", m, lambda a: a == {"thread": "group:g1", "text": "/team 做个调研"})
        await self.step("team chip off", lambda: pg.click(".team-chip"))
        self.ok("C10 团队 chip off", not await self.has(".team-chip.on") and await pg.get_attribute("#input", "placeholder") == "发消息到 dev")
        # @ picker
        await self.step("at", lambda: pg.fill("#input", "@"))
        items = await self.js("[...document.querySelectorAll('.pop.mention .pop-item .mi')].map((b) => b.lastChild.textContent)")
        self.ok("C19 @ lists members, guests, people and all", items == ["claude", "codex", "reviewer", "Amy", "所有人"], items)
        self.ok("C19 @ rows: agent avatar / person initial / @", await self.has(".pop.mention .pop-item .av") and await self.has(".pop.mention .mi-all.person") and await self.has(".pop.mention .mi-all:not(.person)"))
        await self.step("at filter", lambda: pg.fill("#input", "@co"))
        self.ok("C19 @ filters", await self.count(".pop.mention .pop-item") == 1)
        await self.step("at pick", lambda: pg.press("#input", "Enter"))
        self.ok("C19 ↩ picks the member", await pg.input_value("#input") == "@codex ")
        await pg.fill("#input", "@codex hi")
        m = await self.mark()
        await self.step("at send", lambda: pg.press("#input", "Enter"))
        await self.expect("C19 send with @", "send", m, lambda a: a == {"thread": "group:g1", "text": "@codex hi"})
        # / picker in a group
        await self.step("group slash", lambda: pg.fill("#input", "/"))
        items = await self.js("[...document.querySelectorAll('.pop.mention .pop-item b')].map((b) => b.textContent)")
        self.ok("C20 group commands", items == ["/help", "/stop", "/team"], items)
        await self.step("slash down", lambda: pg.press("#input", "ArrowDown"))
        await self.step("slash pick", lambda: pg.press("#input", "Enter"))
        self.ok("C20 ↑↓ + ↩ picks", await pg.input_value("#input") == "/stop ")
        await pg.fill("#input", "")
        # group page from the inspector
        await self.step("insp again", lambda: pg.click(".insp-btn"))
        await self.step("members page", lambda: pg.click("#insp button.li:has-text('群设置')"))
        self.ok("C8/D3 group settings page", await self.text("#pane h1") == "群设置" and await pg.input_value(".gname") == "dev")
        self.ok("D3 member tiles with round checks", await self.count(".ptile") == 5 and await self.count(".ptile.on") == 2)
        self.ok("D3 组长 tag", "claude" in await self.text(".ptile:has(.crown)"))

    async def group_pages(self):
        pg = self.pg
        # edit dev: set lead, save
        m = await self.mark()
        await self.step("make lead", lambda: pg.click(".ptile.on:has-text('codex') .mklead"))
        self.ok("D3 设为组长 moves the tag", "codex" in await self.text(".ptile:has(.crown)"))
        await self.step("save group", lambda: pg.click(".gpage .factions .btn.pri"))
        await self.expect("D3 save group", "save_group", m, lambda a: a.get("name") == "dev" and a.get("lead") == "codex" and a.get("guildId") == "1" and a.get("channelId") == "g1")
        # delete with confirm + undo
        await self.open("group:g1")
        await self.js("groupDialog(groupOf('group:g1'))")
        await self.settle()
        m = await self.mark()
        await self.step("delete group", lambda: pg.click(".gpage .btn.danger"))
        self.ok("D4 ask() confirm dialog", await self.wait("#dlg[open]") and "删除这个群聊" in await self.text("#dlg h3"))
        await self.step("confirm delete", lambda: pg.click("#dlg .btn.pri"))
        await self.expect("D3 delete group", "delete_group", m, lambda a: a == {"id": "g1"})
        self.ok("D4 toast with 撤销", await self.wait(".toast button:text-is('撤销')"))
        await self.step("undo delete", lambda: self.toast_click("撤销"))
        await self.expect("D3 undo restores the group", "restore_group", m, lambda a: a.get("id") == "g1" and a.get("name") == "dev")
        await pg.wait_for_function("S.thread === 'group:g1'")
        # new group
        await self.tab("chats")
        m = await self.mark()
        await self.step("new group", lambda: pg.click("#list-add"))
        self.ok("D3 Discord channel choice when online", await self.wait(".gpage .model-pill.choice:has-text('自动新建同名频道')"))
        await self.expect("D3 lists Discord channels", "discord_channels", m)
        await pg.fill(".gname", "研究组")
        await self.step("pick writer", lambda: pg.click(".ptile:has-text('writer')"))
        await self.step("pick researcher", lambda: pg.click(".ptile:has-text('researcher')"))
        self.ok("D3 member count", await self.text(".gpage .cnt") == "已选 2 个")
        await self.step("channel choice", lambda: pg.click(".gpage .model-pill.choice"))
        await self.step("pick channel", lambda: pg.click(".pop .pop-item:has-text('#general')"))
        await self.step("create", lambda: pg.click(".gpage .factions .btn.pri"))
        await self.expect("D3 create group", "save_group", m, lambda a: a.get("name") == "研究组" and a.get("members") == ["writer", "researcher"] and a.get("lead") == "writer" and a.get("channelId") == "c9")
        await pg.wait_for_function("S.thread && S.thread.startsWith('group:local-')")
        self.ok("D3 new group opens in 群聊", await self.js("S.tab") == "groups" and await self.text(".head h2") == "研究组")
        self.ok("C3 local group subtitle", "仅本地" in await self.text("#chat-sub"))

    async def peers(self):
        pg = self.pg
        await self.tab("chats")
        m = await self.mark()
        await self.step("open p1", lambda: pg.click("#items li:has(.name.route:has-text('小王'))"))
        await pg.wait_for_function("S.thread === 'peer:p1' && document.querySelector('#log')")
        await self.settle(ms=150)
        await self.expect("C17 peer_check when a friend's group chat opens", "peer_check", m, lambda a: a == {"id": "p1"})
        self.ok("C17 talkable peer: composer, no attach", await self.has("#composer #input") and not await self.has("#composer .attach") and not await self.has(".peer-note"))
        self.ok("C17 peer placeholder", await pg.get_attribute("#input", "placeholder") == "在 小王 的群里说话")
        self.ok("C3 peer subtitle", await self.text("#chat-sub") == "小王 的群 · 在用你的 claude")
        self.ok("peer header has no memory/to-do buttons", not await self.has(".mem-btn") and not await self.has(".space-btn"))
        if not await self.has("#insp"):
            await self.step("peer insp", lambda: pg.click(".insp-btn"))
        segs = await self.js("[...document.querySelectorAll('#insp .seg2 button')].map((b) => b.textContent.trim())")
        self.ok("inspector peer tabs: 信息 only", segs == ["信息"], segs)
        self.ok("inspector peer info", "可以说话" in await self.text("#insp-body"))
        await self.step("close insp", lambda: pg.click(".insp-btn"))
        # p2: watch-only until the agents event flips talk
        m = await self.mark()
        await self.open("peer:p2")
        self.ok("C17 watch-only note instead of composer", await self.has(".peer-note") and not await self.has("#composer"))
        self.ok("C17 note says what is missing", "还不能说话" in await self.text(".peer-note"))
        await self.js("Object.assign(STUB.peers[1], { talk: true, missing: '' })")
        await self.step("agents event (talk on)", lambda: self.fire("agents", None))
        self.ok("C17 live redraw: composer appears when talk flips on", await self.wait("#composer #input") and not await self.has(".peer-note"))
        self.ok("C17 live redraw toast", await self.has(".toast:has-text('可以在对方的群里说话了')"))
        self.ok("C17 still the same chat", await self.js("S.thread") == "peer:p2" and await self.count("#log") == 1)
        await self.js("Object.assign(STUB.peers[1], { talk: false, missing: '还不能说话：对方把 Bot 移出了服务器' })")
        await self.step("agents event (talk off)", lambda: self.fire("agents", None))
        self.ok("C17 live redraw: note returns when talk flips off", await self.wait(".peer-note") and not await self.has("#composer") and "移出了服务器" in await self.text(".peer-note"))
        # p3: invite-only, watch-only, no peer_check
        m = await self.mark()
        await self.open("peer:p3")
        self.ok("C17 invite-only watch note", "你现在只能旁观" in await self.text(".peer-note"))
        await self.no_call("C17 no peer_check for an invite-only peer", "peer_check", m)

    async def agents_tab(self):
        pg = self.pg
        m0 = await self.mark()
        await self.tab("agents")
        await self.expect("A1 detect agents", "detect_agents", m0)
        await self.expect("A1 conda envs", "conda_envs", m0)
        n = await self.js("STUB.agents.length")
        self.ok("L9 agent rows", await self.count("#items li.card") == n)
        self.ok("L9 官方 tag only on official templates", await self.count("#items li.card .tag:text-is('官方')") == 2)
        self.ok("L9 live dot on a running agent", await self.has("#items li.card:has(.name:text-is('codex')) .av .dot:not(.amber)"))
        self.ok("L9 subtitle with branch", await self.has("#items li.card:has(.name:text-is('codex')) .branch"))
        self.ok("L9 no 共享 on an A2A agent", not await self.has("#items li.card:has(.name:text-is('researcher')) button[title=共享给朋友]"))
        card = "#items li.card:has(.name:text-is('claude'))"
        m = await self.mark()
        await self.step("card restart", lambda: self.hover_click(card, f"{card} .acts button[title=重启]"))
        await self.expect("L9 hover 重启", "restart_agent", m, lambda a: a == {"name": "claude"})
        await self.step("card more", lambda: self.hover_click(card, f"{card} .acts button[title=更多]"))
        self.ok("L9 hover 更多 opens the agent menu", await self.has(".pop .pop-item:has-text('删除 Agent')"))
        await self.step("esc", lambda: pg.keyboard.press("Escape"))
        # share dialog
        await self.step("card share", lambda: self.hover_click(card, f"{card} .acts button[title=共享给朋友]"))
        await pg.wait_for_selector("#dlg[open]")
        self.ok("D1 share dialog", await self.text("#dlg h3") == "共享 claude")
        self.ok("D1 existing share listed", "需要时问我 · 今天 3/50 条" in await self.text("#dlg .share-row"))
        await pg.fill("#dlg input.fld", "老张")
        await self.step("mode ask", lambda: pg.click("#dlg .seg button:has-text('需要时问我')"))
        self.ok("D1 segmented control", await self.has("#dlg .seg button.on:has-text('需要时问我')"))
        await self.step("limit +", lambda: pg.click("#dlg .stepper button[title=增加]"))
        self.ok("D1 stepper", await pg.input_value("#dlg .stepper input") == "60")
        m = await self.mark()
        await self.step("create share", lambda: pg.click("#dlg button[type=submit]"))
        await self.expect("D1 create_share", "create_share", m, lambda a: a == {"agent": "claude", "guest": "老张", "mode": "ask", "limit": 60})
        self.ok("D1 invite code shown and copied", (await self.text("#dlg .invite code")).startswith("disco1:") and (await self.js("window.CLIP") or "").startswith("disco1:"))
        await self.step("copy invite", lambda: pg.click("#dlg .share-row button:text-is('复制邀请')"))
        await self.expect("D1 copy existing invite", "share_invite", m, lambda a: a == {"id": "s1"})
        await self.step("revoke", lambda: pg.click("#dlg .share-row button:text-is('撤销')"))
        await self.expect("D1 revoke", "revoke_share", m, lambda a: a == {"id": "s1"})
        await pg.wait_for_timeout(200)
        self.ok("D1 revoked row gone", not await self.has("#dlg .share-row"))
        await self.step("dialog keys", lambda: self.press("Mod+2"))
        self.ok("K1 an open dialog owns the keyboard", await self.js("S.tab") == "agents")
        await self.step("close share", lambda: pg.click("#dlg .actions .btn:text-is('关闭')"))
        self.ok("D1 dialog closes", not await self.has("#dlg[open]"))

    async def gallery(self):
        pg = self.pg
        await self.tab("agents")
        here = "这台电脑上" if self.plat == "win" else "这台 Mac 上"
        heads = await self.js("[...document.querySelectorAll('.page.add .gsec-h span:first-child')].map((s) => s.textContent)")
        self.ok("A2 detected section header", f"{here}检测到 4 个" in heads, heads)
        self.ok("A2 detected rows with 导入", await self.count(".page.add .arow .aplus") == 4)
        self.ok("A3 more ways rows", all([await self.has(f".page.add .arow .ahead:has-text('{t}')") for t in ["OpenAI 兼容模型", "其他本地 agent", "朋友的邀请码", "远程 A2A agent"]]))
        self.ok("A4 not-installed rows", await self.count(".page.add .tbtn:text-is('复制安装命令')") == 3)
        await self.step("copy install", lambda: pg.click(".page.add .arow:has-text('Gemini CLI') .tbtn"))
        self.ok("A4 copies the install command", await self.js("window.CLIP") == "npm i -g @google/gemini-cli")
        m = await self.mark()
        await self.step("import all", lambda: pg.click(".page.add .gsec-h .btn.pri:text-is('全部导入')"))
        await self.expect("A2 全部导入 imports the ones not added yet", "add_agent", m, lambda a: a["spec"]["template"] in ("hermes", "pi"), count=2)
        self.ok("A2 全部导入 skips agents already added", [h["spec"]["template"] for h in await self.calls("add_agent", m)] == ["hermes", "pi"])
        await pg.wait_for_selector(".page.add .gsec-h .btn:text-is('已全部导入')")
        self.ok("A2 button settles to 已全部导入", await pg.locator(".page.add .gsec-h .btn:text-is('已全部导入')").is_disabled())
        # quick import of one row
        m = await self.mark()
        await self.step("quick import", lambda: pg.click(".page.add .arow:has-text('Claude Code') .aplus"))
        await self.expect("A2 per-row 导入", "add_agent", m, lambda a: a["spec"]["template"] == "claude-code" and a["spec"]["name"] == "claude-code")
        await pg.wait_for_function("S.thread === 'dm:claude-code'")
        self.ok("A2 quick import opens the chat", await self.js("S.tab") == "chats")
        # full ACP form
        await self.tab("agents")
        await self.step("expand codex row", lambda: pg.click(".page.add .arow:has-text('Codex') .ahead"))
        f = ".page.add .arow.open .aform"
        self.ok("A5 form rows (avatar, name, folder, env, count)", all([await self.has(f"{f} .frow:has(span:text-is('{t}'))") for t in ["头像", "名称", "目录", "环境", "数量"]]))
        self.ok("A5 name defaults to the template", await pg.input_value(f"{f} input.fld") == "codex")
        await pg.fill(f"{f} input.fld", "codex-b")
        await self.step("env ml", lambda: pg.click(f"{f} .chip2:text-is('ml')"))
        self.ok("A5 conda env chip", await self.has(f"{f} .chip2.on:text-is('ml')"))
        m = await self.mark()
        await self.step("folder", lambda: pg.click(f"{f} .pill"))
        await self.expect("A5 folder picker", "choose_path", m)
        self.ok("A5 folder label", "my-agent" in await self.text(f"{f} .pill"))
        await self.step("count +", lambda: pg.click(f"{f} .stepper button[title=增加]"))
        await self.step("import form", lambda: pg.click(f"{f} .factions .btn.pri"))
        await self.expect("A5 ACP form → add_agent", "add_agent", m, lambda a: a["spec"]["name"] == "codex-b" and a["spec"]["count"] == 2 and a["spec"]["worktree"] is True
                          and a["spec"]["conda"] == "/opt/conda/envs/ml" and a["spec"]["cwd"] == "/Users/x/bin/my-agent")
        # OpenAI form with an avatar
        await self.tab("agents")
        await self.step("openai row", lambda: pg.click(".page.add .arow .ahead:has-text('OpenAI 兼容模型')"))
        await self.step("preset", lambda: pg.click(f"{f} .chip2:text-is('DeepSeek')"))
        self.ok("A5 preset fills url/model/name", await pg.input_value(f"{f} input[placeholder^='Base URL']") == "https://api.deepseek.com/v1"
                and await pg.input_value(f"{f} input[placeholder^='模型']") == "deepseek-chat" and await pg.input_value(f"{f} input.fld >> nth=0") == "deepseek")
        await pg.fill(f"{f} input[type=password]", "sk-test")
        async with pg.expect_file_chooser() as fc:
            await pg.click(f"{f} .linkbtn.acc")
        await (await fc.value).set_files(LOGO)
        self.ok("A5 avatar preview in the form", await self.wait(f"{f} .avslot img"))
        m = await self.mark()
        await self.step("openai add", lambda: pg.click(f"{f} .factions .btn.pri"))
        await self.expect("A5 OpenAI form → add_agent", "add_agent", m, lambda a: a["spec"]["template"] == "openai" and a["spec"]["url"] == "https://api.deepseek.com/v1"
                          and a["spec"]["model"] == "deepseek-chat" and a["spec"]["headerValue"] == "sk-test")
        await self.expect("C1 form avatar → set_avatar for the new agent", "set_avatar", m, lambda a: a.get("agent") == "deepseek" and str(a.get("data", "")).startswith("data:image/png"))
        # command form
        await self.tab("agents")
        await self.step("command row", lambda: pg.click(".page.add .arow .ahead:has-text('其他本地 agent')"))
        m = await self.mark()
        await self.step("choose program", lambda: pg.click(f"{f} .btn.sm:text-is('选择…')"))
        await self.expect("A5 program picker", "choose_path", m, lambda a: a == {"file": True})
        self.ok("A5 program path filled", await pg.input_value(f"{f} input[placeholder^='/path']") == "/Users/x/bin/my-agent")
        # A2A form
        await self.step("a2a row", lambda: pg.click(".page.add .arow .ahead:has-text('远程 A2A agent')"))
        self.ok("A3 only one row open at a time", await self.count(".page.add .arow.open") == 1)
        await pg.fill(f"{f} input.fld >> nth=0", "remote")
        await pg.fill(f"{f} input[placeholder^='https://agent']", "https://agent.example.com")
        await pg.fill(f"{f} input[placeholder^='鉴权头']", "authorization")
        m = await self.mark()
        await self.step("a2a add", lambda: pg.click(f"{f} .factions .btn.pri"))
        await self.expect("A5 A2A form → add_agent", "add_agent", m, lambda a: a["spec"]["template"] == "a2a" and a["spec"]["url"] == "https://agent.example.com" and a["spec"]["headerName"] == "authorization")
        # invite form
        await self.tab("agents")
        await self.step("invite row", lambda: pg.click(".page.add .arow .ahead:has-text('朋友的邀请码')"))
        await pg.fill(f"{f} textarea.code", "disco1:abc")
        await pg.fill(f"{f} input.fld", "friend-bot")
        m = await self.mark()
        await self.step("accept invite", lambda: pg.click(f"{f} .factions .btn.pri"))
        await self.expect("A6 invite code", "accept_invite", m, lambda a: a == {"code": "disco1:abc", "name": "friend-bot"})
        await pg.wait_for_function("S.thread === 'dm:friend-bot'")
        self.ok("A6 invite opens the new chat", await self.text(".head h2") == "friend-bot")

    async def settings(self):
        pg = self.pg
        m0 = await self.mark()
        await self.tab("settings")
        self.ok("G sections", all([await self.has(f"#set-{s}") for s in ["dc", "net", "mem", "run", "gen"]]))
        navs = await self.js("[...document.querySelectorAll('#items li.nav .name')].map((n) => n.textContent)")
        self.ok("L10 settings nav rows", navs == ["Discord", "共享与 Tailscale", "记忆", "运行", "通用与更新"], navs)
        self.ok("L10 Discord row shows live status", "在线 · MyBot#0001" in await self.text("#items li.nav >> nth=0"))
        await self.step("nav mem", lambda: pg.click("#items li.nav:has-text('记忆')"))
        self.ok("L10 nav selects + scrolls", await self.js("S.setSec") == "mem" and await self.has("#items li.nav.on:has-text('记忆')"))
        # Discord
        self.ok("G1 status", await self.has("#dc-status.ok") and "在线" in await self.text("#dc-status"))
        await self.expect("G1 discord settings loaded", "discord_settings", m0)
        self.ok("G1 home guild picker", await self.wait("#pane .model-pill.choice:has-text('My Server')"))
        m = await self.mark()
        await self.step("sync", lambda: pg.click("#pane button:text-is('同步频道')"))
        await self.expect("G1 同步频道", "discord_sync", m)
        await self.step("reauth", lambda: pg.click("#pane button:text-is('重新授权 Bot')"))
        await self.expect("G1 重新授权", "open_url", m, lambda a: a == {"url": "https://discord.com/oauth2/x"})
        await self.step("copy bot invite", lambda: pg.click("#pane button:has-text('复制邀请链接')"))
        self.ok("G1 copy invite link", await self.js("window.CLIP") == "https://discord.com/oauth2/x")
        await pg.fill("#pane input[placeholder^='Discord 用户 ID']", "123,456")
        await self.step("allow save", lambda: pg.click("#pane .grow-row:has(input[placeholder^='Discord 用户 ID']) button:text-is('保存')"))
        await self.expect("G1 allow-list", "discord_settings", m, lambda a: a == {"allow": "123,456"})
        await self.step("reconnect", lambda: pg.click("#pane button:text-is('重新连接')"))
        await self.expect("G1 reconnect", "discord_connect", m)
        await self.step("discord-status", lambda: self.fire("discord-status", "连接中…"))
        self.ok("G1 live status patch", await self.text("#dc-status") == "连接中…" and not await self.has("#dc-status.ok"))
        self.ok("G1 status row in list follows", "连接中…" in await self.text("#items li.nav >> nth=0"))
        await pg.fill("#pane input[name=token]", "tok.123")
        await self.step("token save", lambda: pg.click("#pane form.inline button"))
        await self.expect("G1 token save", "save_discord_token", m, lambda a: a == {"token": "tok.123"})
        # Tailscale rules
        await pg.wait_for_selector("#set-net .seg button")
        self.ok("G2 rules loaded", await self.has("#set-net .seg button.on:has-text('整个 Tailscale 网络')") and "old-laptop" in await pg.input_value("#set-net textarea.rules >> nth=1"))
        await self.step("allowlist", lambda: pg.click("#set-net .seg button:has-text('仅允许名单')"))
        self.ok("G2 allow list row appears", await pg.locator("#set-net .grow-row.col:has(.gl:text-is('允许名单'))").is_visible())
        await pg.fill("#set-net textarea.rules >> nth=0", "my-phone\ntag:friends")
        await self.step("quiet start", lambda: pg.click("#set-net .model-pill.choice >> nth=0"))
        self.ok("G2 time picker (long list gets a filter)", await self.has(".pop .pop-filter"))
        await self.step("pick 22:30", lambda: pg.click(".pop .pop-item:has(.lt:text-is('22:30'))"))
        m = await self.mark()
        await self.step("save rules", lambda: pg.click("#set-net button:text-is('保存规则')"))
        await self.expect("G2 save rules", "net_policy_set", m, lambda a: a["policy"]["mode"] == "allowlist" and a["policy"]["allow"] == ["my-phone", "tag:friends"] and a["policy"]["quiet_start"] == "22:30" and a["policy"]["quiet_end"] == "08:00")
        # memories
        self.ok("G3 memories listed", await self.count("#set-mem .grow-row.mem") == 2 and "所有 agent · 偏好" in await self.text("#set-mem .grow-row.mem >> nth=0") and "dev · 项目" in await self.text("#set-mem .grow-row.mem >> nth=1"))
        await self.step("forget", lambda: pg.click("#set-mem .grow-row.mem >> nth=0 >> button"))
        await self.expect("G3 delete memory", "forget", m, lambda a: a == {"key": "k1"})
        await self.step("forget undo", lambda: self.toast_click("撤销"))
        await self.expect("G3 undo", "memory_restore", m, lambda a: a == {"key": "k1"})
        # run
        self.ok("G6 run state", await self.text("#run-state") == "运行中")
        await self.step("kill 1", lambda: pg.click("#kill"))
        self.ok("R4 #kill arms on the first click", await self.text("#kill") == "再点一次停止")
        await self.no_call("R4 #kill first click", "stop_all", m)
        await self.step("kill 2", lambda: pg.click("#kill"))
        await self.expect("R4 #kill second click", "stop_all", m)
        await pg.wait_for_function("document.querySelector('#kill').textContent === '恢复'")
        self.ok("R4 paused: run state + 恢复", await self.text("#run-state") == "已全部停止" and "已全部停止" in await self.text("#items li.nav:has-text('运行')"))
        await self.step("kill resume", lambda: pg.click("#kill"))
        await self.expect("R4 resume from #kill", "resume", m)
        # general
        self.ok("G5 version line", await self.wait("#set-gen .val:has-text('已是最新版本')") and "1.5.14" in await self.text("#set-gen .val"))
        await self.step("diagnose", lambda: pg.click("#set-gen button:text-is('一键诊断')"))
        await self.expect("G4 一键诊断", "diagnose", m)
        await pg.wait_for_selector(".checks .grow-row")
        self.ok("G4 ✓/✗ rows", await self.count(".checks .grow-row.ok .ci") == 1 and await self.count(".checks .grow-row.bad .ci") == 1 and await self.has("#pane button:text-is('重新检查')"))
        await self.js("STUB.update.version = '1.6.0'")
        await self.step("settings again", lambda: self.js("renderSettings()"))
        await pg.wait_for_selector("#set-gen .val:has-text('1.6.0')")
        m = await self.mark()
        await self.step("update", lambda: pg.click("#set-gen button:text-is('更新并重启')"))
        await self.expect("G5 更新并重启", "install_update", m)

    async def events(self):
        pg = self.pg
        await self.tab("chats")
        await self.step("update-available", lambda: self.fire("update-available", {"version": "1.7.0"}))
        self.ok("L5 update bar", "1.7.0" in await self.text("#update-bar"))
        m = await self.mark()
        await self.step("update bar click", lambda: pg.click("#update-bar button"))
        await self.expect("L5 更新并重启", "install_update", m)
        # zoom is blocked
        prevented = await self.js("""(() => { const w = new WheelEvent('wheel', { ctrlKey: true, cancelable: true, bubbles: true, deltaY: -10 }); document.body.dispatchEvent(w);
          const k = new KeyboardEvent('keydown', { key: '=', ctrlKey: true, metaKey: true, cancelable: true, bubbles: true }); document.body.dispatchEvent(k);
          return [w.defaultPrevented, k.defaultPrevented]; })()""")
        self.ok("K1 zoom wheel/keys blocked", prevented == [True, True], prevented)
        # a deleted agent's chat closes
        await self.open("dm:writer")
        await self.js("STUB.agents = STUB.agents.filter((a) => a.name !== 'writer')")
        await self.step("agents event (removed)", lambda: self.fire("agents", None))
        await pg.wait_for_timeout(150)
        self.ok("E1 a removed agent's open chat closes", await self.js("S.thread") is None and await self.has("#pane .blank"))

    async def remove_agent(self):
        pg = self.pg
        await self.tab("agents")
        card = "#items li.card:has(.name:text-is('researcher'))"
        await self.step("researcher more", lambda: self.hover_click(card, f"{card} .acts button[title=更多]"))
        await self.step("menu delete", lambda: pg.click(".pop .pop-item:has-text('删除 Agent')"))
        self.ok("D2 remove dialog with purge toggle on", await self.text("#dlg h3") == "删除 researcher？" and await pg.locator("#dlg input.tgl").is_checked())
        m = await self.mark()
        await self.step("confirm remove", lambda: pg.click("#dlg .btn.danger-fill"))
        await self.expect("D2 remove agent (+ purge)", "remove_agent", m, lambda a: a == {"name": "researcher", "purge": True})
        await pg.wait_for_timeout(200)
        self.ok("D2 row gone", not await self.has(card))

    async def clickrace(self):
        """L12: rows redraw every 5 ms (busy agents); a press must still land on the row under it."""
        pg = self.pg
        await self.tab("chats")
        await self.js("""window.__t = setInterval(() => { __L.typing({ payload: { thread: 'dm:claude', agent: 'claude', on: Math.random() < .5 } });
          __L.message({ payload: { thread: 'dm:helper2', author: 'helper2', body: 'x' + Math.random(), ts: Date.now() / 1e3 } }); }, 5)""")
        landed, N = 0, 10
        for _ in range(N):
            await self.js("S.thread = null")
            box = await self.js("""(() => { const li = [...document.querySelectorAll('#items li')].find((l) => l.querySelector('.name')?.textContent === 'codex');
              li.scrollIntoView({ block: 'nearest' }); const r = li.getBoundingClientRect(); return { x: r.x, y: r.y, h: r.height }; })()""")
            await pg.mouse.move(box["x"] + 40, box["y"] + box["h"] / 2)
            await pg.mouse.down()
            await pg.wait_for_timeout(120)
            await pg.mouse.up()
            await pg.wait_for_timeout(80)
            landed += await self.js("S.thread") == "dm:codex"
        await self.js("clearInterval(__t)")
        await self.settle(ms=150)
        self.ok(f"L12 clicks land during redraw storms ({landed}/{N})", landed >= N - 1, f"{landed}/{N}")
        self.ok("L12 list hold released", await self.js("listHeld") is False)
        for a in ["claude"]:
            await self.fire("typing", {"thread": "dm:claude", "agent": a, "on": False})

    async def early_events(self):
        """E1: events that arrive before the first overview wait for it; a stale overview never lands."""
        init = """
let T; Object.defineProperty(window, '__TAURI__', { configurable: true, get: () => T, set: (v) => { T = v; const inv = v.core.invoke; let n = 0;
  v.core.invoke = async (c, a) => { if (c === 'overview') { const k = n++; const r = await inv(c, a); await new Promise((s) => setTimeout(s, k === 0 ? 300 : 50)); if (k === 0) r.agents = r.agents.slice(0, 1); return r; } return inv(c, a); } } });
addEventListener('DOMContentLoaded', () => setTimeout(() => { __L.message({ payload: { thread: 'dm:claude', author: 'claude', body: 'early', ts: 9e9 } });
  __L.typing({ payload: { thread: 'dm:codex', agent: 'codex', on: true } }); __L.inbox({ payload: [] }); __L.agents({ payload: null }); openThread('dm:claude'); }, 10));
"""
        pg = await self.new_page(init, ready="true")
        await pg.wait_for_timeout(900)
        r = await pg.evaluate("[S.ov.agents.length, S.thread, document.querySelector('#inbox-badge').textContent]")
        self.ok("E1 early events wait for the first overview; stale load ignored", r[0] == 5 and r[1] == "dm:claude", r)
        self.ok("E1 a pushed inbox beats the startup fetch", r[2] == "", r)
        bad = await pg.evaluate(STRAY_JS)
        self.ok("no stray text (early events page)", not bad, bad[:3])
        await pg.context.close()

    async def first_run(self):
        pg = await self.new_page("window.STUB_INIT = { agents: [], groups: [], heads: [], peers: [], inbox: [] };")
        await pg.wait_for_selector(".blank .frun")
        self.ok("B2 first run lists found agents", await pg.locator(".blank .frun").count() == 4 and "找到这些 agent" in await pg.locator(".blank p").first.inner_text())
        self.ok("L11 empty chats list", await pg.locator("#items li.empty").count() == 1)
        m = await pg.evaluate("CALLS.length")
        await pg.click(".blank .frun:has-text('Claude Code') button")
        await pg.wait_for_function("S.thread === 'dm:claude-code'")
        got = await pg.evaluate("(m) => CALLS.slice(m).filter((c) => c[0] === 'add_agent').map((c) => c[1].spec.template)", m)
        self.ok("B2 开始聊天 imports and opens", got == ["claude-code"], got)
        bad = await pg.evaluate(STRAY_JS)
        self.ok("no stray text (first run page)", not bad, bad[:3])
        await pg.context.close()
        pg = await self.new_page("window.STUB_INIT = { agents: [], groups: [], heads: [], peers: [], inbox: [], detect: [{ id: 'gemini', name: 'Gemini CLI', found: null }] };")
        await pg.wait_for_selector(".blank button:text-is('手动添加 agent')")
        await pg.click(".blank button:text-is('手动添加 agent')")
        await pg.wait_for_selector(".page.add")
        self.ok("B2 nothing found → 手动添加 opens the Agent tab", await pg.evaluate("S.tab") == "agents")
        await pg.goto("about:blank")
        await pg.context.close()

    async def run(self):
        self.pg = await self.new_page()
        for suite in [self.boot, self.rail, self.list_column, self.inbox_tab, self.dm_chat, self.dm_codex, self.composer, self.group_chat,
                      self.group_pages, self.peers, self.agents_tab, self.gallery, self.settings, self.events, self.clickrace, self.remove_agent]:
            if self.verbose:
                print(f"[{self.label}] {suite.__name__}")
            try:
                await suite()
            except Exception as e:  # noqa: BLE001  one broken suite must not hide the rest
                self.fails.append(f"[{self.label}] suite {suite.__name__} aborted: {str(e).splitlines()[0][:300]}")
                await self.js("document.querySelector('#dlg').open && document.querySelector('#dlg').close()")
        unknown = await self.js("[...new Set(UNKNOWN)]")
        self.ok("every invoke is known to the stub", not unknown, unknown)
        await self.early_events()
        await self.first_run()


async def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--base", help="static server rooted at the repo (default: serve it here on a free port)")
    ap.add_argument("--chromium", default=os.environ.get("CHROMIUM"), help="Chromium executable (default: Playwright's)")
    ap.add_argument("--only", help="run one combination, e.g. dark-win")
    ap.add_argument("-v", "--verbose", action="store_true")
    args = ap.parse_args()
    srv = None
    base = args.base
    if not base:
        srv, base = serve(ROOT)
    total, fails = 0, []
    t0 = time.monotonic()
    async with async_playwright() as p:
        browser = await p.chromium.launch(executable_path=args.chromium or None, args=["--no-sandbox"])
        for theme, plat in COMBOS:
            if args.only and args.only != f"{theme}-{plat}":
                continue
            a = Audit(browser, base.rstrip("/"), theme, plat, args.verbose)
            await a.run()
            total += a.n
            fails += a.fails
            print(f"{a.label:11s} {a.n:4d} assertions, {len(a.fails)} failures")
        await browser.close()
    if srv:
        srv.shutdown()
    print(f"\n{total} assertions in {time.monotonic() - t0:.1f}s")
    if fails:
        print(f"FAILED ({len(fails)}):")
        for f in fails:
            print("  " + f)
        sys.exit(1)
    print("UI audit passed")


if __name__ == "__main__":
    asyncio.run(main())
