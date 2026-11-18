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
    const t = c.textContent;
    c.textContent = "Copied";
    setTimeout(() => (c.textContent = t), 1400);
  }
});

document.addEventListener("submit", (e) => {
  const msg = e.target.getAttribute("data-confirm");
  if (msg && !confirm(msg)) e.preventDefault();
});
