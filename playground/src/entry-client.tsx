import { useEffect, useState } from "react";
import { hydrateRoot } from "react-dom/client";
import {
  App,
  ErrorBoundary,
  NavContext,
  applyMeta,
  preloadRoute,
  resolveMeta,
  type DataMap,
  type NavState,
} from "@/router";
// Client-only: a plugin virtual module, exercising the client hydration
// bundle's plugin resolveId/load (build_client_entry). Not rendered, so no
// hydration concern; the globalThis assignment is a side effect (not shaken).
import { info as pluginInfo } from "virtual:plugin-greeting";
(globalThis as Record<string, unknown>).__OJ_CLIENT_PLUGIN = pluginInfo;
// Server function: on the client this import is an RPC stub; the real greet()
// runs on the server (dev runner / prod dispatch). Proves end-to-end server fns.
import { greet } from "./greeting.server";
greet("prod").then((r) => {
  (globalThis as Record<string, unknown>).__OJ_SFN = r;
});

declare global {
  interface Window {
    __OJ_DATA__?: DataMap;
  }
}

type Route = { path: string; data: DataMap; error: string | null };

// Fetch server-authoritative route data (the whole chain's loader map); a
// failed loader/action becomes an `error` the route renders instead of crashing.
async function fetchData(path: string, init?: RequestInit): Promise<{ data: DataMap; error: string | null }> {
  try {
    const res = await fetch(path, { headers: { "oj-loader": "1" }, ...init });
    if (!res.ok) return { data: {}, error: `request failed (${res.status})` };
    return { data: (await res.json()) as DataMap, error: null };
  } catch (e) {
    return { data: {}, error: String((e as Error)?.message ?? e) };
  }
}

function Router() {
  const [route, setRoute] = useState<Route>(() => ({
    path: location.pathname,
    data: window.__OJ_DATA__ ?? {},
    error: null,
  }));
  const [nav, setNav] = useState<NavState>("idle");

  useEffect(() => {
    // Prefetched loader data by path (the chunk cache lives in preloadRoute).
    const prefetchedData = new Map<string, Promise<{ data: DataMap; error: string | null }>>();

    const navigate = async (path: string) => {
      setNav("loading");
      try {
        // Reuse hover-prefetched data if present; load the chunk (cached if
        // prefetched) alongside.
        const dataPromise = prefetchedData.get(path) ?? fetchData(path);
        const [{ data, error }] = await Promise.all([dataPromise, preloadRoute(path)]);
        setRoute({ path, data, error });
        applyMeta(resolveMeta(path, data)); // update <title>/meta for the new route
      } finally {
        setNav("idle");
      }
    };
    const onPop = () => navigate(location.pathname);

    // Connection-aware gating: skip speculative prefetch when the user has Data
    // Saver on or is on a slow (2g/slow-2g) connection. Explicit navigation
    // (clicks) is never gated — the user asked for it.
    const okToPrefetch = (): boolean => {
      const c = (navigator as unknown as { connection?: { saveData?: boolean; effectiveType?: string } }).connection;
      if (!c) return true; // Network Information API unavailable -> allow
      return !c.saveData && !/2g/.test(c.effectiveType ?? "");
    };

    // Prefetch on intent (hover/focus) or viewport: warm the route's chunk and
    // loader data so the click navigates instantly.
    const hrefOf = (target: EventTarget | null): string | null => {
      const anchor = (target as Element)?.closest?.("a");
      const href = anchor?.getAttribute("href");
      if (!anchor || anchor.target || !href || !href.startsWith("/")) return null;
      return href === location.pathname + location.search ? null : href;
    };
    // Returns whether it actually prefetched (false = gated/opted-out/done), so
    // the viewport observer only stops watching links it has warmed.
    const prefetch = (target: EventTarget | null): boolean => {
      if (!okToPrefetch()) return false;
      const anchor = (target as Element)?.closest?.("a");
      if (anchor?.hasAttribute("data-no-prefetch")) return false; // opt out
      const href = hrefOf(anchor ?? null);
      if (!href || prefetchedData.has(href)) return false;
      void preloadRoute(href).catch(() => {}); // chunk
      prefetchedData.set(href, fetchData(href)); // data
      return true;
    };
    const onOver = (e: Event) => prefetch(e.target);

    // Viewport prefetch: warm links as they scroll into view (a little early
    // via rootMargin). Stop watching a link once warmed; keep watching ones
    // skipped by connection gating so they can be warmed later.
    const io = new IntersectionObserver(
      (entries) => {
        for (const e of entries) {
          if (e.isIntersecting && prefetch(e.target)) io.unobserve(e.target);
        }
      },
      { rootMargin: "200px" },
    );
    const observeLinks = () => document.querySelectorAll('a[href^="/"]').forEach((a) => io.observe(a));
    const mo = new MutationObserver(observeLinks);

    // React to a connection change: when it improves (Data Saver off / off 2g),
    // warm links currently in view that gating had skipped.
    const onConnectionChange = () => {
      if (!okToPrefetch()) return;
      for (const a of document.querySelectorAll('a[href^="/"]')) {
        const r = a.getBoundingClientRect();
        if (r.bottom > -200 && r.top < innerHeight + 200 && prefetch(a)) io.unobserve(a);
      }
    };
    const conn = (navigator as unknown as { connection?: EventTarget }).connection;

    const onClick = (e: MouseEvent) => {
      if (e.defaultPrevented || e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) {
        return;
      }
      const href = hrefOf(e.target);
      if (!href) return;
      e.preventDefault();
      history.pushState(null, "", href);
      void navigate(location.pathname);
    };

    const onSubmit = async (e: SubmitEvent) => {
      const form = (e.target as Element).closest?.("form");
      if (!form || form.method.toLowerCase() !== "post") return;
      e.preventDefault();
      const body = new URLSearchParams(new FormData(form) as unknown as Record<string, string>).toString();
      setNav("submitting");
      try {
        const { data, error } = await fetchData(location.pathname, {
          method: "POST",
          body,
          headers: { "oj-loader": "1", "content-type": "application/x-www-form-urlencoded" },
        });
        setRoute((r) => ({ ...r, data, error }));
        applyMeta(resolveMeta(location.pathname, data)); // data-derived meta may have changed
        prefetchedData.clear(); // the mutation may have invalidated prefetched data
      } finally {
        setNav("idle");
      }
    };

    addEventListener("popstate", onPop);
    document.addEventListener("click", onClick);
    document.addEventListener("submit", onSubmit);
    document.addEventListener("pointerover", onOver);
    document.addEventListener("focusin", onOver);
    observeLinks();
    mo.observe(document.body, { childList: true, subtree: true });
    conn?.addEventListener("change", onConnectionChange);
    return () => {
      removeEventListener("popstate", onPop);
      document.removeEventListener("click", onClick);
      document.removeEventListener("submit", onSubmit);
      document.removeEventListener("pointerover", onOver);
      document.removeEventListener("focusin", onOver);
      io.disconnect();
      mo.disconnect();
      conn?.removeEventListener("change", onConnectionChange);
    };
  }, []);

  return (
    <NavContext.Provider value={nav}>
      <ErrorBoundary resetKey={route.path}>
        <App url={route.path} data={route.data} error={route.error} />
      </ErrorBoundary>
    </NavContext.Provider>
  );
}

// Load the initial route's chunk before hydrating so the tree matches the SSR
// output (the router renders from the module cache synchronously).
preloadRoute(location.pathname).then(() => {
  hydrateRoot(document.getElementById("app")!, <Router />);
});
