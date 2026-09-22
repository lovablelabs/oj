"use strict"; return ((function () {
const { core, primordials } = __bootstrap;
const {
  ArrayPrototypePop,
  Error,
  FunctionPrototypeBind,
  ReflectApply,
  ObjectDefineProperties,
  ObjectGetOwnPropertyDescriptors,
  ObjectSetPrototypeOf,
  ObjectValues,
  PromisePrototypeThen,
} = primordials;

const { nextTick } = core.loadExtScript("ext:deno_node/_next_tick.ts");
const { validateFunction } = core.loadExtScript(
  "ext:deno_node/internal/validators.mjs",
);

class NodeFalsyValueRejectionError extends Error {
  code = "ERR_FALSY_VALUE_REJECTION";
  constructor(reason) {
    super("Promise was rejected with falsy value");
    this.reason = reason;
  }
}

function callbackify(original) {
  validateFunction(original, "original");

  // We DO NOT return the promise as it gives the user a false sense that
  // the promise is actually somehow related to the callback's execution
  // and that the callback throwing will reject the promise.
  function callbackified(...args) {
    const maybeCb = ArrayPrototypePop(args);
    validateFunction(maybeCb, "last argument");
    const cb = FunctionPrototypeBind(maybeCb, this);
    // In true node style we process the callback on `nextTick` with all the
    // implications (stack, `uncaughtException`, `async_hooks`)
    PromisePrototypeThen(
      ReflectApply(original, this, args),
      (ret) => nextTick(cb, null, ret),
      (rej) => {
        rej = rej || new NodeFalsyValueRejectionError(rej);
        return nextTick(cb, rej);
      },
    );
  }

  const descriptors = ObjectGetOwnPropertyDescriptors(original);
  // It is possible to manipulate a functions `length` or `name` property. This
  // guards against the manipulation.
  if (typeof descriptors.length.value === "number") {
    descriptors.length.value++;
  }
  if (typeof descriptors.name.value === "string") {
    descriptors.name.value += "Callbackified";
  }
  const propertiesValues = ObjectValues(descriptors);
  for (let i = 0; i < propertiesValues.length; i++) {
    // We want to use null-prototype objects to not rely on globally mutable
    // %Object.prototype%.
    ObjectSetPrototypeOf(propertiesValues[i], null);
  }
  ObjectDefineProperties(callbackified, descriptors);
  return callbackified;
}

return {
  callbackify,
};
})());