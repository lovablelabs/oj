// SPDX-License-Identifier: MIT

(() => {
  const KEY = "oj:start:preserve:" + location.pathname;
  const FIELDS = "input, textarea, select";

  function domPath(el) {
    const parts = [];
    while (el && el.nodeType === 1 && el !== document.body) {
      let i = 1;
      for (let s = el.previousElementSibling; s; s = s.previousElementSibling) {
        if (s.tagName === el.tagName) i++;
      }
      parts.unshift(el.tagName + ":" + i);
      el = el.parentElement;
    }
    return parts.join(">");
  }
  const key = (el) => (el.id ? "#" + el.id : "p:" + domPath(el));

  function snapshot() {
    const fields = {};
    for (const el of document.querySelectorAll(FIELDS)) {
      if (el.type === "password" || el.type === "file") continue;
      const rec = { v: el.value };
      if (el.type === "checkbox" || el.type === "radio") rec.c = el.checked;
      if (el.selectionStart != null) {
        rec.s = el.selectionStart;
        rec.e = el.selectionEnd;
      }
      fields[key(el)] = rec;
    }
    const active = document.activeElement;
    const scroll = { x: window.scrollX, y: window.scrollY };
    for (const el of document.querySelectorAll("[id]")) {
      if (el.scrollTop || el.scrollLeft) (scroll.el ??= {})["#" + el.id] = [el.scrollTop, el.scrollLeft];
    }
    const data = { fields, scroll, active: active && active.matches(FIELDS) ? key(active) : null };
    try {
      sessionStorage.setItem(KEY, JSON.stringify(data));
    } catch {}
  }

  function setValue(el, v) {
    const proto =
      el instanceof HTMLTextAreaElement
        ? HTMLTextAreaElement.prototype
        : el instanceof HTMLSelectElement
          ? HTMLSelectElement.prototype
          : HTMLInputElement.prototype;
    const setter = Object.getOwnPropertyDescriptor(proto, "value")?.set;
    setter ? setter.call(el, v) : (el.value = v);
    el.dispatchEvent(new Event("input", { bubbles: true }));
    el.dispatchEvent(new Event("change", { bubbles: true }));
  }

  function apply(data) {
    const byKey = new Map();
    for (const el of document.querySelectorAll(FIELDS)) byKey.set(key(el), el);
    for (const [k, rec] of Object.entries(data.fields)) {
      const el = byKey.get(k);
      if (!el) continue;
      if (rec.c != null) {
        el.checked = rec.c;
        el.dispatchEvent(new Event("change", { bubbles: true }));
      } else if (el.value !== rec.v) setValue(el, rec.v);
    }
    window.scrollTo(data.scroll.x, data.scroll.y);
    for (const [sel, [t, l]] of Object.entries(data.scroll.el || {})) {
      const el = document.querySelector(sel);
      if (el) {
        el.scrollTop = t;
        el.scrollLeft = l;
      }
    }
    if (data.active) {
      const el = byKey.get(data.active);
      if (el) {
        el.focus();
        const rec = data.fields[data.active];
        if (rec && rec.s != null && el.setSelectionRange) {
          try {
            el.setSelectionRange(rec.s, rec.e);
          } catch {}
        }
      }
    }
  }

  function restore() {
    let data;
    try {
      data = JSON.parse(sessionStorage.getItem(KEY));
    } catch {}
    if (!data) return;
    sessionStorage.removeItem(KEY);
    let n = 0;
    const tick = () => {
      apply(data);
      if (++n < 4) setTimeout(tick, 60);
    };
    requestAnimationFrame(tick);
  }

  if (document.readyState === "complete") restore();
  else window.addEventListener("load", restore);

  let ws;
  const connect = () => {
    ws = new WebSocket((location.protocol === "https:" ? "wss" : "ws") + "://" + location.host + "/@oj-start/hmr");
    ws.onmessage = () => {
      snapshot();
      location.reload();
    };
    ws.onclose = () => setTimeout(connect, 1000);
  };
  connect();
})();
