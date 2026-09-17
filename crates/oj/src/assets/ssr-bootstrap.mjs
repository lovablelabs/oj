// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// The in-process SSR runner's JS half: imported by the embedded engine, which
// calls the exports below once per request. The entry arrives as a
// version-stamped specifier; `import()` goes through the engine's module
// host, so transforms, virtual modules and invalidation all live in Rust.
// Unchanged specifiers hit the isolate's module cache — concurrent requests
// share one evaluation, and untouched subtrees keep their instances.

const serialize = (data) => JSON.stringify(data ?? null).replace(/</g, "\\u003c");

const loadData = async (ns, url) => (typeof ns.load === "function" ? await ns.load(url) : null);

export async function load(entry, url) {
  return serialize(await loadData(await import(entry), url));
}

export async function action(entry, url, bodyText, bodyBase64) {
  const ns = await import(entry);
  if (typeof ns.action === "function") {
    const bytes = Uint8Array.from(atob(bodyBase64), (c) => c.charCodeAt(0));
    await ns.action(url, bodyText, bytes);
  }
  return serialize(await loadData(ns, url));
}

export async function render(entry, url) {
  const ns = await import(entry);
  const data = await loadData(ns, url);
  const head = typeof ns.head === "function" ? String(await ns.head(url, data)) : "";
  if (typeof ns.renderStream !== "function" && typeof ns.render !== "function") {
    throw new Error(`SSR entry ${entry} exports neither render() nor renderStream()`);
  }
  let html = "";
  if (typeof ns.renderStream === "function") {
    const decoder = new TextDecoder();
    const reader = (await ns.renderStream(url, data)).getReader();
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      if (value && value.length) html += decoder.decode(value, { stream: true });
    }
    html += decoder.decode();
  } else {
    html = String(await ns.render(url, data));
  }
  return { data: serialize(data), head, html };
}

export async function callFn(module, name, args) {
  const ns = await import(module);
  const fn = name === "default" ? ns.default : ns[name];
  if (typeof fn !== "function") throw new Error(`server function "${name}" not found in ${module}`);
  return (await fn(...(Array.isArray(args) ? args : []))) ?? null;
}
