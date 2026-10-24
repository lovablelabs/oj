// Ported verbatim from @stylexjs/unplugin 0.19.0 (lib/consts.js, VIRTUAL_STYLEX_RUNTIME_SCRIPT).
// Copyright (c) Meta Platforms, Inc. and affiliates. MIT License.
const STYLE_ID = "__stylex_virtual__";
const DEV_CSS_PATH = "/virtual:stylex.css";
const AFTER_UPDATE_DELAY = 180;
let lastCSS = "";

function ensure() {
  let el = document.getElementById(STYLE_ID);
  if (!el) {
    el = document.createElement("style");
    el.id = STYLE_ID;
    document.head.appendChild(el);
  }
  return el;
}

function disableLink() {
  try {
    const links = [...document.querySelectorAll('link[rel="stylesheet"]')];
    for (const l of links) {
      if (typeof l.href === "string" && l.href.includes(DEV_CSS_PATH)) {
        l.disabled = true;
      }
    }
  } catch {}
}

async function fetchCSS() {
  const t = Date.now();
  const r = await fetch(DEV_CSS_PATH + "?t=" + t, { cache: "no-store" });
  return r.text();
}

async function update() {
  try {
    const css = await fetchCSS();
    if (css !== lastCSS) {
      ensure().textContent = css;
      disableLink();
      lastCSS = css;
    }
  } catch {}
}

update();

if (import.meta.hot) {
  import.meta.hot.on("stylex:css-update", update);
  import.meta.hot.on("vite:afterUpdate", () => setTimeout(update, AFTER_UPDATE_DELAY));
  import.meta.hot.dispose(() => {
    const el = document.getElementById(STYLE_ID);
    if (el && el.parentNode) el.parentNode.removeChild(el);
  });
}

export {};
