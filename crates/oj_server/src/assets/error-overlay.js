// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// The error overlay oj's dev clients share: the SPA client (/@vite/client,
// which re-exports it as Vite's does) and the Start live-reload client
// (/@oj-start/error-overlay.js). Kept free of either client's state.

function parseError(text) {
  const s = String(text);
  const title = (s.split("\n").find((l) => l.trim()) || "Build error").trim();
  const loc = s.match(/([^\s():]+\.[A-Za-z0-9]+):(\d+)(?::(\d+))?/);
  return { title, file: loc && loc[1], line: loc && loc[2], col: loc && loc[3], frame: s };
}

// Vite's `vite-error-overlay` custom element: framework runtimes look it up
// with customElements.get() and construct it with an ErrorPayload `err`
// ({message, stack, id, loc, frame, plugin}) to show their own runtime errors.
export class ErrorOverlay extends HTMLElement {
  constructor(err, _links = true) {
    super();
    const root = this.attachShadow({ mode: "open" });
    const e = err && typeof err === "object" ? err : { message: String(err ?? "unknown error") };
    const parsed = parseError(e.message || "unknown error");
    const file = e.loc?.file || e.id || parsed.file;
    const line = e.loc?.line ?? parsed.line;
    const col = e.loc?.column ?? parsed.col;
    const frame = e.frame && String(e.frame).trim() ? e.frame : parsed.frame;
    const backdrop = document.createElement("div");
    backdrop.setAttribute("role", "dialog");
    backdrop.setAttribute("aria-label", "Build error");
    backdrop.style.cssText =
      "position:fixed;inset:0;z-index:99999;background:rgba(10,10,14,.86);" +
      "display:flex;align-items:flex-start;justify-content:center;padding:6vh 4vw;overflow:auto;" +
      "font:13px/1.6 ui-monospace,SFMono-Regular,Menlo,monospace";
    const card = document.createElement("div");
    card.style.cssText =
      "max-width:920px;width:100%;background:#16161c;border:1px solid #33333f;border-radius:12px;" +
      "box-shadow:0 20px 60px rgba(0,0,0,.5);overflow:hidden;color:#e6e6ea";
    const bar = document.createElement("div");
    bar.style.cssText =
      "display:flex;align-items:center;gap:10px;padding:12px 18px;background:#2a1417;border-bottom:1px solid #4a2028";
    const brand = document.createElement("span");
    brand.style.cssText = "font-weight:700;letter-spacing:.08em;color:#ff6b6b";
    brand.textContent = "oj";
    const label = document.createElement("span");
    label.style.cssText = "color:#ff9a9a;font-weight:600";
    label.textContent = e.plugin && e.plugin !== "oj" ? `Build error (plugin: ${e.plugin})` : "Build error";
    bar.append(brand, label);
    const body = document.createElement("div");
    body.style.cssText = "padding:18px";
    const title = document.createElement("div");
    title.style.cssText = "color:#ff8a8a;font-weight:600;font-size:14px;white-space:pre-wrap;margin-bottom:10px";
    title.textContent = parsed.title;
    body.appendChild(title);
    if (file) {
      const loc = document.createElement("div");
      loc.style.cssText = "color:#8ab4ff;margin-bottom:12px;font-size:12px";
      loc.textContent = file + (line ? ":" + line + (col ? ":" + col : "") : "");
      body.appendChild(loc);
    }
    const pre = document.createElement("pre");
    pre.style.cssText =
      "margin:0;white-space:pre-wrap;background:#0d0d12;border:1px solid #26262f;border-radius:8px;" +
      "padding:12px 14px;color:#c9c9d4;max-height:52vh;overflow:auto";
    pre.textContent = frame;
    body.appendChild(pre);
    if (e.stack && String(e.stack).trim() && !String(frame).includes(String(e.stack).trim())) {
      const stack = document.createElement("pre");
      stack.style.cssText = "margin:12px 0 0;white-space:pre-wrap;color:#8d8d9c;font-size:12px";
      stack.textContent = e.stack;
      body.appendChild(stack);
    }
    const hint = document.createElement("div");
    hint.style.cssText = "margin-top:12px;color:#6f6f80;font-size:12px";
    hint.textContent = "Fix the file and save to retry · click outside or press Esc to dismiss";
    body.appendChild(hint);
    card.append(bar, body);
    backdrop.appendChild(card);
    // Dismiss on backdrop click only, so text inside the card stays selectable.
    backdrop.addEventListener("click", (ev) => {
      if (ev.target === backdrop) this.close();
    });
    this._onKey = (ev) => {
      if (ev.key === "Escape") this.close();
    };
    window.addEventListener("keydown", this._onKey);
    root.appendChild(backdrop);
  }
  close() {
    window.removeEventListener("keydown", this._onKey);
    this.remove();
  }
}

const overlayId = "vite-error-overlay";
if (typeof customElements !== "undefined" && !customElements.get(overlayId)) {
  customElements.define(overlayId, ErrorOverlay);
}
