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
