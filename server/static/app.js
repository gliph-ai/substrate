// Substrate's only script: live refresh, copy buttons, confirmations.
// The pages work without it.

// Live: a page with data-live="/path" listens for server-sent events and
// reloads its <main> when something changes, unless the reader is typing.
(function () {
  const el = document.querySelector("[data-live]");
  if (!el || !window.EventSource) return;
  let busy = false;
  const es = new EventSource(el.getAttribute("data-live"));
  es.onmessage = async () => {
    if (busy) return;
    const a = document.activeElement;
    if (a && (a.tagName === "TEXTAREA" || a.tagName === "INPUT")) return;
    busy = true;
    try {
      const r = await fetch(location.href, { headers: { "X-Live": "1" } });
      if (r.ok) {
        const doc = new DOMParser().parseFromString(await r.text(), "text/html");
        const next = doc.querySelector("main");
        const cur = document.querySelector("main");
        if (next && cur) cur.replaceWith(next);
      }
    } finally {
      busy = false;
    }
  };
})();

document.addEventListener("click", (e) => {
  const c = e.target.closest("[data-copy]");
  if (c) {
    navigator.clipboard?.writeText(c.getAttribute("data-copy"));
    // Only the label changes; the icon before it stays.
    const label = c.lastChild;
    const t = label.textContent;
    label.textContent = "Copied";
    setTimeout(() => (label.textContent = t), 1400);
  }
});

document.addEventListener("submit", (e) => {
  const msg = e.target.getAttribute("data-confirm");
  if (msg && !confirm(msg)) e.preventDefault();
});

// The orb: a fluid blob. Each frame, its outline is the sum of a few slow
// waves around a circle, applied as a clip-path. Without the script it is a
// plain round orb; with reduced motion it holds one blob shape.
(function () {
  const waves = [[2, 0.13, 0.8, 0], [3, 0.09, -1.1, 1.7], [4, 0.05, 1.5, 3.1], [5, 0.03, -2.1, 0.6]];
  const still = matchMedia("(prefers-reduced-motion: reduce)").matches;
  const shape = (w, h, t, seed) => {
    let d = "";
    for (let i = 0; i < 72; i++) {
      const a = (i / 72) * 2 * Math.PI;
      let r = 1;
      for (const [k, amp, speed, phase] of waves) r += amp * Math.sin(k * a + speed * t + phase + seed * k);
      r *= 0.76;
      d += (i ? "L" : "M") + (w / 2 + (Math.cos(a) * r * w) / 2).toFixed(1) + " " + (h / 2 + (Math.sin(a) * r * h) / 2).toFixed(1);
    }
    return `path("${d}Z")`;
  };
  const frame = (ms) => {
    document.querySelectorAll(".orb").forEach((o, i) => {
      const w = o.offsetWidth, h = o.offsetHeight;
      if (w) o.style.clipPath = shape(w, h, still ? 0 : ms / 1000, i * 1.3);
    });
    if (!still) requestAnimationFrame(frame);
  };
  requestAnimationFrame(frame);
})();

// Chat: the tester's web chat (#chat). It calls the same MCP tools an
// assistant uses, at data-rpc. Every wire value goes in with textContent.
(function () {
  const root = document.getElementById("chat");
  if (!root) return;
  const $ = (id) => document.getElementById(id);
  const el = (tag, cls, text) => { const e = document.createElement(tag); if (cls) e.className = cls; if (text != null) e.textContent = text; return e; };
  const S = { projects: [], pid: null, cid: null, mid: null, seq: 0, next: "", seen: new Set(), gen: 0 };
  let rid = 0;

  async function call(name, args) {
    const r = await fetch(root.dataset.rpc, {
      method: "POST",
      headers: { "Content-Type": "application/json", "X-CSRF": root.dataset.csrf || "" },
      body: JSON.stringify({ jsonrpc: "2.0", id: ++rid, method: "tools/call", params: { name, arguments: args || {} } }),
    });
    const j = await r.json().catch(() => ({}));
    const res = j.result || {};
    if (!r.ok || j.error || res.isError) throw new Error((res.content && res.content[0] && res.content[0].text) || (j.error && j.error.message) || j.error || "Something went wrong. Try again.");
    return res.structuredContent || {};
  }

  const project = () => S.projects.find((p) => p.project_id === S.pid);
  const available = (p) => p && p.agent_online;
  function pill(p) {
    if (!available(p)) return el("span", "tag", "offline");
    if (p.agent_state === "working") return el("span", "tag blue", "working");
    if (p.agent_state === "blocked") return el("span", "tag warn", "busy");
    return el("span", "tag ok", "free");
  }
  function icon(p) {
    if (p.icon_url) { const i = el("img", "picon"); i.src = p.icon_url; i.alt = ""; i.width = i.height = 22; return i; }
    const t = el("span", "picon tile", (p.name || "?").charAt(0).toUpperCase()); t.style.cssText = "width:22px;height:22px;font-size:10px"; return t;
  }

  function sidebar(convs) {
    const box = $("chat-projects"); box.replaceChildren();
    for (const p of S.projects) {
      const b = el("button", "chat-item" + (p.project_id === S.pid ? " on" : ""));
      b.type = "button";
      b.append(icon(p), el("span", "grow", p.name), pill(p));
      b.onclick = () => { S.pid = p.project_id; fresh(); };
      box.append(b);
    }
    if (!S.projects.length) box.append(el("p", "faint", "No projects yet. Ask the developer for an invite."));
    if (convs) {
      const c = $("chat-convs"); c.replaceChildren();
      for (const v of convs) {
        const b = el("button", "chat-item" + (v.conversation_id === S.cid ? " on" : ""));
        b.type = "button";
        b.append(el("span", "grow", v.title || "Conversation"), el("span", "faint", v.project));
        b.onclick = () => open(v.conversation_id);
        c.append(b);
      }
    }
  }

  function head(summary) {
    const p = project(), h = $("chat-head"); h.replaceChildren();
    if (!p) return;
    const t = el("div", "row"); t.append(icon(p), el("strong", null, p.name), el("span", "faint", (p.agent || "agent") + " · " + p.developer), pill(p));
    h.append(t);
    if (summary) h.append(el("div", "muted chat-sum", summary));
    const ban = $("chat-banner"), btn = $("chat-send").querySelector("span");
    if (available(p)) { ban.hidden = true; btn.textContent = S.next === "answer_question" ? "Answer" : "Send"; }
    else { ban.hidden = false; ban.textContent = "The agent is offline. A message waits until the developer starts it again."; btn.textContent = "Send anyway"; }
    $("chat-text").placeholder = S.next === "answer_question" ? "Answer the agent's question" : "Message " + (p.agent || "the agent");
  }

  function add(events) {
    const log = $("chat-log");
    const empty = log.querySelector(".chat-empty"); if (empty && events.length) empty.remove();
    for (const e of events || []) {
      if (S.seen.has(e.seq)) continue;
      S.seen.add(e.seq);
      let n;
      if (e.actor === "human") n = el("div", "msg me", e.text);
      else if (e.actor === "agent") {
        n = el("div", "msg agent");
        n.append(el("div", "who", e.name + (e.kind === "question" ? " asks" : "")), el("div", null, e.agent_text != null ? e.agent_text : "The developer did not share replies on this link."));
      } else if (e.actor === "deploy") n = el("div", "note", "Deploy: " + (e.agent_text || ""));
      else n = el("div", "note", e.text || e.kind);
      log.append(n);
    }
    log.scrollTop = log.scrollHeight;
  }

  function block(b) {
    if (!b) return;
    S.mid = b.message_id; S.cid = b.conversation_id; S.next = b.next_action || "";
    add(b.new_events); S.seq = Math.max(S.seq, b.seq || 0);
    head(b.summary);
  }

  async function watch() {
    const g = ++S.gen;
    while (g === S.gen && S.mid) {
      try {
        const b = await call("wait_for_update", { message_id: S.mid, since_seq: S.seq, timeout_seconds: 25 });
        if (g !== S.gen) return;
        block(b);
      } catch (e) { await new Promise((r) => setTimeout(r, 5000)); }
    }
  }

  function fresh() {
    S.gen++; S.cid = S.mid = null; S.seq = 0; S.next = ""; S.seen = new Set();
    history.replaceState(null, "", location.pathname);
    const log = $("chat-log"), e = el("div", "chat-empty");
    e.append(el("h2", null, "What do you want to tell " + ((project() || {}).name || "the developer") + "?"), el("p", "muted", "Report a bug, ask a question or request a change. The developer sees every message."));
    log.replaceChildren(e);
    sidebar(); head(); $("chat-text").focus();
  }

  async function open(cid) {
    S.gen++; S.seen = new Set(); S.seq = 0; $("chat-log").replaceChildren();
    try {
      const c = await call("get_conversation", { conversation_id: cid });
      const p = S.projects.find((x) => x.name === c.project); if (p) S.pid = p.project_id;
      block(c.latest); sidebar(); watch();
      history.replaceState(null, "", "#" + cid);
    } catch (e) { $("chat-log").append(el("div", "note", e.message)); }
  }

  async function reload() {
    S.projects = (await call("list_projects")).projects || [];
    if (!S.pid && S.projects[0]) S.pid = S.projects[0].project_id;
    let convs = [];
    try { convs = (await call("get_conversation")).conversations || []; } catch (e) {}
    sidebar(convs); head();
  }

  $("chat-new").onclick = fresh;
  const ta = $("chat-text");
  ta.addEventListener("input", () => { ta.style.height = "auto"; ta.style.height = Math.min(ta.scrollHeight, 220) + "px"; });
  ta.addEventListener("keydown", (e) => { if (e.key === "Enter" && !e.shiftKey && !e.isComposing) { e.preventDefault(); $("chat-form").requestSubmit(); } });
  $("chat-form").addEventListener("submit", async (e) => {
    e.preventDefault();
    const text = ta.value.trim(); if (!text || !S.pid) return;
    const btn = $("chat-send"); btn.disabled = true;
    try {
      const b = S.next === "answer_question"
        ? await call("reply", { message_id: S.mid, text })
        : await call("send_message", { project_id: S.pid, text, conversation_id: S.cid || undefined, send_anyway: !available(project()), idempotency_key: crypto.randomUUID() });
      ta.value = ""; ta.style.height = "auto";
      block(b); watch(); reload();
      history.replaceState(null, "", "#" + S.cid);
    } catch (err) {
      const ban = $("chat-banner"); ban.hidden = false; ban.textContent = err.message;
    } finally { btn.disabled = false; }
  });
  reload().then(() => { if (location.hash.startsWith("#c_")) open(location.hash.slice(1)); })
    .catch((e) => add([{ seq: -1, actor: "system", text: e.message }]));
})();
