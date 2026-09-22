"use strict"; return ((function() {
  const { core, primordials } = __bootstrap;
  const bindingTypes = core.loadExtScript("ext:deno_node/internal_binding/types.ts");
  const cryptoKeys = core.loadExtScript("ext:deno_node/internal/crypto/_keys.ts");
  const { ArrayBufferIsView, TypedArrayPrototypeGetSymbolToStringTag } = primordials;
  function isArrayBufferView(value) {
    return ArrayBufferIsView(value);
  }
  function isBigInt64Array(value) {
    return TypedArrayPrototypeGetSymbolToStringTag(value) === "BigInt64Array";
  }
  function isBigUint64Array(value) {
    return TypedArrayPrototypeGetSymbolToStringTag(value) === "BigUint64Array";
  }
  function isFloat16Array(value) {
    return TypedArrayPrototypeGetSymbolToStringTag(value) === "Float16Array";
  }
  function isFloat32Array(value) {
    return TypedArrayPrototypeGetSymbolToStringTag(value) === "Float32Array";
  }
  function isFloat64Array(value) {
    return TypedArrayPrototypeGetSymbolToStringTag(value) === "Float64Array";
  }
  function isInt8Array(value) {
    return TypedArrayPrototypeGetSymbolToStringTag(value) === "Int8Array";
  }
  function isInt16Array(value) {
    return TypedArrayPrototypeGetSymbolToStringTag(value) === "Int16Array";
  }
  function isInt32Array(value) {
    return TypedArrayPrototypeGetSymbolToStringTag(value) === "Int32Array";
  }
  function isUint8Array(value) {
    return TypedArrayPrototypeGetSymbolToStringTag(value) === "Uint8Array";
  }
  function isUint8ClampedArray(value) {
    return TypedArrayPrototypeGetSymbolToStringTag(value) === "Uint8ClampedArray";
  }
  function isUint16Array(value) {
    return TypedArrayPrototypeGetSymbolToStringTag(value) === "Uint16Array";
  }
  function isUint32Array(value) {
    return TypedArrayPrototypeGetSymbolToStringTag(value) === "Uint32Array";
  }
  const { // isExternal,
  isAnyArrayBuffer, isArgumentsObject, isArrayBuffer, isAsyncFunction, isBigIntObject, isBooleanObject, isBoxedPrimitive, isDataView, isDate, isGeneratorFunction, isGeneratorObject, isMap, isMapIterator, isModuleNamespaceObject, isNativeError, isNumberObject, isPromise, isProxy, isRegExp, isSet, isSetIterator, isSharedArrayBuffer, isStringObject, isSymbolObject, isTypedArray, isWeakMap, isWeakSet } = bindingTypes;
  return {
    isCryptoKey: cryptoKeys.isCryptoKey,
    isKeyObject: cryptoKeys.isKeyObject,
    isArrayBufferView,
    isBigInt64Array,
    isBigUint64Array,
    isFloat16Array,
    isFloat32Array,
    isFloat64Array,
    isInt8Array,
    isInt16Array,
    isInt32Array,
    isUint8Array,
    isUint8ClampedArray,
    isUint16Array,
    isUint32Array,
    isAnyArrayBuffer,
    isArgumentsObject,
    isArrayBuffer,
    isAsyncFunction,
    isBigIntObject,
    isBooleanObject,
    isBoxedPrimitive,
    isDataView,
    isDate,
    isGeneratorFunction,
    isGeneratorObject,
    isMap,
    isMapIterator,
    isModuleNamespaceObject,
    isNativeError,
    isNumberObject,
    isPromise,
    isProxy,
    isRegExp,
    isSet,
    isSetIterator,
    isSharedArrayBuffer,
    isStringObject,
    isSymbolObject,
    isTypedArray,
    isWeakMap,
    isWeakSet
  };
})());