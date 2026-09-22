"use strict"; return ((function() {
  const { core, primordials } = __bootstrap;
  const { ArrayPrototypeForEach, ArrayPrototypeJoin, ArrayPrototypeMap, ArrayPrototypePush, ArrayPrototypeToString, NumberParseInt, SafeRegExp, StringPrototypeMatch, StringPrototypeReplace } = primordials;
  const { getOptionValue } = core.loadExtScript("ext:deno_node/internal/options.ts");
  const { AI_ADDRCONFIG, AI_ALL, AI_V4MAPPED } = core.loadExtScript("ext:deno_node/internal_binding/ares.ts");
  const { ChannelWrap, DNS_ORDER_IPV4_FIRST, DNS_ORDER_IPV6_FIRST, DNS_ORDER_VERBATIM, strerror } = core.loadExtScript("ext:deno_node/internal_binding/cares_wrap.ts");
  const { ERR_DNS_SET_SERVERS_FAILED, ERR_INVALID_ARG_VALUE, ERR_INVALID_IP_ADDRESS } = core.loadExtScript("ext:deno_node/internal/errors.ts");
  const { validateArray, validateInt32, validateOneOf, validateString } = core.loadExtScript("ext:deno_node/internal/validators.mjs");
  const { isIP } = core.loadExtScript("ext:deno_node/internal/net.ts");
  function isLookupOptions(options) {
    return typeof options === "object" || typeof options === "undefined";
  }
  function isLookupCallback(options) {
    return typeof options === "function";
  }
  function isFamily(options) {
    return typeof options === "number";
  }
  function isResolveCallback(callback) {
    return typeof callback === "function";
  }
  const IANA_DNS_PORT = 53;
  const IPv6RE = new SafeRegExp("^\\[([^[\\]]*)\\]");
  const addrSplitRE = new SafeRegExp("(^.+?)(?::(\\d+))?$");
  function validateTimeout(options) {
    const { timeout = -1 } = {
      ...options
    };
    validateInt32(timeout, "options.timeout", -1, 2 ** 31 - 1);
    return timeout;
  }
  function validateTries(options) {
    const { tries = 4 } = {
      ...options
    };
    validateInt32(tries, "options.tries", 1, 2 ** 31 - 1);
    return tries;
  }
  function validateMaxTimeout(options) {
    if (options?.maxTimeout === undefined) return -1; // no cap
    validateInt32(options.maxTimeout, "options.maxTimeout", 0, 2 ** 31 - 1);
    return options.maxTimeout;
  }
  /**
 * An independent resolver for DNS requests.
 *
 * Creating a new resolver uses the default server settings. Setting
 * the servers used for a resolver using `resolver.setServers()` does not affect
 * other resolvers:
 *
 * ```js
 * const { Resolver } = require('dns');
 * const resolver = new Resolver();
 * resolver.setServers(['4.4.4.4']);
 *
 * // This request will use the server at 4.4.4.4, independent of global settings.
 * resolver.resolve4('example.org', (err, addresses) => {
 *   // ...
 * });
 * ```
 *
 * The following methods from the `dns` module are available:
 *
 * - `resolver.getServers()`
 * - `resolver.resolve()`
 * - `resolver.resolve4()`
 * - `resolver.resolve6()`
 * - `resolver.resolveAny()`
 * - `resolver.resolveCaa()`
 * - `resolver.resolveCname()`
 * - `resolver.resolveMx()`
 * - `resolver.resolveNaptr()`
 * - `resolver.resolveNs()`
 * - `resolver.resolvePtr()`
 * - `resolver.resolveSoa()`
 * - `resolver.resolveSrv()`
 * - `resolver.resolveTxt()`
 * - `resolver.reverse()`
 * - `resolver.setServers()`
 */ class Resolver {
    _handle;
    constructor(options){
      const timeout = validateTimeout(options);
      const tries = validateTries(options);
      const maxTimeout = validateMaxTimeout(options);
      this._handle = new ChannelWrap(timeout, tries, maxTimeout);
    }
    cancel() {
      this._handle.cancel();
    }
    getServers() {
      const servers = this._handle.getServers() || [];
      return ArrayPrototypeMap(servers, (val)=>{
        if (!val[1] || val[1] === IANA_DNS_PORT) {
          return val[0];
        }
        const host = isIP(val[0]) === 6 ? `[${val[0]}]` : val[0];
        return `${host}:${val[1]}`;
      });
    }
    setServers(servers) {
      validateArray(servers, "servers");
      // Cache the original servers because in the event of an error while
      // setting the servers, c-ares won't have any servers available for
      // resolution.
      const orig = this._handle.getServers();
      const newSet = [];
      ArrayPrototypeForEach(servers, (serv, index)=>{
        validateString(serv, `servers[${index}]`);
        let ipVersion = isIP(serv);
        if (ipVersion !== 0) {
          return ArrayPrototypePush(newSet, [
            ipVersion,
            serv,
            IANA_DNS_PORT
          ]);
        }
        const match = StringPrototypeMatch(serv, IPv6RE);
        // Check for an IPv6 in brackets.
        if (match) {
          ipVersion = isIP(match[1]);
          if (ipVersion !== 0) {
            const port = NumberParseInt(StringPrototypeReplace(serv, addrSplitRE, "$2")) || IANA_DNS_PORT;
            return ArrayPrototypePush(newSet, [
              ipVersion,
              match[1],
              port
            ]);
          }
        }
        // addr::port
        const addrSplitMatch = StringPrototypeMatch(serv, addrSplitRE);
        if (addrSplitMatch) {
          const hostIP = addrSplitMatch[1];
          const port = addrSplitMatch[2] || `${IANA_DNS_PORT}`;
          ipVersion = isIP(hostIP);
          if (ipVersion !== 0) {
            return ArrayPrototypePush(newSet, [
              ipVersion,
              hostIP,
              NumberParseInt(port)
            ]);
          }
        }
        throw new ERR_INVALID_IP_ADDRESS(serv);
      });
      const errorNumber = this._handle.setServers(newSet);
      if (errorNumber !== 0) {
        // Reset the servers to the old servers, because ares probably unset them.
        this._handle.setServers(ArrayPrototypeJoin(orig, ","));
        const err = strerror(errorNumber);
        throw new ERR_DNS_SET_SERVERS_FAILED(err, ArrayPrototypeToString(servers));
      }
    }
    /**
   * The resolver instance will send its requests from the specified IP address.
   * This allows programs to specify outbound interfaces when used on multi-homed
   * systems.
   *
   * If a v4 or v6 address is not specified, it is set to the default, and the
   * operating system will choose a local address automatically.
   *
   * The resolver will use the v4 local address when making requests to IPv4 DNS
   * servers, and the v6 local address when making requests to IPv6 DNS servers.
   * The `rrtype` of resolution requests has no impact on the local address used.
   *
   * @param [ipv4='0.0.0.0'] A string representation of an IPv4 address.
   * @param [ipv6='::0'] A string representation of an IPv6 address.
   */ setLocalAddress(ipv4, ipv6) {
      validateString(ipv4, "ipv4");
      if (ipv6 !== undefined) {
        validateString(ipv6, "ipv6");
      }
      this._handle.setLocalAddress(ipv4, ipv6);
    }
  }
  let defaultResolver = new Resolver();
  function getDefaultResolver() {
    return defaultResolver;
  }
  function setDefaultResolver(resolver) {
    defaultResolver = resolver;
  }
  function validateHints(hints) {
    if ((hints & ~(AI_ADDRCONFIG | AI_ALL | AI_V4MAPPED)) !== 0) {
      throw new ERR_INVALID_ARG_VALUE("hints", hints, "is invalid");
    }
  }
  let dnsOrder;
  function ensureDnsOrder() {
    if (dnsOrder === undefined) {
      dnsOrder = getOptionValue("--dns-result-order") || "ipv4first";
    }
    return dnsOrder;
  }
  function getDefaultDnsOrder() {
    return ensureDnsOrder();
  }
  const validDnsOrders = [
    "verbatim",
    "ipv4first",
    "ipv6first"
  ];
  function dnsOrderToNumber(order) {
    switch(order){
      case "verbatim":
        return DNS_ORDER_VERBATIM;
      case "ipv4first":
        return DNS_ORDER_IPV4_FIRST;
      case "ipv6first":
        return DNS_ORDER_IPV6_FIRST;
      default:
        throw new ERR_INVALID_ARG_VALUE("order", order);
    }
  }
  function setDefaultResultOrder(order) {
    validateOneOf(order, "dnsOrder", [
      "verbatim",
      "ipv4first",
      "ipv6first"
    ]);
    dnsOrder = order;
  }
  function defaultResolverSetServers(servers) {
    const resolver = new Resolver();
    resolver.setServers(servers);
    setDefaultResolver(resolver);
  }
  return {
    isLookupOptions,
    isLookupCallback,
    isFamily,
    isResolveCallback,
    validateTimeout,
    validateTries,
    validateMaxTimeout,
    Resolver,
    getDefaultResolver,
    setDefaultResolver,
    validateHints,
    getDefaultDnsOrder,
    validDnsOrders,
    dnsOrderToNumber,
    setDefaultResultOrder,
    defaultResolverSetServers
  };
})());