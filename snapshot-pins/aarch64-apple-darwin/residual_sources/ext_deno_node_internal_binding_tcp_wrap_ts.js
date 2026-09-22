"use strict"; return ((function() {
  const { core, primordials } = __bootstrap;
  const { TCPWrap } = core.ops;
  const { AsyncWrap, providerType } = core.loadExtScript("ext:deno_node/internal_binding/async_wrap.ts");
  const { FunctionPrototypeCall } = primordials;
  // Mark TCPWrap as a StreamBase handle, matching Node's StreamBase::AddMethods.
  // This allows parser.consume(socket._handle) to detect it as consumable.
  TCPWrap.prototype.isStreamBase = true;
  /** The type of TCP socket. */ let socketType = /*#__PURE__*/ function(socketType) {
    socketType[socketType["SOCKET"] = 0] = "SOCKET";
    socketType[socketType["SERVER"] = 1] = "SERVER";
    return socketType;
  }({});
  class TCPConnectWrap extends AsyncWrap {
    oncomplete;
    address;
    port;
    localAddress;
    localPort;
    constructor(){
      super(providerType.TCPCONNECTWRAP);
    }
  }
  let constants = /*#__PURE__*/ function(constants) {
    constants[constants["SOCKET"] = 0] = "SOCKET";
    constants[constants["SERVER"] = 1] = "SERVER";
    constants[constants["UV_TCP_IPV6ONLY"] = 2] = "UV_TCP_IPV6ONLY";
    constants[constants["UV_TCP_REUSEPORT"] = 4] = "UV_TCP_REUSEPORT";
    return constants;
  }({});
  /**
 * Wrap the native TCPWrap.listen() to handle connection acceptance.
 * The Rust server_connection_cb fires onconnection(status), and this
 * wrapper creates client handles and calls uv_accept before forwarding
 * to the user's onconnection(status, clientHandle).
 *
 * TODO: Move this logic into Rust by making the connection callback
 * allocate a CppGC TCPWrap directly, removing the need for this JS shim.
 */ function setupListenWrap(serverHandle) {
    const userOnConnection = serverHandle.onconnection;
    serverHandle.onconnection = function(status) {
      if (status !== 0) {
        if (userOnConnection) {
          FunctionPrototypeCall(userOnConnection, serverHandle, status, undefined);
        }
        return;
      }
      // Create a new client handle and accept the connection
      const clientHandle = new TCPWrap(socketType.SOCKET);
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
  // Re-export the Rust TCPWrap as TCP.
  const _defaultExport = {
    TCPConnectWrap,
    constants,
    TCP: TCPWrap,
    setupListenWrap
  };
  return {
    TCP: TCPWrap,
    setupListenWrap,
    TCPConnectWrap,
    socketType,
    constants,
    default: _defaultExport
  };
})());