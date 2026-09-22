"use strict"; return ((function() {
  const { core, primordials } = __bootstrap;
  const { timingSafeEqual } = core.loadExtScript("ext:deno_node/internal_binding/_timingSafeEqual.ts");
  const { Error } = primordials;
  function getFipsCrypto() {
    return false;
  }
  function setFipsCrypto(_fips) {
    throw new Error("FIPS mode is not supported in Deno.");
  }
  return {
    timingSafeEqual,
    getFipsCrypto,
    setFipsCrypto
  };
})());