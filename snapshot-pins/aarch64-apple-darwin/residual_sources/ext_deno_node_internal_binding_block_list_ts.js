"use strict"; return ((function() {
  // Match POSIX values for AF_INET / AF_INET6 on Linux. The exact values are
  // not observable through the public API, only through this internal binding.
  const AF_INET = 2;
  const AF_INET6 = 10;
  class SocketAddress {
    #address;
    #port;
    #family;
    #flowlabel;
    constructor(address, port, family, flowlabel){
      this.#address = address;
      this.#port = port;
      this.#family = family;
      this.#flowlabel = flowlabel;
    }
    address() {
      return this.#address;
    }
    port() {
      return this.#port;
    }
    family() {
      return this.#family;
    }
    flowlabel() {
      return this.#flowlabel;
    }
  }
  const exports = {
    AF_INET,
    AF_INET6,
    SocketAddress
  };
  return {
    ...exports,
    default: exports
  };
})());