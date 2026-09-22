"use strict"; return ((function() {
  const { core, primordials } = __bootstrap;
  const { Date, DatePrototypeToUTCString, DatePrototypeGetMilliseconds, Symbol } = primordials;
  let utcCache;
  function utcDate() {
    if (!utcCache) cache();
    return utcCache;
  }
  function cache() {
    const d = new Date();
    utcCache = DatePrototypeToUTCString(d);
    core.createSystemTimer(resetCache, 1000 - DatePrototypeGetMilliseconds(d));
  }
  function resetCache() {
    utcCache = undefined;
  }
  const kOutHeaders = Symbol("kOutHeaders");
  const kNeedDrain = Symbol("kNeedDrain");
  const _defaultExport = {
    utcDate,
    kOutHeaders,
    kNeedDrain
  };
  return {
    utcDate,
    kOutHeaders,
    kNeedDrain,
    default: _defaultExport
  };
})());