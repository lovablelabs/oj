"use strict"; return ((function() {
  const { core, primordials } = __bootstrap;
  const { ArrayPrototypeSlice, ArrayPrototypeSort } = primordials;
  const { OutgoingMessage, validateHeaderName, validateHeaderValue } = core.createLazyLoader("node:_http_outgoing")();
  const { ClientRequest } = core.createLazyLoader("node:_http_client")();
  const httpAgent = core.createLazyLoader("node:_http_agent")();
  const { Agent } = httpAgent;
  const httpProxy = core.createLazyLoader("node:_http_proxy")();
  const { setGlobalProxyFromEnv } = httpProxy;
  const { IncomingMessage } = core.createLazyLoader("node:_http_incoming")();
  const { _connectionListener, Server: ServerImpl, ServerResponse, STATUS_CODES } = core.createLazyLoader("node:_http_server")();
  const { methods, parsers } = core.createLazyLoader("node:_http_common")();
  const { validateInteger } = core.loadExtScript("ext:deno_node/internal/validators.mjs");
  const METHODS = ArrayPrototypeSort(ArrayPrototypeSlice(methods));
  function createServer(opts, requestListener) {
    return new ServerImpl(opts, requestListener);
  }
  function request(...args) {
    return new ClientRequest(args[0], args[1], args[2]);
  }
  function get(...args) {
    const req = request(args[0], args[1], args[2]);
    req.end();
    return req;
  }
  // Default max header size matches Node.js default (16 KiB).
  // Node reads this from --max-http-header-size; we hardcode it.
  const maxHeaderSize = 16_384;
  function setMaxIdleHTTPParsers(max) {
    validateInteger(max, "max", 1);
    parsers.max = max;
  }
  return {
    _connectionListener,
    Agent,
    ClientRequest,
    createServer,
    get,
    get globalAgent () {
      return httpAgent.globalAgent;
    },
    set globalAgent (value){
      httpAgent.setGlobalAgent(value);
    },
    IncomingMessage,
    maxHeaderSize,
    METHODS,
    OutgoingMessage,
    request,
    Server: ServerImpl,
    ServerImpl,
    ServerResponse,
    setGlobalProxyFromEnv,
    setMaxIdleHTTPParsers,
    STATUS_CODES,
    validateHeaderName,
    validateHeaderValue
  };
})());