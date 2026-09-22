"use strict"; return ((function() {
  const { core, primordials } = __bootstrap;
  const { op_node_random_int } = core.ops;
  const { ERR_INVALID_ARG_TYPE, ERR_OUT_OF_RANGE } = core.loadExtScript("ext:deno_node/internal/errors.ts");
  const { validateFunction } = core.loadExtScript("ext:deno_node/internal/validators.mjs");
  const { MathCeil, MathFloor, NumberIsSafeInteger } = primordials;
  // Largest integer that can be expressed in 6 bytes, mirrors Node's RAND_MAX
  // in lib/internal/crypto/random.js.
  const RAND_MAX = 0xFFFF_FFFF_FFFF;
  // Generates an integer in [min, max) range where min is inclusive and max is
  // exclusive. Matches Node's lib/internal/crypto/random.js randomInt().
  function randomInt(min, max, callback) {
    // Detect optional min syntax
    // randomInt(max)
    // randomInt(max, callback)
    const minNotSpecified = typeof max === "undefined" || typeof max === "function";
    if (minNotSpecified) {
      callback = max;
      max = min;
      min = 0;
    }
    const isSync = typeof callback === "undefined";
    if (!isSync) {
      validateFunction(callback, "callback");
    }
    if (!NumberIsSafeInteger(min)) {
      throw new ERR_INVALID_ARG_TYPE("min", "a safe integer", min);
    }
    if (!NumberIsSafeInteger(max)) {
      throw new ERR_INVALID_ARG_TYPE("max", "a safe integer", max);
    }
    if (max <= min) {
      throw new ERR_OUT_OF_RANGE("max", `greater than the value of "min" (${min})`, max);
    }
    const range = max - min;
    if (!(range <= RAND_MAX)) {
      throw new ERR_OUT_OF_RANGE(`max${minNotSpecified ? "" : " - min"}`, `<= ${RAND_MAX}`, range);
    }
    min = MathCeil(min);
    const result = op_node_random_int(min, MathFloor(max));
    if (!isSync) {
      callback(null, result);
      return;
    }
    return result;
  }
  return {
    default: randomInt
  };
})());