// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// Shared hydration gate for the e2e suites. Modeled on Vite's playground
// vitestSetup (the "page has no browser errors" invariant) and TanStack Start's
// e2e fixture (drive the real client, then assert an interaction proves handlers
// attached post-hydration). The point is that an SSR shell can paint perfectly
// while the client never runs — a 404/500/hang on a client module, a React
// hydration mismatch — so a log-grep or an HTTP-only check passes while the page
// is dead. This helper loads the page in a real browser and fails loudly, with
// the offending {status,url} or error text, when the client graph does not serve
// or the runtime does not come alive.

// React 18 + 19 hydration-mismatch strings. If any of these appears in the
// browser's console or as a page error, the client and server disagreed and the
// tree was (or would be) thrown away and re-rendered — never a pass.
export const HYDRATION_SIGNATURES = [
  "Hydration failed because the server rendered",
  "didn't match the client",
  "tree will be regenerated on the client",
  "attributes of the server rendered HTML didn't match",
  "react.dev/link/hydration-mismatch",
  "Hydration failed because the initial UI does not match",
  "Text content did not match",
  "Expected server HTML to contain",
  "Did not expect server HTML",
  "An error occurred during hydration",
  "There was an error while hydrating",
];

// A URL the browser fetched that is a script/module in oj's dev graph. Covers
// plain extensions and oj/Vite's virtual prefixes (which have no extension).
const MODULE_EXT_RE = /\.(?:m?[jt]sx?)(?:$|[?#])/;
function isModuleUrl(url) {
  if (MODULE_EXT_RE.test(url)) return true;
  return (
    url.includes("/@fs/") ||
    url.includes("/@id/") ||
    url.includes("/@oj/") ||
    url.includes("/@oj-start/") ||
    url.includes("/@oj-deps/")
  );
}

// Parse oj's bound port out of its stdout. oj auto-increments off a busy port
// (unless --strict-port), so the requested port is not necessarily the bound
// one; the "http://localhost:PORT/" line it prints is authoritative. Returns a
// number or null. (When stdout is a pipe, oj emits the URL plain — no ANSI.)
export function parseBoundPort(text) {
  const m = /http:\/\/localhost:(\d+)\//.exec(String(text || ""));
  return m ? Number(m[1]) : null;
}

// Attach browser diagnostics to a page. Returns live arrays that fill as the
// page runs: console error+warning messages, uncaught page errors, failed
// requests, and any script/module response with status >= 400.
export function collectBrowserErrors(page) {
  const consoleMessages = []; // { type, text }
  const pageErrors = []; // string
  const requestFailures = []; // { url, error }
  const badResponses = []; // { status, url }

  page.on("console", (m) => {
    const type = m.type();
    if (type === "error" || type === "warning") consoleMessages.push({ type, text: m.text() });
  });
  page.on("pageerror", (e) => pageErrors.push((e && e.message) || String(e)));
  page.on("requestfailed", (r) => requestFailures.push({ url: r.url(), error: r.failure()?.errorText ?? "unknown" }));
  page.on("response", (r) => {
    if (r.status() >= 400 && isModuleUrl(r.url())) {
      // Capture the body the BROWSER actually got (a re-fetch can return a
      // different result on a flaky module); async, resolves before the assert.
      const entry = { status: r.status(), url: r.url(), body: "" };
      badResponses.push(entry);
      r.text()
        .then((t) => {
          entry.body = t.replace(/\s+/g, " ").trim().slice(0, 600);
        })
        .catch(() => {});
    }
  });

  return { consoleMessages, pageErrors, requestFailures, badResponses };
}

const asMatcher = (w) => (s) => (w instanceof RegExp ? w.test(s) : String(s).includes(w));
const hasSignature = (s) => HYDRATION_SIGNATURES.some((sig) => s.includes(sig));

// Fetch `entryUrl` through the page's request context and assert it (and its
// statically-imported same-origin module URLs) serve 200. This is TanStack's
// `page.request.get(virtualPath)` idiom: it makes the physical client entry
// behind a virtual module (e.g. the client.tsx behind
// virtual:tanstack-start-dev-client-entry) an explicit, named check rather than
// a silent waitForSelector timeout. Returns { ok, failures[] }.
export async function assertModuleGraphServes(page, entryUrl) {
  const failures = [];
  const res = await page.request.get(entryUrl);
  if (!res.ok()) {
    failures.push(`${res.status()} ${entryUrl}`);
    return { ok: false, failures };
  }
  const src = await res.text();
  const specs = new Set();
  const re =
    /(?:import|export)\b[^'"]*?\bfrom\s*["']([^"']+)["']|import\(\s*["']([^"']+)["']\s*\)|import\s*["']([^"']+)["']/g;
  let m;
  while ((m = re.exec(src))) {
    const spec = m[1] || m[2] || m[3];
    if (spec) specs.add(spec);
  }
  const base = new URL(entryUrl);
  for (const spec of specs) {
    let target;
    try {
      if (spec.startsWith("/")) target = new URL(spec, base.origin).href;
      else if (spec.startsWith(".")) target = new URL(spec, base).href;
      else continue; // bare specifier -> not a dev-served URL
    } catch {
      continue;
    }
    const r = await page.request.get(target);
    if (!r.ok()) {
      let detail = "";
      try {
        detail = (await r.text()).replace(/\s+/g, " ").trim().slice(0, 300);
      } catch {}
      failures.push(`${r.status()} ${target}${detail ? ` :: ${detail}` : ""}`);
    }
  }
  return { ok: failures.length === 0, failures };
}

// Load `url` in `browser` and run the hydration ladder, each rung failing loudly
// with evidence. Returns { ok, failures[] }; throws with the joined failures
// unless opts.throwOnFail === false.
//
// opts:
//   ssrMarker?       text expected in the RAW SSR HTML (page.request.get)
//   clientMarker     selector for a client-only element (absent from SSR HTML,
//                    present only after the client runtime mounts)
//   clientMarkerText expected trimmed textContent of clientMarker
//   interaction?     { click: selector, expect: { selector, text? } } -- proves
//                    event handlers attached post-hydration
//   deadlineMs=30000 per-step timeout
//   whitelist?       (RegExp|string)[] of console/response noise to ignore
//                    (e.g. favicon 404, the CF slim-app server-fn limitation)
//   throwOnFail=true throw on any failure with the full evidence
export async function assertHydrates(browser, url, opts = {}) {
  const {
    ssrMarker,
    clientMarker,
    clientMarkerText,
    interaction,
    deadlineMs = 30000,
    whitelist = [],
    throwOnFail = true,
  } = opts;
  const matchers = whitelist.map(asMatcher);
  const whitelisted = (s) => matchers.some((f) => f(s));
  const failures = [];

  const page = await browser.newPage();
  const diag = collectBrowserErrors(page);
  try {
    // 1. doc 200 + ssrMarker present in the raw SSR HTML.
    const docRes = await page.request.get(url);
    const rawHtml = await docRes.text();
    if (docRes.status() !== 200) failures.push(`document ${url} returned ${docRes.status()} (want 200)`);
    if (ssrMarker && !rawHtml.includes(ssrMarker)) {
      failures.push(`SSR HTML missing ssrMarker ${JSON.stringify(ssrMarker)}\n${rawHtml.slice(0, 800)}`);
    }

    // Load the page and let its client graph fetch.
    let navError = null;
    try {
      await page.goto(url, { waitUntil: "domcontentloaded", timeout: deadlineMs });
    } catch (e) {
      navError = (e && e.message) || String(e);
    }

    // 3. client runtime mounted (clientMarker eventually present).
    let mounted = false;
    if (clientMarker) {
      try {
        await page.waitForSelector(clientMarker, { timeout: deadlineMs });
        mounted = true;
      } catch {
        /* reported below, after the module-graph check names the likely cause */
      }
    }

    // 2. every client module served 200. Checked after load so the browser has
    // finished fetching the graph. THIS names the bundledDev /assets/index.js
    // 404/hang and the react-refresh 500 on client.tsx, instead of a bare
    // "selector never appeared" timeout.
    const badModules = diag.badResponses.filter((r) => !whitelisted(r.url));
    if (badModules.length) {
      // Re-fetch each failing module to surface the server's error body (the
      // browser-observed response only carried a status), so a 500 names its
      // compile/resolve cause instead of a bare status line.
      const lines = [];
      for (const r of badModules) {
        // Prefer the body the browser actually received (captured inline); a
        // re-fetch can answer differently on a flaky module, so it's a labelled
        // fallback only.
        let detail = r.body ? `\n    :: ${r.body}` : "";
        if (!r.body) {
          try {
            const again = await page.request.get(r.url);
            const body = (await again.text()).replace(/\s+/g, " ").trim().slice(0, 400);
            if (body) detail = `\n    :: (re-fetch ${again.status()}) ${body}`;
          } catch {}
        }
        lines.push(`  ${r.status} ${r.url}${detail}`);
      }
      failures.push("client module graph served >= 400:\n" + lines.join("\n"));
    }

    if (clientMarker && !mounted) {
      failures.push(
        `client runtime did not mount: ${clientMarker} never appeared within ${deadlineMs}ms` +
          (navError ? ` (navigation: ${navError})` : ""),
      );
    }

    // 4. client-only marker text is correct.
    if (clientMarker && mounted && clientMarkerText != null) {
      const text = (await page.textContent(clientMarker)) ?? "";
      if (text.trim() !== clientMarkerText) {
        failures.push(`client marker text ${JSON.stringify(text.trim())} != ${JSON.stringify(clientMarkerText)}`);
      }
    }

    // 5. interaction: click, then poll for the expected result. Proves handlers
    // are live after hydration.
    if (interaction && mounted) {
      try {
        await page.click(interaction.click, { timeout: deadlineMs });
        const sel = interaction.expect.selector;
        if (interaction.expect.text != null) {
          await page.waitForFunction(
            ([s, t]) => document.querySelector(s)?.textContent?.includes(t),
            [sel, interaction.expect.text],
            { timeout: deadlineMs },
          );
        } else {
          await page.waitForSelector(sel, { timeout: deadlineMs });
        }
      } catch (e) {
        failures.push(
          `interaction failed (click ${interaction.click} -> expect ${JSON.stringify(interaction.expect)}): ` +
            ((e && e.message) || e),
        );
      }
    }

    // 6. no page errors, no console errors, and specifically no React
    // hydration-mismatch signatures (in console error OR warning, or a page
    // error).
    const pageErrs = diag.pageErrors.filter((s) => !whitelisted(s));
    const hydrationHits = [...diag.pageErrors, ...diag.consoleMessages.map((m) => m.text)].filter(
      (s) => !whitelisted(s) && hasSignature(s),
    );
    const consoleErrs = diag.consoleMessages.filter(
      (m) => m.type === "error" && !whitelisted(m.text) && !hasSignature(m.text),
    );

    if (hydrationHits.length) {
      failures.push(
        "React hydration-mismatch signature(s) in the browser output:\n" +
          hydrationHits.map((s) => "  " + s).join("\n"),
      );
    }
    if (pageErrs.length) failures.push("page error(s):\n" + pageErrs.map((s) => "  " + s).join("\n"));
    if (consoleErrs.length) {
      failures.push("console error(s):\n" + consoleErrs.map((m) => `  [${m.type}] ${m.text}`).join("\n"));
    }
  } finally {
    await page.close();
  }

  const ok = failures.length === 0;
  if (!ok && throwOnFail) {
    throw new Error(`assertHydrates(${url}) FAILED:\n` + failures.map((f) => "- " + f).join("\n"));
  }
  return { ok, failures };
}
