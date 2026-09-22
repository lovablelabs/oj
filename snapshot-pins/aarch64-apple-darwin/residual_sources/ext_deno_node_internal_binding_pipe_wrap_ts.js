"use strict"; return ((function() {
  const { core, primordials } = __bootstrap;
  const { op_node_create_pipe, PipeWrap } = core.ops;
  const { AsyncWrap, providerType } = core.loadExtScript("ext:deno_node/internal_binding/async_wrap.ts");
  const { ceilPowOf2 } = core.loadExtScript("ext:deno_node/internal_binding/_listen.ts");
  const { codeMap } = core.loadExtScript("ext:deno_node/internal_binding/uv.ts");
  const { fs } = core.loadExtScript("ext:deno_node/internal_binding/constants.ts");
  const { FunctionPrototypeCall, MapPrototypeGet } = primordials;
  // Mark PipeWrap as a StreamBase handle, matching Node's StreamBase::AddMethods.
  PipeWrap.prototype.isStreamBase = true;
  /** The type of pipe socket. */ let socketType = /*#__PURE__*/ function(socketType) {
    socketType[socketType["SOCKET"] = 0] = "SOCKET";
    socketType[socketType["SERVER"] = 1] = "SERVER";
    socketType[socketType["IPC"] = 2] = "IPC";
    return socketType;
  }({});
  let constants = /*#__PURE__*/ function(constants) {
    constants[constants["SOCKET"] = 0] = "SOCKET";
    constants[constants["SERVER"] = 1] = "SERVER";
    constants[constants["IPC"] = 2] = "IPC";
    constants[constants["UV_READABLE"] = 1] = "UV_READABLE";
    constants[constants["UV_WRITABLE"] = 2] = "UV_WRITABLE";
    return constants;
  }({});
  class PipeConnectWrap extends AsyncWrap {
    oncomplete;
    address;
    constructor(){
      super(providerType.PIPECONNECTWRAP);
    }
  }
  // Translate UV_READABLE/UV_WRITABLE flags to POSIX mode bits before calling
  // the native fchmod op (which takes raw chmod bits).
  const nativeFchmod = PipeWrap.prototype.fchmod;
  PipeWrap.prototype.fchmod = function(mode) {
    if (mode !== constants.UV_READABLE && mode !== constants.UV_WRITABLE && mode !== (constants.UV_WRITABLE | constants.UV_READABLE)) {
      return MapPrototypeGet(codeMap, "EINVAL");
    }
    let desiredMode = 0;
    if (mode & constants.UV_READABLE) {
      desiredMode |= fs.S_IRUSR | fs.S_IRGRP | fs.S_IROTH;
    }
    if (mode & constants.UV_WRITABLE) {
      desiredMode |= fs.S_IWUSR | fs.S_IWGRP | fs.S_IWOTH;
    }
    return FunctionPrototypeCall(nativeFchmod, this, desiredMode);
  };
  // Round up the backlog to the next power of two (matching the previous
  // implementation). TCP uses the raw backlog; pipes historically rounded.
  const nativeListen = PipeWrap.prototype.listen;
  PipeWrap.prototype.listen = function(backlog) {
    return FunctionPrototypeCall(nativeListen, this, ceilPowOf2(backlog + 1));
  };
  /**
 * Wrap the native PipeWrap.listen() to handle connection acceptance.
 * The Rust server_connection_cb fires onconnection(status), and this
 * wrapper creates client handles and calls uv_accept before forwarding
 * to the user's onconnection(status, clientHandle).
 */ function setupListenWrap(serverHandle) {
    const userOnConnection = serverHandle.onconnection;
    serverHandle.onconnection = function(status) {
      if (status !== 0) {
        if (userOnConnection) {
          FunctionPrototypeCall(userOnConnection, serverHandle, status, undefined);
        }
        return;
      }
      const clientHandle = new PipeWrap(socketType.SOCKET);
      const acceptErr = serverHandle.accept(clientHandle);
      if (acceptErr !== 0) {
        if (userOnConnection) {
          FunctionPrototypeCall(userOnConnection, serverHandle, acceptErr, undefined);
        }
        return;
      }
      if (userOnConnection) {
        FunctionPrototypeCall(userOnConnection, serverHandle, 0, clientHandle);
      }
    };
  }
  // Re-export the Rust PipeWrap as Pipe.
  /** Create an anonymous pipe pair. Returns [readFd, writeFd]. */ function createPipe() {
    return op_node_create_pipe();
  }
  const _defaultExport = {
    Pipe: PipeWrap,
    PipeConnectWrap,
    constants,
    setupListenWrap,
    createPipe
  };
  return {
    Pipe: PipeWrap,
    setupListenWrap,
    createPipe,
    PipeConnectWrap,
    socketType,
    constants,
    default: _defaultExport
  };
})());