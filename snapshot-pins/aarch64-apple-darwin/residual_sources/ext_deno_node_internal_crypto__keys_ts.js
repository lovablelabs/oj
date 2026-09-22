"use strict"; return ((function() {
  const { core, primordials } = __bootstrap;
  const { Symbol } = primordials;
  const { kKeyObject } = core.loadExtScript("ext:deno_node/internal/crypto/constants.ts");
  const kKeyType = Symbol("kKeyType");
  function isKeyObject(obj) {
    return obj != null && obj[kKeyType] !== undefined;
  }
  function isCryptoKey(obj) {
    return obj != null && obj[kKeyObject] !== undefined;
  }
  return {
    kKeyType,
    isKeyObject,
    isCryptoKey
  };
})());