// Ported verbatim from @stylexjs/unplugin 0.19.0 (lib/consts.js, VIRTUAL_STYLEX_CSS_ONLY_SCRIPT).
// Copyright (c) Meta Platforms, Inc. and affiliates. MIT License.
const DEV_CSS_PATH = "/virtual:stylex.css";

function bust() {
  try {
    const links = [...document.querySelectorAll('link[rel="stylesheet"]')];
    for (const l of links) {
      if (typeof l.href === "string" && l.href.includes(DEV_CSS_PATH)) {
        const u = new URL(l.href, location.origin);
        u.searchParams.set("t", String(Date.now()));
        l.href = u.pathname + u.search;
      }
    }
  } catch {}
}

// initial attempt to ensure we have a fresh version after client connects
if (document.readyState !== "loading") {
  bust();
} else {
  document.addEventListener("DOMContentLoaded", bust);
}

if (import.meta.hot) {
  import.meta.hot.on("stylex:css-update", bust);
}

export {};
