"use strict"; return ((function() {
  const { core, primordials } = __bootstrap;
  const { getExecArgvOptions, getOptions } = core.loadExtScript("ext:deno_node/internal_binding/node_options.ts");
  const { MapPrototypeGet, SafeMap, StringPrototypeSlice, StringPrototypeStartsWith } = primordials;
  const dummyOptions = new SafeMap();
  function isWarmupPhase() {
    return !Deno.build;
  }
  function getOptionsFromBinding() {
    // If Deno.build is not defined, this is in warmup phase.
    if (isWarmupPhase()) {
      return dummyOptions;
    }
    return getOptions().options;
  }
  function getOptionValue(optionName) {
    return getOptionValueFromMap(getOptionsFromBinding(), optionName);
  }
  function getExecArgvOptionValue(optionName) {
    if (isWarmupPhase()) {
      return undefined;
    }
    return getOptionValueFromMap(getExecArgvOptions().options, optionName);
  }
  function getOptionValueFromMap(options, optionName) {
    if (StringPrototypeStartsWith(optionName, "--no-")) {
      const option = MapPrototypeGet(options, "--" + StringPrototypeSlice(optionName, 5));
      return option && !option.value;
    }
    return MapPrototypeGet(options, optionName)?.value;
  }
  return {
    getExecArgvOptionValue,
    getOptionValue
  };
})());