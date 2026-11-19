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

  // Text to nodes: ``` fences, `code`, **bold** and http(s) links. No innerHTML.
  function inline(text, into) {
    const re = /(`[^`\n]+`|\*\*[^*\n]+\*\*|https?:\/\/[^\s<>"'`)\]]+)/g;
    let last = 0, m;
    while ((m = re.exec(text))) {
      if (m.index > last) into.append(text.slice(last, m.index));
      const t = m[0];
      if (t[0] === "`") into.append(el("code", null, t.slice(1, -1)));
      else if (t[0] === "*") into.append(el("strong", null, t.slice(2, -2)));
      else { const a = el("a", null, t); a.href = t; a.target = "_blank"; a.rel = "noopener noreferrer"; into.append(a); }
      last = m.index + t.length;
    }
    into.append(text.slice(last));
  }
  function rich(text) {
    const box = el("div", "rich");
    String(text || "").split(/```/).forEach((part, i) => {
      if (i % 2) { const pre = el("pre"); pre.append(el("code", null, part.replace(/^[\w-]*\n/, "").replace(/\n$/, ""))); box.append(pre); }
      else if (part) { const d = el("div"); inline(part, d); box.append(d); }
    });
    return box;
  }
  function files(list) {
    const box = el("div", "files");
    for (const f of list || []) {
      if (!f.url) continue;
      const a = el("a", (f.type || "").startsWith("image/") ? "thumb" : "file"); a.href = f.url; a.target = "_blank"; a.rel = "noopener noreferrer";
      if (a.className === "thumb") { const i = el("img"); i.src = f.url; i.alt = f.name; i.loading = "lazy"; a.append(i); }
      else a.append(el("span", null, f.name));
      box.append(a);
    }
    return box;
  }

  const project = () => S.projects.find((p) => p.project_id === S.pid);
  // The developer by name: "waseem" -> "Waseem". Server notes say "the developer".
  const dev = () => { const d = (project() || {}).developer || ""; return d ? d.charAt(0).toUpperCase() + d.slice(1) : "the developer"; };
  const named = (t) => String(t || "").replace(/\b[Tt]he developer\b/g, dev());
  const LOGOS = [["claude", "claude"], ["codex", "openai"], ["openai", "openai"], ["gemini", "gemini"], ["cursor", "cursor"], ["copilot", "copilot"]];
  function agentLogo(name) {
    const hit = LOGOS.find(([k]) => String(name || "").toLowerCase().includes(k));
    if (!hit) return null;
    const i = el("img", "logo"); i.src = "/static/logos/" + hit[1] + ".svg"; i.alt = ""; i.width = i.height = 16; return i;
  }
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
    const t = el("div", "row"); t.append(icon(p), el("strong", null, p.name));
    const lg = agentLogo(p.agent); if (lg) t.append(lg);
    t.append(el("span", "faint", (p.agent || "agent") + " · " + dev()), pill(p));
    h.append(t);
    if (summary) h.append(el("div", "muted chat-sum", named(summary)));
    const ban = $("chat-banner"), btn = $("chat-send").querySelector("span");
    if (available(p)) { ban.hidden = true; btn.textContent = S.next === "answer_question" ? "Answer" : "Send"; }
    else { ban.hidden = false; ban.textContent = "The agent is offline. A message waits until " + dev() + " starts it again."; btn.textContent = "Send anyway"; }
    $("chat-text").placeholder = S.next === "answer_question" ? "Answer the agent's question" : "Message " + (p.agent || "the agent");
  }

  function add(events) {
    const log = $("chat-log");
    const empty = log.querySelector(".chat-empty"); if (empty && events.length) empty.remove();
    for (const e of events || []) {
      if (S.seen.has(e.seq)) continue;
      S.seen.add(e.seq);
      let n;
      if (e.actor === "human") { n = el("div", "msg me"); n.append(rich(e.text), files(e.attachments)); }
      else if (e.actor === "agent") {
        n = el("div", "msg agent");
        const who = el("div", "who"), lg = agentLogo(e.name); if (lg) who.append(lg);
        who.append(e.name + (e.kind === "question" ? " asks" : ""));
        n.append(who, rich(e.agent_text), files(e.attachments));
      } else if (e.actor === "deploy") n = el("div", "note", "Deploy: " + (e.agent_text || ""));
      else n = el("div", "note", named(e.text || e.kind));
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
    const p = project(), big = p ? icon(p) : null;
    if (big) { big.style.width = big.style.height = "64px"; big.style.fontSize = "28px"; big.style.borderRadius = "18px"; e.append(big); }
    e.append(el("h2", null, p ? "Talk to " + dev() + "'s " + (p.agent || "agent") + " on " + p.name + "." : "Talk to the developer's agent."),
      el("p", "muted", "Report a bug, ask a question or request a change. " + dev().charAt(0).toUpperCase() + dev().slice(1) + " sees every message."));
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

  const pending = [];
  function addFiles(list) {
    for (const f of list) {
      if (pending.length >= 5) break;
      if (f.size > 10 * 1024 * 1024) { const b = $("chat-banner"); b.hidden = false; b.textContent = f.name + " is over 10 MB."; continue; }
      pending.push(f);
    }
    chips();
  }
  function chips() {
    const box = $("chat-files"); box.replaceChildren();
    pending.forEach((f, i) => {
      const c = el("span", "chip", f.name), x = el("button", null, "×"); x.type = "button"; x.setAttribute("aria-label", "Remove " + f.name);
      x.onclick = () => { pending.splice(i, 1); chips(); };
      c.append(x); box.append(c);
    });
    box.hidden = !pending.length;
  }
  const b64 = (f) => new Promise((ok, no) => { const r = new FileReader(); r.onload = () => ok(String(r.result).split(",")[1]); r.onerror = no; r.readAsDataURL(f); });
  $("chat-attach").onclick = () => $("chat-file").click();
  $("chat-file").onchange = (e) => { addFiles(e.target.files); e.target.value = ""; };
  document.addEventListener("paste", (e) => { const f = [...(e.clipboardData || {}).files || []]; if (f.length) { e.preventDefault(); addFiles(f); } });
  root.addEventListener("dragover", (e) => e.preventDefault());
  root.addEventListener("drop", (e) => { e.preventDefault(); addFiles(e.dataTransfer.files); });

  $("chat-new").onclick = fresh;
  const ta = $("chat-text");
  ta.addEventListener("input", () => { ta.style.height = "auto"; ta.style.height = Math.min(ta.scrollHeight, 220) + "px"; });
  ta.addEventListener("keydown", (e) => { if (e.key === "Enter" && !e.shiftKey && !e.isComposing) { e.preventDefault(); $("chat-form").requestSubmit(); } });
  $("chat-form").addEventListener("submit", async (e) => {
    e.preventDefault();
    const text = ta.value.trim(); if ((!text && !pending.length) || !S.pid) return;
    const btn = $("chat-send"); btn.disabled = true;
    try {
      const b = S.next === "answer_question"
        ? await call("reply", { message_id: S.mid, text })
        : await call("send_message", { project_id: S.pid, text: text || "(files attached)", conversation_id: S.cid || undefined, send_anyway: !available(project()), idempotency_key: crypto.randomUUID(),
            attachments: pending.length ? await Promise.all(pending.map(async (f) => ({ name: f.name, data_base64: await b64(f) }))) : undefined });
      ta.value = ""; ta.style.height = "auto"; pending.length = 0; chips();
      block(b); watch(); reload();
      history.replaceState(null, "", "#" + S.cid);
    } catch (err) {
      const ban = $("chat-banner"); ban.hidden = false; ban.textContent = err.message;
    } finally { btn.disabled = false; }
  });
  reload().then(() => { if (location.hash.startsWith("#c_")) open(location.hash.slice(1)); else fresh(); })
    .catch((e) => add([{ seq: -1, actor: "system", text: e.message }]));
})();
