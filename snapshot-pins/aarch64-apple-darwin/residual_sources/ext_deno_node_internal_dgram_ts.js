"use strict"; return ((function() {
  const { core, primordials } = __bootstrap;
  const lazyDns = core.createLazyLoader("node:dns");
  // `nextTick()` silently drops callbacks until node:process has been
  // bootstrapped, so make sure it is loaded before scheduling one.
  const lazyProcess = core.createLazyLoader("node:process");
  const { ERR_SOCKET_BAD_TYPE } = core.loadExtScript("ext:deno_node/internal/errors.ts");
  const { UDP } = core.loadExtScript("ext:deno_node/internal_binding/udp_wrap.ts");
  const { guessHandleType } = core.loadExtScript("ext:deno_node/internal_binding/util.ts");
  const { codeMap } = core.loadExtScript("ext:deno_node/internal_binding/uv.ts");
  const { isInt32, validateFunction } = core.loadExtScript("ext:deno_node/internal/validators.mjs");
  const { nextTick } = core.loadExtScript("ext:deno_node/_next_tick.ts");
  const { isIP } = core.loadExtScript("ext:deno_node/internal/net.ts");
  const { FunctionPrototypeBind, MapPrototypeGet, Symbol } = primordials;
  const kStateSymbol = Symbol("kStateSymbol");
  function lookup4(lookup, address, callback) {
    return lookup(address || "127.0.0.1", 4, callback);
  }
  function lookup6(lookup, address, callback) {
    return lookup(address || "::1", 6, callback);
  }
  function defaultLookup(address, family, callback) {
    if (isIP(address) === family) {
      lazyProcess();
      nextTick(callback, null, address, family);
      return;
    }
    return lazyDns().default.lookup(address, family, callback);
  }
  function newHandle(type, lookup) {
    if (lookup === undefined) {
      lookup = defaultLookup;
    } else {
      validateFunction(lookup, "lookup");
    }
    if (type === "udp4") {
      const handle = new UDP();
      handle.lookup = FunctionPrototypeBind(lookup4, handle, lookup);
      return handle;
    }
    if (type === "udp6") {
      const handle = new UDP();
      handle.lookup = FunctionPrototypeBind(lookup6, handle, lookup);
      handle.bind = handle.bind6;
      handle.connect = handle.connect6;
      handle.send = handle.send6;
      return handle;
    }
    throw new ERR_SOCKET_BAD_TYPE();
  }
  function _createSocketHandle(address, port, addressType, fd, flags) {
    const handle = newHandle(addressType);
    let err;
    if (isInt32(fd) && fd > 0) {
      const type = guessHandleType(fd);
      if (type !== "UDP") {
        err = MapPrototypeGet(codeMap, "EINVAL");
      } else {
        err = handle.open(fd);
      }
    } else if (port || address) {
      // deno-lint-ignore deno-internal/prefer-primordials
      err = handle.bind(address, port || 0, flags);
    }
    if (err) {
      handle.close();
      return err;
    }
    return handle;
  }
  return {
    default: {
      kStateSymbol,
      newHandle,
      _createSocketHandle
    },
    kStateSymbol,
    newHandle,
    _createSocketHandle
  };
})());