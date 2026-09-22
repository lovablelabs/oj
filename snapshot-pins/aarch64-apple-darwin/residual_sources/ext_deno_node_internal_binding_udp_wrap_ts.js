"use strict"; return ((function() {
  const { core, primordials } = __bootstrap;
  const { op_node_udp_bind, op_node_udp_fd_for_ipc, op_node_udp_join_multi_v4, op_node_udp_join_multi_v6, op_node_udp_join_source_specific, op_node_udp_leave_multi_v4, op_node_udp_leave_multi_v6, op_node_udp_leave_source_specific, op_node_udp_open, op_node_udp_recv, op_node_udp_send, op_node_udp_set_broadcast, op_node_udp_set_multicast_interface, op_node_udp_set_multicast_loopback, op_node_udp_set_multicast_ttl, op_node_udp_set_ttl } = core.ops;
  const { ArrayPrototypeMap, ErrorPrototype, ObjectPrototypeIsPrototypeOf, SafeRegExp, StringPrototypeIncludes, StringPrototypeMatch, Uint8Array } = primordials;
  const osErrorRegExp = new SafeRegExp(/os error (40|90|10040)/);
  const { AsyncWrap, providerType } = core.loadExtScript("ext:deno_node/internal_binding/async_wrap.ts");
  const { HandleWrap } = core.loadExtScript("ext:deno_node/internal_binding/handle_wrap.ts");
  const { ownerSymbol } = core.loadExtScript("ext:deno_node/internal_binding/symbols.ts");
  const { codeMap, errorMap } = core.loadExtScript("ext:deno_node/internal_binding/uv.ts");
  const { Buffer } = core.loadExtScript("ext:deno_node/internal/buffer.mjs");
  const { isIP } = core.loadExtScript("ext:deno_node/internal/net.ts");
  const { isLinux, isWindows } = core.loadExtScript("ext:deno_node/_util/os.ts");
  const { os } = core.loadExtScript("ext:deno_node/internal_binding/constants.ts");
  const AF_INET = 2;
  const AF_INET6 = 10;
  const UDP_DGRAM_MAXSIZE = 64 * 1024;
  /** Validate that the address is a parseable IPv4 address. */ function isValidIPv4Address(address) {
    return isIP(address) === 4;
  }
  /** Validate multicast address matches the socket family. */ function isValidMulticastAddress(multicastAddress, family, interfaceAddress) {
    if (family === "IPv6") {
      // IPv6 multicast - interface can be address, name, or address%zone
      // Validation of interface is done in Rust
      return isIP(multicastAddress) === 6;
    } else {
      // IPv4 multicast
      if (!isValidIPv4Address(multicastAddress)) return false;
      if (interfaceAddress !== undefined && !isValidIPv4Address(interfaceAddress)) {
        return false;
      }
      return true;
    }
  }
  class SendWrap extends AsyncWrap {
    list;
    address;
    port;
    callback;
    oncomplete;
    constructor(){
      super(providerType.UDPSENDWRAP);
    }
  }
  class UDP extends HandleWrap {
    [ownerSymbol] = null;
    #address;
    #family;
    #port;
    #remoteAddress;
    #remoteFamily;
    #remotePort;
    #rid;
    #receiving = false;
    #recvPromiseId;
    #unrefed = false;
    #recvBufferSize = UDP_DGRAM_MAXSIZE;
    #sendBufferSize = UDP_DGRAM_MAXSIZE;
    onmessage;
    lookup;
    constructor(){
      super(providerType.UDPWRAP);
    }
    addMembership(multicastAddress, interfaceAddress) {
      if (!isValidMulticastAddress(multicastAddress, this.#family, interfaceAddress)) {
        return codeMap.get("EINVAL");
      }
      if (this.#rid === undefined) {
        return codeMap.get("EBADF");
      }
      try {
        if (this.#family === "IPv6") {
          op_node_udp_join_multi_v6(this.#rid, multicastAddress, interfaceAddress ?? null);
        } else {
          op_node_udp_join_multi_v4(this.#rid, multicastAddress, interfaceAddress ?? null);
        }
        return 0;
      } catch  {
        return codeMap.get("EINVAL");
      }
    }
    addSourceSpecificMembership(sourceAddress, groupAddress, interfaceAddress) {
      if (!isValidIPv4Address(sourceAddress) || !isValidIPv4Address(groupAddress)) {
        return codeMap.get("EINVAL");
      }
      if (this.#rid === undefined) {
        return codeMap.get("EBADF");
      }
      try {
        op_node_udp_join_source_specific(this.#rid, sourceAddress, groupAddress, interfaceAddress ?? "0.0.0.0");
      } catch  {
        return codeMap.get("EINVAL");
      }
      return 0;
    }
    /**
   * Bind to an IPv4 address.
   * @param ip The hostname to bind to.
   * @param port The port to bind to
   * @return An error status code.
   */ bind(ip, port, flags) {
      return this.#doBind(ip, port, flags, AF_INET);
    }
    /**
   * Bind to an IPv6 address.
   * @param ip The hostname to bind to.
   * @param port The port to bind to
   * @return An error status code.
   */ bind6(ip, port, flags) {
      return this.#doBind(ip, port, flags, AF_INET6);
    }
    bufferSize(size, buffer, ctx) {
      if (!this.#address) {
        const err = isWindows ? "ENOTSOCK" : "EBADF";
        ctx.errno = codeMap.get(err);
        ctx.code = err;
        ctx.message = errorMap.get(ctx.errno)[1];
        ctx.syscall = buffer ? "uv_recv_buffer_size" : "uv_send_buffer_size";
        return;
      }
      if (size !== 0) {
        size = isLinux ? size * 2 : size;
        if (buffer) {
          return this.#recvBufferSize = size;
        }
        return this.#sendBufferSize = size;
      }
      return buffer ? this.#recvBufferSize : this.#sendBufferSize;
    }
    connect(ip, port) {
      return this.#doConnect(ip, port, AF_INET);
    }
    connect6(ip, port) {
      return this.#doConnect(ip, port, AF_INET6);
    }
    disconnect() {
      this.#remoteAddress = undefined;
      this.#remotePort = undefined;
      this.#remoteFamily = undefined;
      return 0;
    }
    dropMembership(multicastAddress, interfaceAddress) {
      if (!isValidMulticastAddress(multicastAddress, this.#family, interfaceAddress)) {
        return codeMap.get("EINVAL");
      }
      if (this.#rid === undefined) {
        return codeMap.get("EBADF");
      }
      try {
        if (this.#family === "IPv6") {
          op_node_udp_leave_multi_v6(this.#rid, multicastAddress, interfaceAddress ?? null);
        } else {
          op_node_udp_leave_multi_v4(this.#rid, multicastAddress, interfaceAddress ?? null);
        }
        return 0;
      } catch  {
        return codeMap.get("EINVAL");
      }
    }
    dropSourceSpecificMembership(sourceAddress, groupAddress, interfaceAddress) {
      if (!isValidIPv4Address(sourceAddress) || !isValidIPv4Address(groupAddress)) {
        return codeMap.get("EINVAL");
      }
      if (this.#rid === undefined) {
        return codeMap.get("EBADF");
      }
      try {
        op_node_udp_leave_source_specific(this.#rid, sourceAddress, groupAddress, interfaceAddress ?? "0.0.0.0");
      } catch  {
        return codeMap.get("EINVAL");
      }
      return 0;
    }
    /**
   * Populates the provided object with remote address entries.
   * @param peername An object to add the remote address entries to.
   * @return An error status code.
   */ getpeername(peername) {
      if (this.#remoteAddress === undefined) {
        return codeMap.get("EBADF");
      }
      peername.address = this.#remoteAddress;
      peername.port = this.#remotePort;
      peername.family = this.#remoteFamily;
      return 0;
    }
    /**
   * Populates the provided object with local address entries.
   * @param sockname An object to add the local address entries to.
   * @return An error status code.
   */ getsockname(sockname) {
      if (this.#address === undefined) {
        return codeMap.get("EBADF");
      }
      sockname.address = this.#address;
      sockname.port = this.#port;
      sockname.family = this.#family;
      return 0;
    }
    /**
   * Opens an existing file descriptor as this UDP socket.
   * @param fd The file descriptor to open.
   * @return An error status code.
   */ open(fd) {
      try {
        const result = op_node_udp_open(fd);
        const rid = result[0];
        const hostname = result[1];
        const boundPort = result[2];
        this.#rid = rid;
        this.#address = hostname;
        this.#port = boundPort;
        // Determine family from the address string returned by the op.
        this.#family = StringPrototypeIncludes(hostname, ":") ? "IPv6" : "IPv4";
        return 0;
      } catch (e) {
        return codeMap.get(e.code ?? "UNKNOWN") ?? codeMap.get("UNKNOWN");
      }
    }
    /**
   * Return the raw fd so it can be sent over IPC via SCM_RIGHTS.
   * Returns -1 on platforms that don't support fd-passing.
   */ fdForIpc() {
      if (this.#rid === undefined) {
        return -1;
      }
      return op_node_udp_fd_for_ipc(this.#rid);
    }
    /**
   * Start receiving on the connection.
   * @return An error status code.
   */ recvStart() {
      if (!this.#receiving) {
        this.#receiving = true;
        this.#receive();
      }
      return 0;
    }
    /**
   * Stop receiving on the connection.
   * @return An error status code.
   */ recvStop() {
      this.#receiving = false;
      return 0;
    }
    ref() {
      this.#unrefed = false;
    }
    send(req, bufs, count, ...args) {
      return this.#doSend(req, bufs, count, args, AF_INET);
    }
    send6(req, bufs, count, ...args) {
      return this.#doSend(req, bufs, count, args, AF_INET6);
    }
    setBroadcast(bool) {
      if (this.#rid === undefined) {
        return codeMap.get("EBADF");
      }
      try {
        op_node_udp_set_broadcast(this.#rid, bool === 1);
        return 0;
      } catch  {
        return codeMap.get("EINVAL");
      }
    }
    setMulticastInterface(interfaceAddress) {
      if (this.#rid === undefined) {
        return codeMap.get("EBADF");
      }
      try {
        op_node_udp_set_multicast_interface(this.#rid, this.#family === "IPv6", interfaceAddress);
        return 0;
      } catch  {
        return codeMap.get("EINVAL");
      }
    }
    setMulticastLoopback(bool) {
      if (this.#rid === undefined) {
        return codeMap.get("EBADF");
      }
      try {
        op_node_udp_set_multicast_loopback(this.#rid, this.#family === "IPv4", bool === 1);
        return 0;
      } catch  {
        return codeMap.get("EINVAL");
      }
    }
    setMulticastTTL(ttl) {
      if (ttl < 1 || ttl > 255) {
        return codeMap.get("EINVAL");
      }
      if (this.#rid === undefined) {
        return codeMap.get("EBADF");
      }
      try {
        if (this.#family === "IPv4") {
          op_node_udp_set_multicast_ttl(this.#rid, ttl);
        }
        return 0;
      } catch  {
        return codeMap.get("EINVAL");
      }
    }
    setTTL(ttl) {
      if (ttl < 1 || ttl > 255) {
        return codeMap.get("EINVAL");
      }
      if (this.#rid === undefined) {
        return codeMap.get("EBADF");
      }
      try {
        op_node_udp_set_ttl(this.#rid, ttl);
        return 0;
      } catch  {
        return codeMap.get("EINVAL");
      }
    }
    unref() {
      this.#unrefed = true;
    }
    #doBind(ip, port, flags, family) {
      try {
        const result = op_node_udp_bind(ip, port, (flags & os.UV_UDP_REUSEADDR) !== 0, (flags & os.UV_UDP_IPV6ONLY) !== 0);
        const rid = result[0];
        const hostname = result[1];
        const boundPort = result[2];
        this.#rid = rid;
        this.#address = hostname;
        this.#port = boundPort;
        this.#family = family === AF_INET6 ? "IPv6" : "IPv4";
        return 0;
      } catch (e) {
        if (ObjectPrototypeIsPrototypeOf(Deno.errors.NotCapable.prototype, e)) {
          throw e;
        }
        return codeMap.get(e.code ?? "UNKNOWN") ?? codeMap.get("UNKNOWN");
      }
    }
    #doConnect(ip, port, family) {
      this.#remoteAddress = ip;
      this.#remotePort = port;
      this.#remoteFamily = family === AF_INET6 ? "IPv6" : "IPv4";
      return 0;
    }
    #doSend(req, bufs, _count, args, _family) {
      let hasCallback;
      if (args.length === 3) {
        this.#remotePort = args[0];
        this.#remoteAddress = args[1];
        hasCallback = args[2];
      } else {
        hasCallback = args[0];
      }
      const payload = new Uint8Array(// deno-lint-ignore deno-internal/prefer-primordials
      Buffer.concat(ArrayPrototypeMap(bufs, (buf)=>{
        if (typeof buf === "string") {
          return Buffer.from(buf);
        }
        // deno-lint-ignore deno-internal/prefer-primordials
        return Buffer.from(buf.buffer, buf.byteOffset, buf.byteLength);
      })));
      (async ()=>{
        let sent;
        let err = null;
        try {
          sent = await op_node_udp_send(this.#rid, payload, this.#remoteAddress, this.#remotePort);
        } catch (e) {
          if (ObjectPrototypeIsPrototypeOf(Deno.errors.BadResource.prototype, e)) {
            err = codeMap.get("EBADF");
          } else if (ObjectPrototypeIsPrototypeOf(ErrorPrototype, e) && StringPrototypeMatch(e.message, osErrorRegExp)) {
            err = codeMap.get("EMSGSIZE");
          } else {
            err = codeMap.get("UNKNOWN");
          }
          sent = 0;
        }
        if (hasCallback) {
          try {
            req.oncomplete(err, sent);
          } catch  {
          // swallow callback errors
          }
        }
      })();
      return 0;
    }
    async #receive() {
      if (!this.#receiving) {
        return;
      }
      const p = new Uint8Array(this.#recvBufferSize);
      let nread;
      let remoteHostname = null;
      let remotePort = null;
      try {
        const promise = op_node_udp_recv(this.#rid, p);
        if (this.#unrefed) {
          core.unrefOpPromise(promise);
        }
        const result = await promise;
        nread = result.nread;
        remoteHostname = result.hostname;
        remotePort = result.port;
      } catch (e) {
        if (ObjectPrototypeIsPrototypeOf(Deno.errors.Interrupted.prototype, e) || ObjectPrototypeIsPrototypeOf(Deno.errors.BadResource.prototype, e)) {
          nread = 0;
        } else {
          nread = codeMap.get("UNKNOWN");
        }
      }
      const rinfo = remoteHostname !== null ? {
        address: remoteHostname,
        port: remotePort,
        family: isIP(remoteHostname) === 6 ? "IPv6" : "IPv4"
      } : undefined;
      const buf = remoteHostname !== null ? Buffer.from(p.buffer, p.byteOffset, nread) : Buffer.alloc(0);
      try {
        this.onmessage(nread, this, buf, rinfo);
      } catch  {
      // swallow callback errors.
      }
      this.#receive();
    }
    /** Handle socket closure. */ _onClose() {
      this.#receiving = false;
      this.#address = undefined;
      this.#port = undefined;
      this.#family = undefined;
      if (this.#rid !== undefined) {
        try {
          core.close(this.#rid);
        } catch  {
        // already closed
        }
        this.#rid = undefined;
      }
      return 0;
    }
  }
  return {
    default: {
      SendWrap,
      UDP
    },
    SendWrap,
    UDP
  };
})());