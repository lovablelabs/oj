"use strict"; return ((function() {
  const { core, primordials } = __bootstrap;
  const { Int32Array } = primordials;
  // deno-lint-ignore no-unused-vars
  const { HandleWrap } = core.loadExtScript("ext:deno_node/internal_binding/handle_wrap.ts");
  const { AsyncWrap, providerType } = core.loadExtScript("ext:deno_node/internal_binding/async_wrap.ts");
  const kReadBytesOrError = 0;
  const kArrayBufferOffset = 1;
  const kBytesWritten = 2;
  const kLastWriteWasAsync = 3;
  const kNumStreamBaseStateFields = 4;
  const streamBaseState = new Int32Array(5);
  class WriteWrap extends AsyncWrap {
    handle;
    oncomplete;
    async;
    bytes;
    buffer;
    callback;
    _chunks;
    constructor(){
      super(providerType.WRITEWRAP);
    }
  }
  class ShutdownWrap extends AsyncWrap {
    handle;
    oncomplete;
    callback;
    constructor(){
      super(providerType.SHUTDOWNWRAP);
    }
  }
  return {
    WriteWrap,
    ShutdownWrap,
    kReadBytesOrError,
    kArrayBufferOffset,
    kBytesWritten,
    kLastWriteWasAsync,
    kNumStreamBaseStateFields,
    streamBaseState
  };
})());