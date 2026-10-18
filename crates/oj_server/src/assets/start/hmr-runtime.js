// SPDX-License-Identifier: MIT

// The Start dev client's HMR runtime: rolldown's `devMode.implement`, so it is
// inlined into the client bundle right after rolldown's own `DevRuntime`
// (a store and executor that makes no HMR decisions). The decisions follow
// Vite's bundled dev client (client/bundledDevHmrClient.ts): a pushed patch
// registers fresh factories, the change bubbles up the importers to the
// modules that accept it, those re-run, and no boundary means a full reload.
// The transport is the page's /@oj-start/hmr socket, owned by live-reload.js
// and reached through `globalThis.__ojStartHmr`: either script may load first.

var __ojBus = (globalThis.__ojStartHmr ??= {});

class OjHotContext {
  constructor(client, id) {
    this.client = client;
    this.id = id;
    if (!client.dataMap.has(id)) client.dataMap.set(id, {});
    // A re-executed module registers its callbacks and listeners again
    // (Vite's HMRContext drops the stale ones by owner).
    const mod = client.hotModules.get(id);
    if (mod) mod.callbacks = [];
    const stale = client.ownListeners.get(id);
    if (stale) {
      for (const [event, fns] of stale) {
        const list = client.listeners.get(event);
        if (list)
          client.listeners.set(
            event,
            list.filter((l) => !fns.includes(l)),
          );
      }
    }
    this.own = new Map();
    client.ownListeners.set(id, this.own);
  }
  get data() {
    return this.client.dataMap.get(this.id);
  }
  accept(deps, callback) {
    if (typeof deps === "function" || !deps) this.acceptDeps([this.id], ([mod]) => deps?.(mod));
    else if (typeof deps === "string") this.acceptDeps([deps], ([mod]) => callback?.(mod));
    else if (Array.isArray(deps)) this.acceptDeps(deps, callback);
    else throw new Error("invalid hot.accept() usage.");
  }
  acceptExports(_, callback) {
    this.acceptDeps([this.id], ([mod]) => callback?.(mod));
  }
  acceptDeps(deps, fn = () => {}) {
    const mod = this.client.hotModules.get(this.id) || { id: this.id, callbacks: [] };
    mod.callbacks.push({ deps, fn });
    this.client.hotModules.set(this.id, mod);
  }
  dispose(cb) {
    this.client.disposeMap.set(this.id, cb);
  }
  prune(cb) {
    this.client.pruneMap.set(this.id, cb);
  }
  decline() {}
  // Bundled dev never sends `vite:invalidate` to the server: no rebuild
  // happens, the page's runtime re-runs the importers itself.
  invalidate(message) {
    this.client.notify("vite:invalidate", {
      path: this.id,
      message,
      firstInvalidatedBy: this.client.firstInvalidatedBy ?? this.id,
    });
    this.client.invalidate(this.id, message);
  }
  on(event, cb) {
    const add = (map) => {
      const list = map.get(event) || [];
      list.push(cb);
      map.set(event, list);
    };
    add(this.client.listeners);
    add(this.own);
  }
  off(event, cb) {
    const drop = (map) => {
      const list = map.get(event);
      if (!list) return;
      const pruned = list.filter((l) => l !== cb);
      if (pruned.length) map.set(event, pruned);
      else map.delete(event);
    };
    drop(this.client.listeners);
    drop(this.own);
  }
  // To the dev server's `server.ws.on(event)` listeners; one sent before the
  // socket (or live-reload.js) is up waits for it, as Vite's transport does.
  send(event, data) {
    const msg = { type: "custom", event, data };
    if (__ojBus.post) __ojBus.post(msg);
    else (__ojBus.outbox ??= []).push(msg);
  }
}

class OjHmrClient {
  constructor(runtime) {
    this.runtime = runtime;
    this.hotModules = new Map();
    this.disposeMap = new Map();
    this.pruneMap = new Map();
    this.dataMap = new Map();
    this.listeners = new Map();
    // Each module's own listeners, by event (Vite's ctxToListenersMap).
    this.ownListeners = new Map();
    this.queue = Promise.resolve();
    this.lastSeq = 0;
    this.reloadPending = false;
    this.firstInvalidatedBy = undefined;
  }

  async notify(event, data) {
    const cbs = this.listeners.get(event);
    if (cbs) await Promise.allSettled(cbs.map((cb) => cb(data)));
  }

  selfAccepted(id) {
    return this.hotModules.get(id)?.callbacks.some((c) => c.deps.includes(id)) ?? false;
  }

  acceptsDep(parent, id) {
    return this.hotModules.get(parent)?.callbacks.some((c) => c.deps.includes(id)) ?? false;
  }

  // Vite's computeHmrUpdate: the modules to re-run and the boundaries that
  // accept them, or the reason a full reload is needed.
  compute(changedIds, firstInvalidatedBy) {
    const boundaries = [];
    const updateSet = new Set();
    const seen = new Set();
    for (const id of changedIds) {
      if (!this.runtime.isExecuted(id)) continue;
      const reload = this.bubble(id, [id], updateSet, boundaries, firstInvalidatedBy, seen);
      if (reload) return { reload };
    }
    return boundaries.length ? { boundaries, updateSet: [...updateSet] } : null;
  }

  bubble(id, stack, updateSet, boundaries, firstInvalidatedBy, seen) {
    if (seen.has(id)) return;
    seen.add(id);
    updateSet.add(id);
    if (firstInvalidatedBy !== undefined && id === firstInvalidatedBy) {
      return `update propagated back to ${firstInvalidatedBy}, which called import.meta.hot.invalidate()`;
    }
    if (this.selfAccepted(id)) {
      boundaries.push({ boundary: id, acceptedVia: id, circular: this.circular(id, stack) });
      return;
    }
    const parents = this.runtime.getImporters(id).filter((p) => this.runtime.isExecuted(p));
    if (!parents.length) return `no hmr boundary found for module ${id}`;
    for (const parent of parents) {
      const chain = [...stack, parent];
      if (this.acceptsDep(parent, id)) {
        boundaries.push({ boundary: parent, acceptedVia: id, circular: this.circular(parent, chain) });
        continue;
      }
      if (!stack.includes(parent)) {
        const reload = this.bubble(parent, chain, updateSet, boundaries, firstInvalidatedBy, seen);
        if (reload) return reload;
      }
    }
  }

  // An accepted module inside an import loop cannot recover its execution
  // order (Vite's isNodeWithinCircularImports).
  circular(node, nodeChain, currentChain = [node], seen = new Set()) {
    if (seen.has(node)) return false;
    seen.add(node);
    for (const importer of this.runtime.getImporters(node)) {
      if (importer === node) continue;
      if (nodeChain.includes(importer)) return true;
      if (!currentChain.includes(importer) && this.circular(importer, nodeChain, currentChain.concat(importer), seen)) {
        return true;
      }
    }
    return false;
  }

  push(frame) {
    this.queue = this.queue.then(() => this.applyPush(frame)).catch((err) => this.failed(err, frame.changedIds));
  }

  invalidate(id, message) {
    console.debug(`[oj] invalidate ${id}${message ? `: ${message}` : ""}`);
    this.queue = this.queue.then(() => this.applyInvalidate(id)).catch((err) => this.failed(err, id));
  }

  // Vite's warnFailedUpdate: the page keeps running, the next edit retries.
  failed(err, path) {
    if (!(err instanceof Error) || !err.message.includes("fetch")) console.error("[oj]", err);
    console.error(
      `[oj] Failed to reload ${path}. This could be due to syntax errors or importing non-existent modules. (see errors above)`,
    );
  }

  // The page opened on a failed build, so its own bundle never ran: only a
  // reload recovers it. Otherwise the update clears the error overlay.
  beforeApply() {
    return __ojBus.beforeApply ? __ojBus.beforeApply() : "continue";
  }

  payload(boundaries, firstInvalidatedBy) {
    return {
      type: "update",
      updates: boundaries.map((b) => ({
        type: "js-update",
        path: b.boundary,
        acceptedPath: b.acceptedVia,
        timestamp: Date.now(),
        isWithinCircularImport: b.circular,
        firstInvalidatedBy,
      })),
    };
  }

  async applyPush({ url, seq, changedIds }) {
    if (this.reloadPending) return;
    if (seq !== this.lastSeq + 1)
      return this.requestReload(`update sequence gap (expected ${this.lastSeq + 1}, got ${seq})`);
    this.lastSeq = seq;
    const update = this.compute(changedIds);
    if (!update) return;
    if (update.reload) return this.requestReload(update.reload);
    const payload = this.payload(update.boundaries, undefined);
    await this.notify("vite:beforeUpdate", payload);
    if (this.beforeApply() === "reload") return;
    try {
      await import(/* @vite-ignore */ url);
    } catch {
      return this.requestReload(`failed to import hmr patch ${url}`);
    }
    await this.apply(update);
    await this.notify("vite:afterUpdate", payload);
  }

  async applyInvalidate(id) {
    if (this.reloadPending) return;
    const firstInvalidatedBy = this.firstInvalidatedBy ?? id;
    const importers = this.runtime.getImporters(id).filter((p) => this.runtime.isExecuted(p));
    if (!importers.length) return this.requestReload(`no importers to handle the invalidation of ${id}`);
    const update = this.compute(importers, firstInvalidatedBy);
    if (!update) return;
    if (update.reload) return this.requestReload(update.reload);
    const payload = this.payload(update.boundaries, firstInvalidatedBy);
    await this.notify("vite:beforeUpdate", payload);
    if (this.beforeApply() === "reload") return;
    await this.apply(update, firstInvalidatedBy);
    await this.notify("vite:afterUpdate", payload);
  }

  async apply(update, firstInvalidatedBy) {
    for (const id of update.updateSet) {
      if (!this.runtime.hasFactory(id)) return this.requestReload(`no factory for module ${id}`);
    }
    // Collected before the caches drop: re-execution registers new ones.
    const applies = update.boundaries.map((b) => ({
      ...b,
      callbacks: this.hotModules.get(b.boundary)?.callbacks.filter((c) => c.deps.includes(b.acceptedVia)) ?? [],
    }));
    for (const id of update.updateSet) this.runtime.removeModuleCache(id);
    for (const { boundary, acceptedVia, circular, callbacks } of applies) {
      let fresh;
      try {
        this.runtime.initModule(acceptedVia);
        fresh = this.runtime.loadExports(acceptedVia);
      } catch (err) {
        if (circular) return this.requestReload(`${acceptedVia} is within a circular import`);
        throw err;
      }
      try {
        this.firstInvalidatedBy = firstInvalidatedBy;
        for (const { deps, fn } of callbacks) fn(deps.map((dep) => (dep === acceptedVia ? fresh : undefined)));
      } finally {
        this.firstInvalidatedBy = undefined;
      }
      console.debug(`[oj] hot updated: ${boundary === acceptedVia ? boundary : `${acceptedVia} via ${boundary}`}`);
    }
  }

  disposeModule(id) {
    const dispose = this.disposeMap.get(id);
    if (dispose) dispose(this.dataMap.get(id));
  }

  // The server reloads the page once its bundle output covers every change
  // (a reload straight away could load the bundle from before this edit).
  requestReload(reason) {
    if (this.reloadPending) return;
    this.reloadPending = true;
    console.debug(`[oj] full reload needed: ${reason}`);
    if (!__ojBus.send || !__ojBus.send({ type: "oj:reload-needed", reason })) location.reload();
  }
}

class OjDevRuntime extends DevRuntime {
  constructor(clientId) {
    super(clientId);
    const hmr = (this.hmr = new OjHmrClient(this));
    this.hooks = {
      createModuleHotContext: (id) => new OjHotContext(hmr, id),
      onModuleCacheRemoval: (id) => hmr.disposeModule(id),
    };
  }
  payloadDelivered(filename) {
    __ojBus.send?.({ type: "oj:delivered", filename });
  }
  ojApply(frame) {
    this.hmr.push(frame);
  }
}

globalThis.__rolldown_runtime__ ??= new OjDevRuntime(Date.now().toString(36) + Math.random().toString(36).slice(2, 10));
__ojBus.runtime = globalThis.__rolldown_runtime__;
__ojBus.onRuntime?.(globalThis.__rolldown_runtime__);
