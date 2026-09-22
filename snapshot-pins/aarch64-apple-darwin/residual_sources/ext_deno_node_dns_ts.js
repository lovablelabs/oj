"use strict"; return ((function() {
  const { core, primordials } = __bootstrap;
  const { nextTick } = core.loadExtScript("ext:deno_node/_next_tick.ts");
  const { customPromisifyArgs } = core.loadExtScript("ext:deno_node/internal/util.mjs");
  const { validateBoolean, validateFunction, validateNumber, validateOneOf, validatePort, validateString } = core.loadExtScript("ext:deno_node/internal/validators.mjs");
  const { isIP } = core.loadExtScript("ext:deno_node/internal/net.ts");
  const dnsUtilsNs = core.loadExtScript("ext:deno_node/internal/dns/utils.ts");
  const { dnsOrderToNumber, getDefaultDnsOrder, getDefaultResolver, isFamily, isLookupCallback, isLookupOptions, isResolveCallback, setDefaultResolver, setDefaultResultOrder, validateHints, validDnsOrders } = dnsUtilsNs;
  const CallbackResolver = dnsUtilsNs.Resolver;
  const promisesBase = core.loadExtScript("ext:deno_node/internal/dns/promises.ts").default;
  const { dnsException, ERR_INVALID_ARG_TYPE, ERR_INVALID_ARG_VALUE, ERR_MISSING_ARGS, handleDnsError } = core.loadExtScript("ext:deno_node/internal/errors.ts");
  const { AI_ADDRCONFIG: ADDRCONFIG, AI_ALL: ALL, AI_V4MAPPED: V4MAPPED } = core.loadExtScript("ext:deno_node/internal_binding/ares.ts");
  const { default: cares, GetAddrInfoReqWrap, GetNameInfoReqWrap, QueryReqWrap, kPermTokenSink } = core.loadExtScript("ext:deno_node/internal_binding/cares_wrap.ts");
  const { toASCII } = core.loadExtScript("ext:deno_node/internal/idna.ts");
  const { ArrayPrototypeMap, ObjectCreate, ObjectDefineProperty, ReflectApply } = primordials;
  function onlookup(err, addresses, netPermToken) {
    if (err) {
      return this.callback(dnsException(err, "getaddrinfo", this.hostname));
    }
    // The NetPermToken is only handed to net.connect's built-in lookup, which
    // tags its callback with `kPermTokenSink`. User-supplied callbacks never
    // bear the marker and so never observe the token (GHSA-fhjh-jqv7-m238).
    if (this.callback[kPermTokenSink]) {
      this.callback(null, addresses[0], this.family || isIP(addresses[0]), netPermToken);
    } else {
      this.callback(null, addresses[0], this.family || isIP(addresses[0]));
    }
  }
  function onlookupall(err, addresses, netPermToken) {
    if (err) {
      return this.callback(dnsException(err, "getaddrinfo", this.hostname));
    }
    const family = this.family;
    const parsedAddresses = [];
    for(let i = 0; i < addresses.length; i++){
      const addr = addresses[i];
      parsedAddresses[i] = {
        address: addr,
        family: family || isIP(addr)
      };
    }
    // Only net.connect's built-in lookup (tagged with `kPermTokenSink`) is given
    // the NetPermToken; user-supplied callbacks never observe it.
    if (this.callback[kPermTokenSink]) {
      this.callback(null, parsedAddresses, undefined, netPermToken);
    } else {
      this.callback(null, parsedAddresses);
    }
  }
  const validFamilies = [
    0,
    4,
    6
  ];
  function lookup(hostname, options, callback) {
    let hints = 0;
    let family = 0;
    let all = false;
    let dnsOrder = getDefaultDnsOrder();
    let port = undefined;
    // Parse arguments
    if (hostname) {
      validateString(hostname, "hostname");
    }
    if (isLookupCallback(options)) {
      callback = options;
      family = 0;
    } else if (isFamily(options)) {
      validateFunction(callback, "callback");
      validateOneOf(options, "family", validFamilies);
      family = options;
    } else if (!isLookupOptions(options)) {
      validateFunction(arguments.length === 2 ? options : callback, "callback");
      throw new ERR_INVALID_ARG_TYPE("options", [
        "integer",
        "object"
      ], options);
    } else {
      validateFunction(callback, "callback");
      if (options?.hints != null) {
        validateNumber(options.hints, "options.hints");
        hints = options.hints >>> 0;
        validateHints(hints);
      }
      if (options?.family != null) {
        // Accept both numeric (0, 4, 6) and string ('IPv4', 'IPv6') family values
        // to match Node.js behavior
        switch(options.family){
          case "IPv4":
            family = 4;
            break;
          case "IPv6":
            family = 6;
            break;
          default:
            validateOneOf(options.family, "options.family", validFamilies);
            family = options.family;
        }
      }
      if (options?.all != null) {
        validateBoolean(options.all, "options.all");
        all = options.all;
      }
      if (options?.verbatim != null) {
        validateBoolean(options.verbatim, "options.verbatim");
        dnsOrder = options.verbatim ? "verbatim" : "ipv4first";
      }
      if (options?.order != null) {
        validateOneOf(options.order, "options.order", validDnsOrders);
        dnsOrder = options.order;
      }
      if (options?.port != null) {
        validateNumber(options.port, "options.port");
        port = options.port;
      }
    }
    if (!hostname) {
      if (all) {
        nextTick(callback, null, []);
      } else {
        nextTick(callback, null, null, family === 6 ? 6 : 4);
      }
      return {};
    }
    const matchedFamily = isIP(hostname);
    if (matchedFamily) {
      if (all) {
        nextTick(callback, null, [
          {
            address: hostname,
            family: matchedFamily
          }
        ]);
      } else {
        nextTick(callback, null, hostname, matchedFamily);
      }
      return {};
    }
    const req = new GetAddrInfoReqWrap();
    req.callback = callback;
    req.family = family;
    req.hostname = hostname;
    req.oncomplete = all ? onlookupall : onlookup;
    req.port = port;
    const err = cares.getaddrinfo(req, toASCII(hostname), family, hints, dnsOrderToNumber(dnsOrder));
    if (err) {
      nextTick(callback, dnsException(err, "getaddrinfo", hostname));
      return {};
    }
    return req;
  }
  ObjectDefineProperty(lookup, customPromisifyArgs, {
    __proto__: null,
    value: [
      "address",
      "family"
    ],
    enumerable: false
  });
  function onlookupservice(err, hostname, service) {
    if (err) {
      return this.callback(handleDnsError(err, "getnameinfo", this.address));
    }
    this.callback(err, hostname, service);
  }
  function lookupService(address, port, callback) {
    if (arguments.length !== 3) {
      throw new ERR_MISSING_ARGS("address", "port", "callback");
    }
    if (isIP(address) === 0) {
      throw new ERR_INVALID_ARG_VALUE("address", address);
    }
    port = validatePort(port);
    validateFunction(callback, "callback");
    const req = new GetNameInfoReqWrap();
    req.callback = callback;
    req.address = address;
    req.port = port;
    req.oncomplete = onlookupservice;
    const errCode = cares.getnameinfo(req, address, port);
    if (errCode) {
      throw dnsException(errCode, "getnameinfo", address);
    }
    return req;
  }
  ObjectDefineProperty(lookupService, customPromisifyArgs, {
    __proto__: null,
    value: [
      "hostname",
      "service"
    ],
    enumerable: false
  });
  function onresolve(err, records, ttls) {
    if (err) {
      this.callback(dnsException(err, this.bindingName, this.hostname));
      return;
    }
    const parsedRecords = ttls && this.ttl ? ArrayPrototypeMap(records, (address, index)=>({
        address,
        ttl: ttls[index]
      })) : records;
    this.callback(null, parsedRecords);
  }
  function resolver(bindingName) {
    function query(name, options, callback) {
      if (isResolveCallback(options)) {
        callback = options;
        options = {};
      }
      validateString(name, "name");
      validateFunction(callback, "callback");
      const req = new QueryReqWrap();
      req.bindingName = bindingName;
      req.callback = callback;
      req.hostname = name;
      req.oncomplete = onresolve;
      req.ttl = !!(options && options.ttl);
      const err = this._handle[bindingName](req, toASCII(name));
      if (err) {
        throw dnsException(err, bindingName, name);
      }
      return req;
    }
    ObjectDefineProperty(query, "name", {
      __proto__: null,
      value: bindingName
    });
    return query;
  }
  const resolveMap = ObjectCreate(null);
  class Resolver extends CallbackResolver {
    constructor(options){
      super(options);
    }
  }
  Resolver.prototype.resolveAny = resolveMap.ANY = resolver("queryAny");
  Resolver.prototype.resolve4 = resolveMap.A = resolver("queryA");
  Resolver.prototype.resolve6 = resolveMap.AAAA = resolver("queryAaaa");
  Resolver.prototype.resolveCaa = resolveMap.CAA = resolver("queryCaa");
  Resolver.prototype.resolveCname = resolveMap.CNAME = resolver("queryCname");
  Resolver.prototype.resolveMx = resolveMap.MX = resolver("queryMx");
  Resolver.prototype.resolveNs = resolveMap.NS = resolver("queryNs");
  Resolver.prototype.resolveTxt = resolveMap.TXT = resolver("queryTxt");
  Resolver.prototype.resolveSrv = resolveMap.SRV = resolver("querySrv");
  Resolver.prototype.resolvePtr = resolveMap.PTR = resolver("queryPtr");
  Resolver.prototype.resolveNaptr = resolveMap.NAPTR = resolver("queryNaptr");
  Resolver.prototype.resolveSoa = resolveMap.SOA = resolver("querySoa");
  Resolver.prototype.reverse = resolver("getHostByAddr");
  Resolver.prototype.resolve = _resolve;
  function _resolve(hostname, rrtype, callback) {
    let resolver;
    if (typeof hostname !== "string") {
      throw new ERR_INVALID_ARG_TYPE("name", "string", hostname);
    }
    if (typeof rrtype === "string") {
      resolver = resolveMap[rrtype];
    } else if (typeof rrtype === "function") {
      resolver = resolveMap.A;
      callback = rrtype;
    } else {
      throw new ERR_INVALID_ARG_TYPE("rrtype", "string", rrtype);
    }
    if (typeof resolver === "function") {
      return ReflectApply(resolver, this, [
        hostname,
        callback
      ]);
    }
    throw new ERR_INVALID_ARG_VALUE("rrtype", rrtype);
  }
  /**
 * Sets the IP address and port of servers to be used when performing DNS
 * resolution. The `servers` argument is an array of [RFC 5952](https://tools.ietf.org/html/rfc5952#section-6) formatted
 * addresses. If the port is the IANA default DNS port (53) it can be omitted.
 *
 * ```js
 * dns.setServers([
 *   '4.4.4.4',
 *   '[2001:4860:4860::8888]',
 *   '4.4.4.4:1053',
 *   '[2001:4860:4860::8888]:1053',
 * ]);
 * ```
 *
 * An error will be thrown if an invalid address is provided.
 *
 * The `dns.setServers()` method must not be called while a DNS query is in
 * progress.
 *
 * The `setServers` method affects only `resolve`,`dns.resolve*()` and `reverse` (and specifically _not_ `lookup`).
 *
 * This method works much like [resolve.conf](https://man7.org/linux/man-pages/man5/resolv.conf.5.html).
 * That is, if attempting to resolve with the first server provided results in a
 * `NOTFOUND` error, the `resolve()` method will _not_ attempt to resolve with
 * subsequent servers provided. Fallback DNS servers will only be used if the
 * earlier ones time out or result in some other error.
 *
 * @param servers array of `RFC 5952` formatted addresses
 */ function setServers(servers) {
    const resolver = new Resolver();
    resolver.setServers(servers);
    setDefaultResolver(resolver);
  }
  // The Node implementation uses `bindDefaultResolver` to set the follow methods
  // on `module.exports` bound to the current `defaultResolver`. We don't have
  // the same ability in ESM but can simulate this (at some cost) by explicitly
  // exporting these methods which dynamically bind to the default resolver when
  // called.
  /**
 * Returns an array of IP address strings, formatted according to [RFC 5952](https://tools.ietf.org/html/rfc5952#section-6),
 * that are currently configured for DNS resolution. A string will include a port
 * section if a custom port is used.
 *
 * ```js
 * [
 *   '4.4.4.4',
 *   '2001:4860:4860::8888',
 *   '4.4.4.4:1053',
 *   '[2001:4860:4860::8888]:1053',
 * ]
 * ```
 */ function getServers() {
    return ReflectApply(Resolver.prototype.getServers, getDefaultResolver(), []);
  }
  function resolveAny(...args) {
    return ReflectApply(Resolver.prototype.resolveAny, getDefaultResolver(), args);
  }
  function resolve4(hostname, options, callback) {
    return ReflectApply(Resolver.prototype.resolve4, getDefaultResolver(), [
      hostname,
      options,
      callback
    ]);
  }
  function resolve6(hostname, options, callback) {
    return ReflectApply(Resolver.prototype.resolve6, getDefaultResolver(), [
      hostname,
      options,
      callback
    ]);
  }
  function resolveCaa(...args) {
    return ReflectApply(Resolver.prototype.resolveCaa, getDefaultResolver(), args);
  }
  function resolveCname(...args) {
    return ReflectApply(Resolver.prototype.resolveCname, getDefaultResolver(), args);
  }
  function resolveMx(...args) {
    return ReflectApply(Resolver.prototype.resolveMx, getDefaultResolver(), args);
  }
  function resolveNs(...args) {
    return ReflectApply(Resolver.prototype.resolveNs, getDefaultResolver(), args);
  }
  function resolveTxt(...args) {
    return ReflectApply(Resolver.prototype.resolveTxt, getDefaultResolver(), args);
  }
  function resolveSrv(...args) {
    return ReflectApply(Resolver.prototype.resolveSrv, getDefaultResolver(), args);
  }
  function resolvePtr(...args) {
    return ReflectApply(Resolver.prototype.resolvePtr, getDefaultResolver(), args);
  }
  function resolveNaptr(...args) {
    return ReflectApply(Resolver.prototype.resolveNaptr, getDefaultResolver(), args);
  }
  function resolveSoa(...args) {
    return ReflectApply(Resolver.prototype.resolveSoa, getDefaultResolver(), args);
  }
  function reverse(...args) {
    return ReflectApply(Resolver.prototype.reverse, getDefaultResolver(), args);
  }
  function resolve(hostname, rrtype, callback) {
    return ReflectApply(Resolver.prototype.resolve, getDefaultResolver(), [
      hostname,
      rrtype,
      callback
    ]);
  }
  // ERROR CODES
  const NODATA = "ENODATA";
  const FORMERR = "EFORMERR";
  const SERVFAIL = "ESERVFAIL";
  const NOTFOUND = "ENOTFOUND";
  const NOTIMP = "ENOTIMP";
  const REFUSED = "EREFUSED";
  const BADQUERY = "EBADQUERY";
  const BADNAME = "EBADNAME";
  const BADFAMILY = "EBADFAMILY";
  const BADRESP = "EBADRESP";
  const CONNREFUSED = "ECONNREFUSED";
  const TIMEOUT = "ETIMEOUT";
  const EOF = "EOF";
  const FILE = "EFILE";
  const NOMEM = "ENOMEM";
  const DESTRUCTION = "EDESTRUCTION";
  const BADSTR = "EBADSTR";
  const BADFLAGS = "EBADFLAGS";
  const NONAME = "ENONAME";
  const BADHINTS = "EBADHINTS";
  const NOTINITIALIZED = "ENOTINITIALIZED";
  const LOADIPHLPAPI = "ELOADIPHLPAPI";
  const ADDRGETNETWORKPARAMS = "EADDRGETNETWORKPARAMS";
  const CANCELLED = "ECANCELLED";
  const promises = {
    ...promisesBase,
    getDefaultResultOrder: getDefaultDnsOrder,
    setDefaultResultOrder,
    setServers,
    // ERROR CODES
    NODATA,
    FORMERR,
    SERVFAIL,
    NOTFOUND,
    NOTIMP,
    REFUSED,
    BADQUERY,
    BADNAME,
    BADFAMILY,
    BADRESP,
    CONNREFUSED,
    TIMEOUT,
    EOF,
    FILE,
    NOMEM,
    DESTRUCTION,
    BADSTR,
    BADFLAGS,
    NONAME,
    BADHINTS,
    NOTINITIALIZED,
    LOADIPHLPAPI,
    ADDRGETNETWORKPARAMS,
    CANCELLED
  };
  const defaultExport = {
    ADDRCONFIG,
    ALL,
    V4MAPPED,
    lookup,
    lookupService,
    getServers,
    resolveAny,
    resolve4,
    resolve6,
    resolveCaa,
    resolveCname,
    resolveMx,
    resolveNs,
    resolveTxt,
    resolveSrv,
    resolvePtr,
    resolveNaptr,
    resolveSoa,
    resolve,
    Resolver,
    reverse,
    setServers,
    getDefaultResultOrder: getDefaultDnsOrder,
    setDefaultResultOrder,
    promises,
    NODATA,
    FORMERR,
    SERVFAIL,
    NOTFOUND,
    NOTIMP,
    REFUSED,
    BADQUERY,
    BADNAME,
    BADFAMILY,
    BADRESP,
    CONNREFUSED,
    TIMEOUT,
    EOF,
    FILE,
    NOMEM,
    DESTRUCTION,
    BADSTR,
    BADFLAGS,
    NONAME,
    BADHINTS,
    NOTINITIALIZED,
    LOADIPHLPAPI,
    ADDRGETNETWORKPARAMS,
    CANCELLED
  };
  return {
    default: defaultExport,
    ADDRCONFIG,
    ALL,
    V4MAPPED,
    lookup,
    lookupService,
    getServers,
    resolveAny,
    resolve4,
    resolve6,
    resolveCaa,
    resolveCname,
    resolveMx,
    resolveNs,
    resolveTxt,
    resolveSrv,
    resolvePtr,
    resolveNaptr,
    resolveSoa,
    resolve,
    Resolver,
    reverse,
    setServers,
    getDefaultResultOrder: getDefaultDnsOrder,
    setDefaultResultOrder,
    promises,
    NODATA,
    FORMERR,
    SERVFAIL,
    NOTFOUND,
    NOTIMP,
    REFUSED,
    BADQUERY,
    BADNAME,
    BADFAMILY,
    BADRESP,
    CONNREFUSED,
    TIMEOUT,
    EOF,
    FILE,
    NOMEM,
    DESTRUCTION,
    BADSTR,
    BADFLAGS,
    NONAME,
    BADHINTS,
    NOTINITIALIZED,
    LOADIPHLPAPI,
    ADDRGETNETWORKPARAMS,
    CANCELLED
  };
})());