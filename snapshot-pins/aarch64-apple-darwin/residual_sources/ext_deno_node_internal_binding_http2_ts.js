"use strict"; return ((function() {
  const { core } = __bootstrap;
  const { op_http2_error_string } = core.ops;
  const constants = core.loadExtScript("ext:deno_node/internal/http2/constants.ts");
  class Http2Stream {
    respond(headers, count, options) {
      // Tests replace this prototype method; otherwise `this` is the native
      // handle, so dispatch falls through to the handle's own `respond` op.
      return this.respond(headers, count, options);
    }
    pushPromise(headers, count, options) {
      // Tests replace this prototype method; otherwise `this` is the native
      // handle, so dispatch falls through to the handle's own `pushPromise` op.
      return this.pushPromise(headers, count, options);
    }
  }
  class Http2Session {
    request(headers, count, options, parent, weight, exclusive) {
      // Tests replace this prototype method; otherwise `this` is the native
      // handle, so dispatch falls through to the handle's own `request` op.
      return this.request(headers, count, options, parent, weight, exclusive);
    }
  }
  function nghttp2ErrorString(integerCode) {
    return op_http2_error_string(integerCode);
  }
  const _defaultExport = {
    constants,
    Http2Session,
    Http2Stream,
    nghttp2ErrorString
  };
  return {
    constants,
    Http2Session,
    Http2Stream,
    nghttp2ErrorString,
    default: _defaultExport
  };
})());