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

  // One live status line instead of a note per stage: a Claude Code-style
  // spinner, a random critter per stage, and a verb that changes while it works.
  const SPIN = ["·", "✢", "✳", "✶", "✻", "✽", "✻", "✶", "✳", "✢"];
  const CRITTERS = ["ʕ•ᴥ•ʔ", "(•ᴗ•)", "ᕕ( ᐛ )ᕗ", "(=^･ω･^=)", "(ﾉ◕ヮ◕)ﾉ", "┌( ಠ‿ಠ)┘", "(⌐■_■)", "(ง •̀_•́)ง", "(っ˘ڡ˘ς)", "(˘ω˘)", "ʕっ•ᴥ•ʔっ", "(ᵔᴥᵔ)"];
  const VERBS = ["Pondering", "Tinkering", "Noodling", "Brewing", "Spelunking", "Untangling", "Wrangling", "Conjuring", "Percolating", "Clauding"];
  const pick = (l) => l[Math.floor(Math.random() * l.length)];
  const st = { el: null, text: "", live: false, critter: "", tick: 0 };
  function status(text, live, working) {
    if (!st.el) {
      st.el = el("div", "status-line");
      st.el.append(el("span", "spin"), el("span", "critter"), el("span", "txt"));
      setInterval(() => {
        if (!st.el || !st.el.isConnected) return;
        st.tick++;
        st.el.children[0].textContent = st.live ? SPIN[st.tick % SPIN.length] : "✻";
        if (st.working && st.tick % 25 === 0) st.el.children[2].textContent = pick(VERBS) + "…";
      }, 120);
    }
    if (text !== st.text) { st.critter = pick(CRITTERS); st.text = text; }
    st.live = live; st.working = working;
    st.el.classList.toggle("done", !live);
    st.el.children[1].textContent = st.critter;
    st.el.children[2].textContent = working ? pick(VERBS) + "…" : text;
    st.el.hidden = !text;
    $("chat-log").append(st.el);
  }

  // Sing: the CHIMERA head on your own message sings it up a scale (POST /sing).
  // An access link proves itself with x-link; a signed-in page with its cookie.
  let playing = null;
  function singer(text) {
    const b = el("button", "singer"); b.type = "button"; b.title = "Sing it"; b.setAttribute("aria-label", "Sing this message");
    const i = el("img"); i.src = "/static/logos/chimera.png"; i.alt = ""; b.append(i);
    b.onclick = async () => {
      if (playing) { playing.audio.pause(); playing.btn.classList.remove("singing", "loading"); const was = playing.btn; playing = null; if (was === b) return; }
      b.classList.add("loading");
      try {
        const link = root.dataset.rpc.startsWith("/mcp/") ? root.dataset.rpc.slice(5) : "";
        const r = await fetch("/sing", { method: "POST", headers: Object.assign({ "Content-Type": "application/json" }, link ? { "x-link": link } : {}), body: JSON.stringify({ text }) });
        if (!r.ok) throw new Error((await r.json().catch(() => ({}))).error || "The voice is busy. Try again.");
        const audio = new Audio(URL.createObjectURL(await r.blob()));
        playing = { audio, btn: b };
        b.classList.remove("loading"); b.classList.add("singing");
        audio.onended = () => { b.classList.remove("singing"); if (playing && playing.btn === b) playing = null; };
        await audio.play();
      } catch (err) {
        b.classList.remove("loading", "singing");
        notice(err.message, "bad");
      }
    };
    return b;
  }

  // Deploy: the targets the developer allowed this tester, remembered per project.
  const depKey = () => "substrate.deploy." + S.pid;
  function deployChoice() {
    const t = (project() || {}).deploy_targets || [];
    if (!t.length) return undefined;
    let c = "none"; try { c = localStorage.getItem(depKey()) || "none"; } catch (e) {}
    return c === "none" || t.includes(c) ? c : "none";
  }
  function deployOpts() {
    const box = $("chat-opts"), t = (project() || {}).deploy_targets || [];
    box.replaceChildren(); box.hidden = !t.length;
    if (!t.length) return;
    const seg = el("div", "seg"); seg.setAttribute("role", "radiogroup"); seg.setAttribute("aria-label", "Deploy");
    seg.append(el("span", "seg-label", "Deploy"));
    const cur = deployChoice();
    for (const [v, label] of [["none", "Not yet"], ["test", "To test"], ["production", "To production"]]) {
      if (v !== "none" && !t.includes(v)) continue;
      const b = el("button", "seg-btn" + (v === cur ? " on" : "") + (v === "production" ? " prod" : ""), label);
      b.type = "button"; b.setAttribute("role", "radio"); b.setAttribute("aria-checked", String(v === cur));
      b.onclick = () => { try { localStorage.setItem(depKey(), v); } catch (e) {} deployOpts(); };
      seg.append(b);
    }
    box.append(seg);
  }

  // The deploy track: one line of steps from the stage the agent and CI report.
  const STEPS = ["Commit", "Checks", "Push", "CI", "Deploy", "Live"];
  const AT = { committing: [0, "run"], running_checks: [1, "run"], pushing: [2, "run"], fix_committed: [3, "wait"], ci_running: [3, "run"],
    ci_passed: [4, "wait"], ci_failed: [3, "fail"], deploying: [4, "run"], deployed: [5, "wait"], deploy_failed: [4, "fail"], verified: [6, "done"], ready_to_retest: [6, "done"] };
  const dt = { el: null };
  function deployTrack(stage, text) {
    const log = $("chat-log");
    if (!stage || !AT[stage]) { if (dt.el) dt.el.remove(); dt.el = null; return; }
    const [at, how] = AT[stage];
    if (!dt.el) dt.el = el("div", "deploy-track");
    dt.el.replaceChildren();
    STEPS.forEach((s, i) => {
      const state = i < at ? "done" : i === at ? how : "todo";
      const step = el("span", "step " + state);
      step.append(el("span", "mark", state === "done" ? "✓" : state === "fail" ? "✕" : state === "run" ? "✻" : "·"), s);
      dt.el.append(step);
      if (i < STEPS.length - 1) dt.el.append(el("span", "sep", "›"));
    });
    if (text) dt.el.append(el("span", "dt-note", text));
    log.append(dt.el);
  }

  // One notice above the composer. warn stays (the agent is offline); bad is an error
  // that goes after a few seconds or on its ×.
  let noticeTimer = null;
  function notice(text, kind) {
    const b = $("chat-banner");
    clearTimeout(noticeTimer);
    if (!text) { b.hidden = true; return; }
    b.className = "chat-banner notice " + (kind || "warn");
    $("chat-banner-text").textContent = text;
    $("chat-banner-x").hidden = kind !== "bad";
    b.hidden = false;
    if (kind === "bad") noticeTimer = setTimeout(() => notice(S.offlineText || ""), 6000);
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
    if (!available(p)) return el("span", "tag off", "offline");
    if (p.agent_state === "working") return el("span", "tag blue", "working");
    if (p.agent_state === "blocked") return el("span", "tag warn", "busy");
    return el("span", "tag ok", "free");
  }
  // The agent's own logo is its status: it moves while working, glows when it needs
  // someone, and greys out when offline.
  function agentMark(p) {
    const state = !available(p) ? "offline" : p.agent_state === "working" ? "working" : p.agent_state === "blocked" ? "blocked" : "free";
    const lg = agentLogo(p.agent);
    if (!lg) return pill(p);
    const m = el("span", "agent-mark " + state + (String(p.agent || "").toLowerCase().includes("claude") ? " claude" : ""));
    m.title = (p.agent || "Agent") + ": " + { offline: "offline", working: "working", blocked: "needs someone at the computer", free: "free" }[state];
    lg.width = lg.height = 18; m.append(lg);
    return m;
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
      b.append(icon(p), el("span", "grow", p.name), agentMark(p));
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
    // project › person › agent
    const t = el("div", "row"); t.append(icon(p), el("strong", null, p.name), el("span", "crumb-sep", "›"));
    if (p.developer_avatar) { const av = el("img", "avatar-img"); av.src = p.developer_avatar; av.alt = ""; t.append(av); }
    t.append(el("span", "faint", dev()), el("span", "crumb-sep", "›"), agentMark(p), el("span", "faint", p.agent || "agent"));
    if (!available(p)) t.append(el("span", "tag off", "offline"));
    h.append(t);
    if (summary) h.append(el("div", "muted chat-sum", named(summary)));
    const btn = $("chat-send").querySelector("span");
    if (available(p)) { S.offlineText = ""; notice(""); btn.textContent = S.next === "answer_question" ? "Answer" : "Send"; }
    else { S.offlineText = "The agent is offline. A message waits until " + dev() + " starts it again."; notice(S.offlineText, "warn"); btn.textContent = "Send anyway"; }
    deployOpts();
    $("chat-text").placeholder = S.next === "answer_question" ? "Answer the agent's question" : "Message " + (p.agent || "the agent");
  }

  function add(events) {
    const log = $("chat-log");
    const empty = log.querySelector(".chat-empty"); if (empty && events.length) empty.remove();
    for (const e of events || []) {
      if (S.seen.has(e.seq)) continue;
      S.seen.add(e.seq);
      let n;
      if (e.actor === "human") { n = el("div", "msg me"); n.append(singer(e.text), rich(e.text), files(e.attachments)); }
      else if (e.actor === "agent") {
        n = el("div", "msg agent");
        const who = el("div", "who"), lg = agentLogo(e.name); if (lg) who.append(lg);
        who.append(e.name + (e.kind === "question" ? " asks" : ""));
        n.append(who, rich(e.agent_text), files(e.attachments));
      } else if (e.actor === "deploy") { S.deployText = e.agent_text || ""; continue; }
      else if (e.kind === "stage") { S.stageText = named(e.text); continue; }
      else n = el("div", "note", named(e.text || e.kind));
      log.append(n);
    }
    log.scrollTop = log.scrollHeight;
  }

  function block(b) {
    if (!b) return;
    S.mid = b.message_id; S.cid = b.conversation_id; S.next = b.next_action || "";
    add(b.new_events); S.seq = Math.max(S.seq, b.seq || 0);
    const asked = b.next_action === "answer_question";
    status(asked ? "Waiting for your answer." : (S.stageText || named(b.summary)), !b.terminal && !asked, b.stage === "working");
    deployTrack(b.deployment_stage, S.deployText);
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
    S.gen++; S.cid = S.mid = null; S.seq = 0; S.next = ""; S.seen = new Set(); S.stageText = ""; S.deployText = ""; st.text = ""; dt.el = null;
    history.replaceState(null, "", location.pathname);
    const log = $("chat-log"), e = el("div", "chat-empty");
    const p = project(), big = p ? icon(p) : null;
    if (big) { big.style.width = big.style.height = "64px"; big.style.fontSize = "28px"; big.style.borderRadius = "18px"; e.append(big); }
    // About the project: its icon, its name, and what the developer wrote for testers.
    e.append(el("h2", null, p ? "About " + p.name : "Talk to the developer's agent."));
    if (p && p.description) e.append(el("p", "about", p.description));
    e.append(el("p", "muted", "Report a bug, ask a question or request a change. " + dev() + " sees every message."));
    log.replaceChildren(e);
    sidebar(); head(); $("chat-text").focus();
  }

  async function open(cid) {
    S.gen++; S.seen = new Set(); S.seq = 0; S.stageText = ""; S.deployText = ""; st.text = ""; dt.el = null; $("chat-log").replaceChildren();
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
      if (f.size > 10 * 1024 * 1024) { notice(f.name + " is over 10 MB.", "bad"); continue; }
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

  // Agent status is pushed: the relay's websocket reaches the server, which
  // sends "changed" here; then the sidebar pills and the header refresh.
  if (window.EventSource) {
    const live = root.dataset.rpc === "/chat/rpc" ? "/chat/live" : root.dataset.rpc + "/live";
    new EventSource(live).onmessage = () => reload().catch(() => {});
  }

  $("chat-banner-x").onclick = () => notice(S.offlineText || "");
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
        : await call("send_message", { project_id: S.pid, text: text || "(files attached)", conversation_id: S.cid || undefined, send_anyway: !available(project()), idempotency_key: crypto.randomUUID(), deploy: deployChoice(),
            attachments: pending.length ? await Promise.all(pending.map(async (f) => ({ name: f.name, data_base64: await b64(f) }))) : undefined });
      ta.value = ""; ta.style.height = "auto"; pending.length = 0; chips();
      block(b); watch(); reload();
      history.replaceState(null, "", "#" + S.cid);
    } catch (err) {
      notice(err.message, "bad");
    } finally { btn.disabled = false; }
  });
  reload().then(() => { if (location.hash.startsWith("#c_")) open(location.hash.slice(1)); else fresh(); })
    .catch((e) => add([{ seq: -1, actor: "system", text: e.message }]));
})();
