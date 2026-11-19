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
    // Patches announced and not yet applied.
    this.pending = [];
    // Modules a hot update changed or re-ran since the page loaded.
    this.outdated = new Set();
    // Modules whose patch failed to load: a chunk naming one cannot be
    // brought up to date in place.
    this.unpatched = new Set();
    // Chunks built before a hot update, or holding a module a patch factory
    // already ran, that ran and wait for refreshStale.
    this.staleChunks = [];
    // The page's location when the last update came.
    this.updatedAt = undefined;
    this.deferredReload = undefined;
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
    for (const id of frame.changedIds ?? []) this.outdated.add(id);
    this.updatedAt = location.href;
    this.pending.push(frame);
    this.queue = this.queue.then(() => this.applyPending()).catch((err) => this.failed(err, frame.changedIds));
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

  // Every patch in hand is one update: the editor's gate releases the
  // patches of one change together (one per file it wrote, each built
  // before the next write), and a page far from its server is sent the
  // next while it still fetches this one. Applied one by one, as sent, a
  // module re-run by the first would bind to another module's exports as
  // they were before the edit (an export added in the same change is
  // `undefined` there; a render that throws on it unmounts the page, and
  // the next patch has nothing left to refresh). Vite invalidates every
  // changed file in its module graph as the watcher reports it
  // (server/index.ts onFileChange, hmr.ts updateModules), so the boundary
  // its update re-imports loads each import fresh, and its client fetches
  // every update queued in one tick before it runs any accept callback
  // (shared/hmr.ts queueUpdate); here the patches are fetched first, then
  // the changed modules re-run once, over all of them. Each batch keeps
  // Vite's order for one update (client.ts `case 'update'`): the listeners
  // hear vite:beforeUpdate, an overlay up before the first update reloads
  // instead, and only then is the patch fetched.
  async applyPending() {
    if (this.reloadPending || !this.pending.length) return;
    if (this.deferredReload) return this.deferredReload();
    const changedIds = new Set();
    const announced = new Set();
    let update = null;
    while (this.pending.length) {
      const frames = this.pending.splice(0);
      for (const { seq, changedIds: ids } of frames) {
        if (seq !== this.lastSeq + 1)
          return this.requestReload(`update sequence gap (expected ${this.lastSeq + 1}, got ${seq})`);
        this.lastSeq = seq;
        for (const id of ids) changedIds.add(id);
      }
      // Over every change so far: a boundary found by an earlier batch is
      // found again, and re-run once at the end.
      const next = this.compute([...changedIds]);
      if (next?.reload) return this.requestReload(next.reload);
      if (next) {
        update = next;
        const fresh = update.boundaries.filter((b) => !announced.has(`${b.boundary}\n${b.acceptedVia}`));
        for (const b of fresh) announced.add(`${b.boundary}\n${b.acceptedVia}`);
        if (fresh.length) await this.notify("vite:beforeUpdate", this.payload(fresh, undefined));
        if (this.beforeApply() === "reload") return;
      }
      // Imported even when none of its modules ran here: a lazy chunk built
      // before it runs its modules' factories instead (refreshStale).
      for (const { url, changedIds: ids } of frames) {
        try {
          await import(/* @vite-ignore */ url);
          for (const id of ids) this.unpatched.delete(id);
        } catch {
          if (update) return this.requestReload(`failed to import hmr patch ${url}`);
          for (const id of ids) this.unpatched.add(id);
        }
      }
    }
    if (!update) return;
    await this.apply(update);
    await this.notify("vite:afterUpdate", this.payload(update.boundaries, undefined));
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

  async apply(update, firstInvalidatedBy, reload = (why) => this.requestReload(why)) {
    for (const id of update.updateSet) {
      if (!this.runtime.hasFactory(id)) return reload(`no factory for module ${id}`);
    }
    // Collected before the caches drop: re-execution registers new ones.
    const applies = update.boundaries.map((b) => ({
      ...b,
      callbacks: this.hotModules.get(b.boundary)?.callbacks.filter((c) => c.deps.includes(b.acceptedVia)) ?? [],
    }));
    for (const id of update.updateSet) {
      this.outdated.add(id);
      this.runtime.removeModuleCache(id);
      // Its factory runs now: a chunk waiting with an older copy keeps this
      // one, and the page's instance it held is disposed.
      for (const c of this.staleChunks) {
        c.fresh.add(id);
        c.before.get(id)?.hot.dispose?.(this.dataMap.get(id));
        c.before.delete(id);
      }
    }
    for (const { boundary, acceptedVia, circular, callbacks } of applies) {
      let fresh;
      try {
        this.runtime.initModule(acceptedVia);
        fresh = this.runtime.loadExports(acceptedVia);
      } catch (err) {
        if (circular) return reload(`${acceptedVia} is within a circular import`);
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

  // Re-runs on their patch factories the stale modules of chunks from before
  // a hot update; what cannot re-run reloads.
  async refreshStale() {
    if (this.reloadPending || !this.staleChunks.length) return;
    // A patch announced since: its factories register first.
    if (this.pending.length) {
      this.queue = this.queue.then(() => this.refreshStale()).catch((err) => this.failed(err, "stale chunk"));
      return;
    }
    const chunks = this.staleChunks.splice(0);
    const locals = chunks.flatMap((c) => c.locals);
    const edges = chunks.flatMap((c) => c.edges);
    const before = new Map(chunks.flatMap((c) => [...c.before]));
    const fresh = new Set(chunks.flatMap((c) => [...c.fresh]));
    for (const [id, { mod, hot }] of before) {
      this.runtime.moduleCache.set(id, mod);
      this.restoreHot(id, hot);
    }
    // A module that ran here before the chunk is the page's instance; the
    // chunk's own copy of it is stale for what imports it.
    const stale = new Set([...this.outdated, ...before.keys()]);
    for (let grew = true; grew;) {
      grew = false;
      locals.forEach((id, i) => {
        if (stale.has(id) || !edges[i].some((dep) => stale.has(dep))) return;
        stale.add(id);
        grew = true;
      });
    }
    // What a patch re-ran while the chunks waited is current already.
    const ids = locals.filter((id) => stale.has(id) && !before.has(id) && !fresh.has(id));
    if (!ids.length) return;
    const reads = new Set([...locals, ...edges.flat()]);
    const unpatched = [...reads].find((id) => stale.has(id) && this.unpatched.has(id));
    if (unpatched !== undefined) return this.reloadSelf(`the hot patch of ${unpatched} failed to load`);
    const imported = (id) => this.runtime.getImporters(id).some((p) => p !== id && this.runtime.isExecuted(p));
    // What the current code no longer imports does not run again.
    const live = ids.filter(imported);
    // A chunk that imports a dropped copy later is stale too.
    for (const id of ids) if (!live.includes(id)) this.outdated.add(id);
    if (live.length) {
      const update = this.compute(live);
      const missing = update?.updateSet?.find((id) => !this.runtime.hasFactory(id));
      const outside = update?.boundaries?.find((b) => b.boundary !== b.acceptedVia || !locals.includes(b.boundary));
      if (!update || update.reload || missing !== undefined || outside !== undefined) {
        const why =
          update?.reload ??
          (missing !== undefined ? `no factory for ${missing}` : `${outside?.boundary ?? ids[0]} accepts it`);
        return this.reloadSelf(`a chunk built before a hot update loaded (${why})`);
      }
      // A boundary another updated module imports re-runs through that module.
      const imports = (id) => (this.runtime.staticImports.get(id)?.edges ?? []).filter((dep) => dep !== id);
      const tops = update.boundaries.filter((b) => !update.updateSet.some((id) => imports(id).includes(b.boundary)));
      const reached = new Set();
      const queue = tops.map((b) => b.boundary);
      for (const id of queue) {
        if (reached.has(id) || !update.updateSet.includes(id)) continue;
        reached.add(id);
        queue.push(...imports(id));
      }
      if (tops.length && update.updateSet.every((id) => reached.has(id))) update.boundaries = tops;
      // A stale module only the current code's new imports reach runs again
      // on its factory, not from the chunk's copy.
      for (const id of ids) if (!live.includes(id)) this.runtime.removeModuleCache(id);
      try {
        await this.apply(update, undefined, (why) => this.reloadSelf(why, true));
      } catch (err) {
        console.error("[oj]", err);
        // The update's modules have left the cache: no old code is left to keep.
        return this.reloadSelf(`re-running ${ids[0]} on its current code threw`, true);
      }
    }
    // The chunks' modules nothing running imports any more (Vite's prune).
    const paths = locals.filter(
      (id) =>
        !before.has(id) &&
        !this.runtime.isEntry(id) &&
        (this.runtime.isExecuted(id) ? !imported(id) : ids.includes(id) && !live.includes(id)),
    );
    if (!paths.length) return;
    await this.notify("vite:beforePrune", { type: "prune", paths });
    for (const id of paths) if (this.runtime.isExecuted(id)) this.runtime.removeModuleCache(id);
    for (const id of paths) this.pruneMap.get(id)?.(this.dataMap.get(id));
    for (const id of paths) this.dataMap.delete(id);
  }

  // A module's accept callbacks, listeners and dispose and prune handlers.
  saveHot(id) {
    return {
      mod: this.hotModules.get(id),
      callbacks: this.hotModules.get(id)?.callbacks,
      own: this.ownListeners.get(id),
      dispose: this.disposeMap.get(id),
      prune: this.pruneMap.get(id),
    };
  }

  restoreHot(id, hot) {
    // The chunk's copy cleared and refilled the same entry's callbacks.
    if (hot.mod) hot.mod.callbacks = hot.callbacks;
    const dispose = this.disposeMap.get(id);
    if (dispose && dispose !== hot.dispose) dispose(this.dataMap.get(id));
    const own = this.ownListeners.get(id);
    if (own && own !== hot.own) {
      for (const [event, fns] of own) {
        const list = (this.listeners.get(event) ?? []).filter((l) => !fns.includes(l));
        if (list.length) this.listeners.set(event, list);
        else this.listeners.delete(event);
      }
      for (const [event, fns] of hot.own ?? [])
        this.listeners.set(event, [...(this.listeners.get(event) ?? []), ...fns]);
    }
    for (const [map, value] of [
      [this.hotModules, hot.mod],
      [this.ownListeners, hot.own],
      [this.disposeMap, hot.dispose],
      [this.pruneMap, hot.prune],
    ]) {
      if (value === undefined) map.delete(id);
      else map.set(id, value);
    }
  }

  // This page alone reloads; its document waits for the current output. A
  // page still on the location it had at the last update reloads at its next
  // navigation, or at the next update.
  reloadSelf(reason, now = false) {
    if (this.reloadPending) return;
    if (this.deferredReload) {
      if (now) this.deferredReload();
      return;
    }
    console.debug(`[oj] full reload needed: ${reason}`);
    this.reloadReason = reason;
    const reload = async () => {
      if (this.reloadPending) return;
      this.reloadPending = true;
      await this.notify("vite:beforeFullReload", { type: "full-reload", path: "*" });
      location.reload();
    };
    if (now || location.href !== this.updatedAt) return void reload();
    this.deferredReload = reload;
    const from = location.href;
    const moved = () => {
      if (location.href !== from) reload();
    };
    for (const name of ["pushState", "replaceState"]) {
      const original = history[name];
      history[name] = function (...args) {
        const result = original.apply(history, args);
        moved();
        return result;
      };
    }
    addEventListener("popstate", moved);
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
    // Factories running: what they register is a patch's code.
    this.factoryRuns = 0;
  }
  initModule(id) {
    this.factoryRuns++;
    try {
      return super.initModule(id);
    } finally {
      this.factoryRuns--;
    }
  }
  // A full-build chunk from before a hot update (patches carry stamps):
  // patch rows win, and its stale modules re-run after it (refreshStale).
  registerGraph(delta) {
    if (delta.stamps !== undefined || this.factoryRuns > 0) return super.registerGraph(delta);
    const hmr = this.hmr;
    const locals = delta.ids.slice(0, delta.localCount);
    const waiting = new Set(hmr.staleChunks.flatMap((c) => c.locals));
    const ran = locals.filter((id) => this.moduleCache.has(id));
    // Queued when a module of it already ran, or it reads an outdated or queued module.
    if (ran.length || delta.ids.some((id) => hmr.outdated.has(id) || waiting.has(id))) {
      const edges = delta.edges.map((row) => row.map((j) => delta.ids[j]));
      const before = new Map(ran.map((id) => [id, { mod: this.moduleCache.get(id), hot: hmr.saveHot(id) }]));
      // After every chunk of this evaluation, behind the update queue.
      if (!hmr.staleChunks.push({ locals, edges, before, fresh: new Set() }) - 1) {
        queueMicrotask(() => {
          hmr.queue = hmr.queue.then(() => hmr.refreshStale()).catch((err) => hmr.failed(err, "stale chunk"));
        });
      }
    }
    // A patch's rows are newer than the chunk's: its factory is what runs.
    const kept = locals.filter((id) => !this.rowStamps.has(id));
    if (kept.length === locals.length) return super.registerGraph(delta);
    const ids = [...kept, ...delta.ids.filter((id) => !kept.includes(id))];
    const at = new Map(ids.map((id, i) => [id, i]));
    const row = (i) => at.get(delta.ids[i]);
    const rows = kept.map((id) => locals.indexOf(id));
    super.registerGraph({
      ids,
      localCount: kept.length,
      edges: rows.map((i) => delta.edges[i].map(row)),
      bindings:
        delta.bindings &&
        Object.fromEntries(rows.flatMap((i, k) => (delta.bindings[i] ? [[k, delta.bindings[i]]] : []))),
      dynamicEdges:
        delta.dynamicEdges &&
        Object.fromEntries(
          rows.flatMap((i, k) => (delta.dynamicEdges[i] ? [[k, delta.dynamicEdges[i].map(row)]] : [])),
        ),
      entries: delta.entries?.map((i) => rows.indexOf(i)).filter((k) => k !== -1),
    });
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
