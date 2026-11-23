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

// How long a page waits for the importers it asked the server for
// (`applyInvalidate`) before it reloads instead.
var RESEND_WAIT = 10000;

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
    this.lastSeq = 0;
    // The last update applied (`lastSeq` counts one as soon as it is taken
    // in), and how many `notify` calls are waiting for their listeners.
    this.applied = 0;
    this.listening = 0;
    this.reloadPending = false;
    this.firstInvalidatedBy = undefined;
    // Waiting for an update to be taken in (`OjDevRuntime.ojPatched`).
    this.heard = [];
  }

  wake() {
    for (const resolve of this.heard.splice(0)) resolve();
  }

  async notify(event, data) {
    const cbs = this.listeners.get(event);
    if (!cbs) return;
    // An `import()` a listener awaits must not wait for the update this
    // call is part of, or for one queued behind it (`ojPatched`).
    this.listening++;
    this.wake();
    try {
      await Promise.allSettled(cbs.map((cb) => cb(data)));
    } finally {
      this.listening--;
    }
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
    this.pending.push(frame);
    this.queue = this.queue.then(() => this.applyPending()).catch((err) => this.failed(err, frame.changedIds));
  }

  invalidate(id, message) {
    console.debug(`[oj] invalidate ${id}${message ? `: ${message}` : ""}`);
    this.queue = this.queue.then(() => this.applyInvalidate(id)).catch((err) => this.failed(err, id));
  }

  // Vite's warnFailedUpdate: the page keeps running, the next edit retries.
  failed(err, path) {
    this.applied = this.lastSeq;
    this.wake();
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
    const changedIds = new Set();
    const announced = new Set();
    let update = null;
    // An update the server sent for an invalidation (`applyInvalidate`)
    // carries it on: every importer of the module that invalidated re-runs,
    // the ones the page had a factory for with the ones just sent (run
    // apart, a provider and its consumers would hold two copies of what
    // they share), and one that comes back to that module reloads, as it
    // does when the page re-runs the importers itself.
    let by;
    let answer;
    // What re-runs for the changes so far. An answer's importers walk up
    // with who invalidated; an edit that came with it walks up as any edit
    // does (one to the module that invalidated is no loop).
    const compute = () => {
      if (!answer) return this.compute([...changedIds]);
      const importers = this.runtime
        .getImporters(answer.id)
        .filter((p) => p !== answer.id && this.runtime.isExecuted(p));
      const asked = this.compute(importers, by);
      if (asked?.reload) return asked;
      const edits = [...changedIds].filter((id) => !answer.ids.has(id));
      const edited = edits.length ? this.compute(edits) : null;
      if (edited?.reload || !asked || !edited) return edited ?? asked;
      const known = new Set(asked.boundaries.map((b) => `${b.boundary}\n${b.acceptedVia}`));
      return {
        boundaries: [
          ...asked.boundaries,
          ...edited.boundaries.filter((b) => !known.has(`${b.boundary}\n${b.acceptedVia}`)),
        ],
        updateSet: [...new Set([...asked.updateSet, ...edited.updateSet])],
      };
    };
    while (this.pending.length) {
      const frames = this.pending.splice(0);
      for (const { seq, changedIds: ids } of frames) {
        if (seq !== this.lastSeq + 1)
          return this.requestReload(`update sequence gap (expected ${this.lastSeq + 1}, got ${seq})`);
        this.lastSeq = seq;
        for (const id of ids) changedIds.add(id);
      }
      this.wake();
      // Over every change so far: a boundary found by an earlier batch is
      // found again, and re-run once at the end.
      // Its answer ends the wait, alone or with an edit made meanwhile (the
      // bundler sends both as one update, and the page takes the updates
      // waiting for it together): another edit's update, arriving first,
      // leaves the page still asking.
      const resent = this.resent;
      if (resent && [...resent.ids].every((id) => changedIds.has(id))) {
        this.resent = undefined;
        answer = resent;
        by = resent.by;
      }
      const next = compute();
      if (next?.reload) return this.requestReload(next.reload);
      if (next) {
        update = next;
        const fresh = update.boundaries.filter((b) => !announced.has(`${b.boundary}\n${b.acceptedVia}`));
        for (const b of fresh) announced.add(`${b.boundary}\n${b.acceptedVia}`);
        if (fresh.length) {
          await this.notify("vite:beforeUpdate", this.payload(fresh, by));
          // A module a listener imported meanwhile runs here now.
          const again = compute();
          if (again?.reload) return this.requestReload(again.reload);
          update = again ?? update;
        }
        if (this.beforeApply() === "reload") return;
      }
      // A patch nothing here runs yet is imported all the same: it holds
      // the changed modules as they are now, and an earlier patch may have
      // left this page their code from before (the bundler sends a changed
      // module's importers along, loaded or not). Left with that, a later
      // `import()` of one would run it without asking the server. Under
      // Vite the change stops at a module no page loaded (hmr.ts
      // propagateUpdate) and its `import()` is a request, answered with
      // the module as it is then.
      for (const { url, changedIds: ids } of frames) {
        try {
          await import(/* @vite-ignore */ url);
        } catch {
          if (next) return this.requestReload(`failed to import hmr patch ${url}`);
          // Nothing to re-run, so nothing to reload for: without the
          // patch, the changed modules are asked for when first imported.
          for (const id of ids) if (!this.runtime.isExecuted(id)) this.runtime.factories?.delete(id);
        }
      }
    }
    if (update) await this.apply(update, by);
    this.applied = this.lastSeq;
    this.wake();
    if (update) await this.notify("vite:afterUpdate", this.payload(update.boundaries, by));
  }

  async applyInvalidate(id) {
    if (this.reloadPending) return;
    const firstInvalidatedBy = this.firstInvalidatedBy ?? id;
    // Not the module itself, as Vite's server leaves self-imports out of an
    // invalidation's importers (server/environment.ts `invalidateModule`): a
    // Fast Refresh module imports itself to compare its exports
    // (@vitejs/plugin-react's `import(import.meta.url)`), and walking up
    // from it would find the invalidation back where it started and reload.
    const importers = this.runtime.getImporters(id).filter((p) => p !== id && this.runtime.isExecuted(p));
    if (!importers.length) return this.requestReload(`no importers to handle the invalidation of ${id}`);
    const update = this.compute(importers, firstInvalidatedBy);
    if (!update) return;
    if (update.reload) return this.requestReload(update.reload);
    // The page runs a module again from its factory, and holds none for a
    // module of its first bundle (a patch or a lazy chunk registers one):
    // the server is asked for those, and sends them as an update, which
    // re-runs the same importers. Vite's client hands the invalidation to
    // its server the same way (`vite:invalidate`, answered with an update
    // for the importers).
    const missing = update.updateSet.filter((m) => !this.runtime.hasFactory(m));
    if (missing.length) {
      if (!__ojBus.send || !__ojBus.send({ type: "oj:invalidate", path: id, modules: missing })) {
        return this.requestReload(`no factory for module ${missing[0]}`);
      }
      const asked = (this.resent = { id, by: firstInvalidatedBy, ids: new Set(missing) });
      // The update may never come (the importer on disk does not compile,
      // or the server's bundler has no module under that name): the page
      // would stay on its old importers, where Vite reloads.
      if (typeof setTimeout === "function") {
        setTimeout(() => {
          if (this.resent !== asked) return;
          this.resent = undefined;
          this.requestReload(`the server sent no update for ${missing[0]}`);
        }, RESEND_WAIT);
      }
      return;
    }
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
    this.wake();
    console.debug(`[oj] full reload needed: ${reason}`);
    if (!__ojBus.send || !__ojBus.send({ type: "oj:reload-needed", reason })) location.reload();
  }
}

// Whether live-reload.js has run, once that is settled: it says so as it
// runs, and a document that finished loading without it never will (the tag
// was refused, or the document has none).
function ojStampsKnown() {
  if ("stamp" in __ojBus) return Promise.resolve(true);
  return new Promise((resolve) => {
    (__ojBus.onStamps ??= []).push(() => resolve(true));
    const loaded = () => resolve("stamp" in __ojBus);
    if (typeof document === "undefined" || document.readyState === "complete") loaded();
    else addEventListener("load", loaded, { once: true });
  });
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
  // The module behind an `import()`, as the dev server has it now
  // (rolldown's lazy boundary; the URL is the server's to choose). A URL of
  // its own for each request: the browser keeps a module by URL, a failed
  // one too.
  loadLazyChunk(proxyModuleId) {
    // Before live-reload.js has run, the request names neither of the
    // page's stamps (it is the last tag of the document, and the bundle
    // can import while the document still streams).
    const unnamed = !("stamp" in __ojBus);
    const chunk = this.ojImport(this.lazyChunkUrl(proxyModuleId));
    return unnamed ? chunk.then(() => this.ojAskedUnnamed(proxyModuleId)) : chunk;
  }
  // The answer to a request that named no stamp was made for a page that
  // has everything the server's newest output loads. A page behind a change
  // (one that went out between its document and this request) lacks what
  // that change made part of the output, and the module cannot run on it:
  // it is asked for again once the page can say what it has, and answered
  // then with all it lacks. Not waited for otherwise: the module of a page
  // that is behind nothing runs as soon as its chunk is there.
  async ojAskedUnnamed(proxyModuleId) {
    const id = this.lazyTargets?.get(proxyModuleId);
    if (id == null || this.isRunnableHere(id)) return;
    if (await ojStampsKnown()) await this.ojImport(this.lazyChunkUrl(proxyModuleId));
  }
  lazyChunkUrl(proxyModuleId) {
    this.lazyAsked = (this.lazyAsked ?? 0) + 1;
    // With the document's stamp: a page another run of the dev server
    // rendered is told to reload, not answered as one of this run's.
    const run = __ojBus.stamp == null ? "" : `&g=${encodeURIComponent(__ojBus.stamp)}`;
    // With the hot changes the page has, as its socket names them: a page
    // behind one (its patch held by the editor's gate, or missed while its
    // socket was down) lacks what that change made part of the bundle.
    const has = __ojBus.hotHas?.();
    const hot = has == null ? "" : `&h=${has}`;
    return `${__OJ_DEV_BASE__}/@oj-start/hmr/lazy?id=${encodeURIComponent(proxyModuleId)}&clientId=${this.clientId}&n=${this.lazyAsked}${run}${hot}`;
  }
  // A module with top-level `await` is an async factory, which rolldown's
  // runtime starts and does not wait for: its promise is kept, so an
  // `import()` of the module settles when it has finished evaluating, and
  // rejects if it threw, as the module's own `import()` does in a browser.
  registerFactory(id, factory, stamp) {
    if (this.heldSkip?.has(id)) return;
    const runtime = this;
    return super.registerFactory(
      id,
      function (moduleId) {
        const result = factory(moduleId);
        if (result && typeof result.then === "function") {
          const evaluating = (runtime.evaluating ??= new Map());
          evaluating.set(moduleId, result);
          result.then(
            () => evaluating.get(moduleId) === result && evaluating.delete(moduleId),
            () => {},
          );
        }
        return result;
      },
      stamp,
    );
  }
  removeModuleCache(id) {
    this.evaluating?.delete(id);
    super.removeModuleCache(id);
  }
  // rolldown's runtime keeps a failed request for good, as `import()` keeps
  // a module that threw. A module whose chunk never arrived (the server
  // could not answer) has run nothing, so the next `import()` asks again:
  // the request is dropped, if it is still the one that failed.
  requestLazy(id, proxyModuleId, isNodeMode) {
    (this.lazyTargets ??= new Map()).set(proxyModuleId, id);
    const pending = super.requestLazy(id, proxyModuleId, isNodeMode);
    const asked = this.lazyRequests.get(id);
    pending.catch(() => {
      if (!this.moduleCache.has(id) && this.lazyRequests.get(id) === asked) this.lazyRequests.delete(id);
    });
    return pending.then(async (exports) => {
      // The module, and the modules it imports statically, as far as they
      // are still evaluating (not everything that is: a module that awaits
      // this very import is).
      const seen = new Set([id]);
      for (const current of seen) {
        await this.evaluating?.get(current);
        for (const dep of this.staticImports?.get(current)?.edges ?? []) seen.add(dep);
      }
      return exports;
    });
  }
  // The dev server compiled a module for an `import()` after it sent this
  // page hot update number `seq`: the module must not run before the page
  // has applied that update (it was compiled against it, and a frame can
  // be slower than this answer). Bounded: a page whose socket is down gets
  // no update at all, and reloads when it is back.
  async ojPatched(seq) {
    const hmr = this.hmr;
    // Not while a listener is being waited for (`vite:beforeUpdate`, a
    // custom event): the update cannot go on until the listener returns,
    // so an `import()` the listener awaits would wait for itself. Under
    // Vite that import is an ordinary request.
    const ready = () => hmr.applied >= seq || hmr.reloadPending || hmr.listening > 0;
    if (ready()) return;
    // Woken as the page takes updates in (not by a timer of its own: a
    // hidden tab's timers are slowed), up to a bound.
    let timer;
    const late = new Promise((resolve) => (timer = setTimeout(resolve, 10000, "late")));
    for (;;) {
      const heard = new Promise((resolve) => hmr.heard.push(resolve));
      if (ready()) break;
      if ((await Promise.race([heard, late])) === "late") {
        console.warn(`[oj] hot update ${seq} did not reach this page in time; running the imported module without it`);
        break;
      }
    }
    clearTimeout(timer);
  }
  // The dev server compiled a module for an `import()` while the editor's
  // gate holds a change, and the chunk at `url` carries the modules `held`
  // of that change. If this page already runs them all, it takes the chunk,
  // which leaves them alone (`ojSkipHeld`): a module that ran is not run
  // again, so the chunk runs nothing of the held change. Otherwise it asks
  // for the module again, to be answered after the flush, as it does for a
  // kept chunk the server no longer has.
  async ojLazyWhileHeld(held, url, proxyModuleId) {
    if (!held.every((id) => this.moduleCache.has(id))) {
      return this.ojImport(this.lazyChunkUrl(proxyModuleId) + "&held=1");
    }
    try {
      await this.ojImport(url);
    } catch {
      // Not asked anew for another chunk to take (that answer would be
      // one of these again, and so on): the import waits for the flush.
      await this.ojImport(this.lazyChunkUrl(proxyModuleId) + "&held=1");
    }
  }
  ojImport(url) {
    return import(/* @vite-ignore */ url);
  }
  // Called by a chunk the page takes during a hold, right before its
  // registrations: for the rest of the chunk's evaluation (it is
  // synchronous from here) the factories and graph rows of `held` are not
  // taken. They are the held change's; the page keeps its own until the
  // patch the flush releases brings them, or already has that patch's if
  // the flush came while the chunk was on its way.
  ojSkipHeld(held) {
    // Ended by the chunk's last statement (`ojSkipHeld(null)`); the
    // microtask is for a chunk that throws before it gets there.
    this.heldSkip = held ? new Set(held) : null;
    if (held) queueMicrotask(() => (this.heldSkip = null));
  }
  registerGraph(delta) {
    const skip = this.heldSkip;
    if (!skip || !delta.ids.slice(0, delta.localCount).some((id) => skip.has(id))) return super.registerGraph(delta);
    // The same rows without the held modules': those move behind the
    // local ones, where they are only referred to.
    const local = [];
    const held = [];
    for (let i = 0; i < delta.localCount; i++) (skip.has(delta.ids[i]) ? held : local).push(i);
    const order = [...local, ...held];
    for (let i = delta.localCount; i < delta.ids.length; i++) order.push(i);
    const moved = new Map(order.map((from, to) => [from, to]));
    const rows = (of) => of && local.map((i) => of[i]);
    const refs = (of) => of && local.map((i) => (of[i] ?? []).map((j) => moved.get(j)));
    return super.registerGraph({
      ...delta,
      ids: order.map((i) => delta.ids[i]),
      localCount: local.length,
      edges: refs(delta.edges),
      dynamicEdges: refs(delta.dynamicEdges),
      bindings: rows(delta.bindings),
      stamps:
        delta.stamps &&
        Object.fromEntries(local.flatMap((i, to) => (i in delta.stamps ? [[to, delta.stamps[i]]] : []))),
      entries: delta.entries?.flatMap((i) => (local.includes(i) ? [local.indexOf(i)] : [])),
    });
  }
  payloadDelivered(filename) {
    __ojBus.send?.({ type: "oj:delivered", filename });
  }
  ojApply(frame) {
    this.hmr.push(frame);
  }
}

// The id starts with the tag of the engine that made this bundle: the dev
// server tells a page that runs its engine's output from one that runs
// another bundle (one restored from the cache, an earlier engine's). The
// tag is set by the entry's first import, a module of its own: written
// here, it would change this chunk's name, and with it the name of every
// chunk that imports this one, each time an engine starts.
globalThis.__rolldown_runtime__ ??= new OjDevRuntime(
  `${globalThis.__ojEngine ?? ""}.${Date.now().toString(36)}${Math.random().toString(36).slice(2, 10)}`,
);
__ojBus.runtime = globalThis.__rolldown_runtime__;
__ojBus.onRuntime?.(globalThis.__rolldown_runtime__);
