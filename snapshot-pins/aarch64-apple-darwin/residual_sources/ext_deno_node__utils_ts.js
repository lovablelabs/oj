"use strict"; return ((function() {
  const { core, primordials } = __bootstrap;
  const { Error, PromisePrototypeThen, ArrayPrototypePop, NumberIsInteger, ObjectGetOwnPropertyNames, ReflectGetOwnPropertyDescriptor, ObjectDefineProperty, NumberIsSafeInteger, FunctionPrototypeApply, SafeArrayIterator } = primordials;
  const { TextDecoder, TextEncoder } = core.loadExtScript("ext:deno_web/08_text_encoding.js");
  const { errorMap } = core.loadExtScript("ext:deno_node/internal_binding/uv.ts");
  const { codes } = core.loadExtScript("ext:deno_node/internal/error_codes.ts");
  const { ERR_NOT_IMPLEMENTED } = core.loadExtScript("ext:deno_node/internal/errors.ts");
  const { validateNumber } = core.loadExtScript("ext:deno_node/internal/validators.mjs");
  function notImplemented(msg) {
    throw new ERR_NOT_IMPLEMENTED(msg);
  }
  function warnNotImplemented(msg) {
    const message = msg ? `Warning: Not implemented: ${msg}` : "Warning: Not implemented";
    // deno-lint-ignore no-console
    console.warn(message);
  }
  const _TextDecoder = TextDecoder;
  const _TextEncoder = TextEncoder;
  function intoCallbackAPI(// deno-lint-ignore no-explicit-any
  func, cb, // deno-lint-ignore no-explicit-any
  ...args) {
    PromisePrototypeThen(func(...new SafeArrayIterator(args)), (value)=>cb && cb(null, value), (err)=>cb && cb(err));
  }
  function intoCallbackAPIWithIntercept(// deno-lint-ignore no-explicit-any
  func, interceptor, cb, // deno-lint-ignore no-explicit-any
  ...args) {
    PromisePrototypeThen(func(...new SafeArrayIterator(args)), (value)=>cb && cb(null, interceptor(value)), (err)=>cb && cb(err));
  }
  function spliceOne(list, index) {
    for(; index + 1 < list.length; index++){
      list[index] = list[index + 1];
    }
    ArrayPrototypePop(list);
  }
  function validateIntegerRange(value, name, min = -2147483648, max = 2147483647) {
    // The defaults for min and max correspond to the limits of 32-bit integers.
    if (!NumberIsInteger(value)) {
      throw new Error(`${name} must be 'an integer' but was ${value}`);
    }
    if (value < min || value > max) {
      throw new Error(`${name} must be >= ${min} && <= ${max}. Value was ${value}`);
    }
  }
  function once(callback) {
    let called = false;
    return function(...args) {
      if (called) return;
      called = true;
      FunctionPrototypeApply(callback, this, args);
    };
  }
  function makeMethodsEnumerable(klass) {
    const proto = klass.prototype;
    const names = ObjectGetOwnPropertyNames(proto);
    for(let i = 0; i < names.length; i++){
      const key = names[i];
      const value = proto[key];
      if (typeof value === "function") {
        const desc = ReflectGetOwnPropertyDescriptor(proto, key);
        if (desc) {
          desc.enumerable = true;
          ObjectDefineProperty(proto, key, {
            __proto__: null,
            ...desc
          });
        }
      }
    }
  }
  /**
 * Returns a system error name from an error code number.
 * @param code error code number
 */ function getSystemErrorName(code) {
    validateNumber(code, "err");
    if (code >= 0 || !NumberIsSafeInteger(code)) {
      throw new codes.ERR_OUT_OF_RANGE("err", "a negative integer", code);
    }
    return errorMap.get(code)?.[0];
  }
  /**
 * Returns a system error message from an error code number.
 * @param code error code number
 */ function getSystemErrorMessage(code) {
    validateNumber(code, "err");
    if (code >= 0 || !NumberIsSafeInteger(code)) {
      throw new codes.ERR_OUT_OF_RANGE("err", "a negative integer", code);
    }
    return errorMap.get(code)?.[1];
  }
  /**
 * Returns the map of all system error codes available from the Node.js API.
 */ function getSystemErrorMap() {
    return errorMap;
  }
  return {
    notImplemented,
    warnNotImplemented,
    _TextDecoder,
    _TextEncoder,
    intoCallbackAPI,
    intoCallbackAPIWithIntercept,
    spliceOne,
    validateIntegerRange,
    once,
    makeMethodsEnumerable,
    getSystemErrorMap,
    getSystemErrorMessage,
    getSystemErrorName
  };
})());