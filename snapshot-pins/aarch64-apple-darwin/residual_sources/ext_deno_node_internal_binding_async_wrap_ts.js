"use strict"; return ((function() {
  const { core, primordials } = __bootstrap;
  const { AsyncWrap, op_node_new_async_id } = core.ops;
  const { Float64Array, ObjectKeys, Uint32Array } = primordials;
  function registerDestroyHook(// deno-lint-ignore no-explicit-any
  _target, _asyncId, _prop) {
  // TODO(kt3k): implement actual procedures
  }
  let constants = /*#__PURE__*/ function(constants) {
    constants[constants["kInit"] = 0] = "kInit";
    constants[constants["kBefore"] = 1] = "kBefore";
    constants[constants["kAfter"] = 2] = "kAfter";
    constants[constants["kDestroy"] = 3] = "kDestroy";
    constants[constants["kPromiseResolve"] = 4] = "kPromiseResolve";
    constants[constants["kTotals"] = 5] = "kTotals";
    constants[constants["kCheck"] = 6] = "kCheck";
    constants[constants["kExecutionAsyncId"] = 7] = "kExecutionAsyncId";
    constants[constants["kTriggerAsyncId"] = 8] = "kTriggerAsyncId";
    constants[constants["kAsyncIdCounter"] = 9] = "kAsyncIdCounter";
    constants[constants["kDefaultTriggerAsyncId"] = 10] = "kDefaultTriggerAsyncId";
    constants[constants["kUsesExecutionAsyncResource"] = 11] = "kUsesExecutionAsyncResource";
    constants[constants["kStackLength"] = 12] = "kStackLength";
    return constants;
  }({});
  const asyncHookFields = new Uint32Array(ObjectKeys(constants).length);
  // Increment the internal id counter and return the value.
  function newAsyncId() {
    return op_node_new_async_id();
  }
  let UidFields = /*#__PURE__*/ function(UidFields) {
    UidFields[UidFields["kExecutionAsyncId"] = 0] = "kExecutionAsyncId";
    UidFields[UidFields["kTriggerAsyncId"] = 1] = "kTriggerAsyncId";
    UidFields[UidFields["kDefaultTriggerAsyncId"] = 2] = "kDefaultTriggerAsyncId";
    UidFields[UidFields["kUidFieldsCount"] = 3] = "kUidFieldsCount";
    return UidFields;
  }({});
  const asyncIdFields = new Float64Array(ObjectKeys(UidFields).length);
  // `kDefaultTriggerAsyncId` should be `-1`, this indicates that there is no
  // specified default value and it should fallback to the executionAsyncId.
  // 0 is not used as the magic value, because that indicates a missing
  // context which is different from a default context.
  asyncIdFields[UidFields.kDefaultTriggerAsyncId] = -1;
  let providerType = /*#__PURE__*/ function(providerType) {
    providerType[providerType["NONE"] = 0] = "NONE";
    providerType[providerType["DIRHANDLE"] = 1] = "DIRHANDLE";
    providerType[providerType["DNSCHANNEL"] = 2] = "DNSCHANNEL";
    providerType[providerType["ELDHISTOGRAM"] = 3] = "ELDHISTOGRAM";
    providerType[providerType["FILEHANDLE"] = 4] = "FILEHANDLE";
    providerType[providerType["FILEHANDLECLOSEREQ"] = 5] = "FILEHANDLECLOSEREQ";
    providerType[providerType["FIXEDSIZEBLOBCOPY"] = 6] = "FIXEDSIZEBLOBCOPY";
    providerType[providerType["FSEVENTWRAP"] = 7] = "FSEVENTWRAP";
    providerType[providerType["FSREQCALLBACK"] = 8] = "FSREQCALLBACK";
    providerType[providerType["FSREQPROMISE"] = 9] = "FSREQPROMISE";
    providerType[providerType["GETADDRINFOREQWRAP"] = 10] = "GETADDRINFOREQWRAP";
    providerType[providerType["GETNAMEINFOREQWRAP"] = 11] = "GETNAMEINFOREQWRAP";
    providerType[providerType["HEAPSNAPSHOT"] = 12] = "HEAPSNAPSHOT";
    providerType[providerType["HTTP2SESSION"] = 13] = "HTTP2SESSION";
    providerType[providerType["HTTP2STREAM"] = 14] = "HTTP2STREAM";
    providerType[providerType["HTTP2PING"] = 15] = "HTTP2PING";
    providerType[providerType["HTTP2SETTINGS"] = 16] = "HTTP2SETTINGS";
    providerType[providerType["HTTPINCOMINGMESSAGE"] = 17] = "HTTPINCOMINGMESSAGE";
    providerType[providerType["HTTPCLIENTREQUEST"] = 18] = "HTTPCLIENTREQUEST";
    providerType[providerType["JSSTREAM"] = 19] = "JSSTREAM";
    providerType[providerType["JSUDPWRAP"] = 20] = "JSUDPWRAP";
    providerType[providerType["MESSAGEPORT"] = 21] = "MESSAGEPORT";
    providerType[providerType["PIPECONNECTWRAP"] = 22] = "PIPECONNECTWRAP";
    providerType[providerType["PIPESERVERWRAP"] = 23] = "PIPESERVERWRAP";
    providerType[providerType["PIPEWRAP"] = 24] = "PIPEWRAP";
    providerType[providerType["PROCESSWRAP"] = 25] = "PROCESSWRAP";
    providerType[providerType["PROMISE"] = 26] = "PROMISE";
    providerType[providerType["QUERYWRAP"] = 27] = "QUERYWRAP";
    providerType[providerType["SHUTDOWNWRAP"] = 28] = "SHUTDOWNWRAP";
    providerType[providerType["SIGNALWRAP"] = 29] = "SIGNALWRAP";
    providerType[providerType["STATWATCHER"] = 30] = "STATWATCHER";
    providerType[providerType["STREAMPIPE"] = 31] = "STREAMPIPE";
    providerType[providerType["TCPCONNECTWRAP"] = 32] = "TCPCONNECTWRAP";
    providerType[providerType["TCPSERVERWRAP"] = 33] = "TCPSERVERWRAP";
    providerType[providerType["TCPWRAP"] = 34] = "TCPWRAP";
    providerType[providerType["TTYWRAP"] = 35] = "TTYWRAP";
    providerType[providerType["UDPSENDWRAP"] = 36] = "UDPSENDWRAP";
    providerType[providerType["UDPWRAP"] = 37] = "UDPWRAP";
    providerType[providerType["SIGINTWATCHDOG"] = 38] = "SIGINTWATCHDOG";
    providerType[providerType["WORKER"] = 39] = "WORKER";
    providerType[providerType["WORKERHEAPSNAPSHOT"] = 40] = "WORKERHEAPSNAPSHOT";
    providerType[providerType["WRITEWRAP"] = 41] = "WRITEWRAP";
    providerType[providerType["ZLIB"] = 42] = "ZLIB";
    return providerType;
  }({});
  return {
    async_hook_fields: asyncHookFields,
    asyncIdFields,
    AsyncWrap,
    registerDestroyHook,
    newAsyncId,
    constants,
    UidFields,
    providerType
  };
})());