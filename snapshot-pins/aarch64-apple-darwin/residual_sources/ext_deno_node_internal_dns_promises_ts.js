"use strict"; return ((function() {
  const { core, primordials } = __bootstrap;
  const { ArrayPrototypeMap, FunctionPrototypeBind, ObjectCreate, ObjectDefineProperty, Promise, ReflectApply } = primordials;
  const { validateBoolean, validateNumber, validateOneOf, validatePort, validateString } = core.loadExtScript("ext:deno_node/internal/validators.mjs");
  const { isIP } = core.loadExtScript("ext:deno_node/internal/net.ts");
  const { dnsOrderToNumber, getDefaultDnsOrder, getDefaultResolver, isFamily, isLookupOptions, Resolver: CallbackResolver, validateHints, validDnsOrders } = core.loadExtScript("ext:deno_node/internal/dns/utils.ts");
  const { dnsException, ERR_INVALID_ARG_TYPE, ERR_INVALID_ARG_VALUE, ERR_MISSING_ARGS, handleDnsError } = core.loadExtScript("ext:deno_node/internal/errors.ts");
  const { default: cares, GetAddrInfoReqWrap, GetNameInfoReqWrap, QueryReqWrap } = core.loadExtScript("ext:deno_node/internal_binding/cares_wrap.ts");
  const { toASCII } = core.loadExtScript("ext:deno_node/internal/idna.ts");
  function onlookup(err, addresses) {
    if (err) {
      this.reject(dnsException(err, "getaddrinfo", this.hostname));
      return;
    }
    const family = this.family || isIP(addresses[0]);
    this.resolve({
      address: addresses[0],
      family
    });
  }
  function onlookupall(err, addresses) {
    if (err) {
      this.reject(dnsException(err, "getaddrinfo", this.hostname));
      return;
    }
    const family = this.family;
    const parsedAddresses = [];
    for(let i = 0; i < addresses.length; i++){
      const address = addresses[i];
      parsedAddresses[i] = {
        address,
        family: family ? family : isIP(address)
      };
    }
    this.resolve(parsedAddresses);
  }
  function createLookupPromise(family, hostname, all, hints, dnsOrder) {
    return new Promise((resolve, reject)=>{
      if (!hostname) {
        if (all) {
          resolve([]);
        } else {
          resolve({
            address: null,
            family: family === 6 ? 6 : 4
          });
        }
        return;
      }
      const matchedFamily = isIP(hostname);
      if (matchedFamily !== 0) {
        const result = {
          address: hostname,
          family: matchedFamily
        };
        resolve(all ? [
          result
        ] : result);
        return;
      }
      const req = new GetAddrInfoReqWrap();
      req.family = family;
      req.hostname = hostname;
      req.oncomplete = all ? onlookupall : onlookup;
      req.resolve = resolve;
      req.reject = reject;
      const err = cares.getaddrinfo(req, toASCII(hostname), family, hints, dnsOrderToNumber(dnsOrder));
      if (err) {
        reject(dnsException(err, "getaddrinfo", hostname));
      }
    });
  }
  const validFamilies = [
    0,
    4,
    6
  ];
  function lookup(hostname, options) {
    let hints = 0;
    let family = 0;
    let all = false;
    let dnsOrder = getDefaultDnsOrder();
    // Parse arguments
    if (hostname) {
      validateString(hostname, "hostname");
    }
    if (isFamily(options)) {
      validateOneOf(options, "family", validFamilies);
      family = options;
    } else if (!isLookupOptions(options)) {
      throw new ERR_INVALID_ARG_TYPE("options", [
        "integer",
        "object"
      ], options);
    } else {
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
    }
    return createLookupPromise(family, hostname, all, hints, dnsOrder);
  }
  function onresolve(err, records, ttls) {
    if (err) {
      this.reject(dnsException(err, this.bindingName, this.hostname));
      return;
    }
    const parsedRecords = ttls && this.ttl ? ArrayPrototypeMap(records, (address, index)=>({
        address,
        ttl: ttls[index]
      })) : records;
    this.resolve(parsedRecords);
  }
  function onlookupservice(err, hostname, service) {
    if (err) {
      this.reject(handleDnsError(err, "getnameinfo", this.address));
      return;
    }
    this.resolve({
      hostname: hostname,
      service: service
    });
  }
  function createLookupServicePromise(address, port) {
    return new Promise((resolve, reject)=>{
      const req = new GetNameInfoReqWrap();
      req.address = address;
      req.port = port;
      req.oncomplete = onlookupservice;
      req.resolve = resolve;
      req.reject = reject;
      const errCode = cares.getnameinfo(req, address, port);
      if (errCode) {
        reject(dnsException(errCode, "getnameinfo", address));
      }
    });
  }
  function lookupService(address, port) {
    if (arguments.length !== 2) {
      throw new ERR_MISSING_ARGS("address", "port");
    }
    if (isIP(address) === 0) {
      throw new ERR_INVALID_ARG_VALUE("address", address);
    }
    port = validatePort(port);
    return createLookupServicePromise(address, port);
  }
  function createResolverPromise(resolver, bindingName, hostname, ttl) {
    return new Promise((resolve, reject)=>{
      const req = new QueryReqWrap();
      req.bindingName = bindingName;
      req.hostname = hostname;
      req.oncomplete = onresolve;
      req.resolve = resolve;
      req.reject = reject;
      req.ttl = ttl;
      const err = resolver._handle[bindingName](req, toASCII(hostname));
      if (err) {
        reject(dnsException(err, bindingName, hostname));
      }
    });
  }
  function resolver(bindingName) {
    function query(name, options) {
      validateString(name, "name");
      const ttl = !!(options && options.ttl);
      return createResolverPromise(this, bindingName, name, ttl);
    }
    ObjectDefineProperty(query, "name", {
      __proto__: null,
      value: bindingName
    });
    return query;
  }
  const resolveMap = ObjectCreate(null);
  class Resolver extends CallbackResolver {
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
  function _resolve(hostname, rrtype) {
    let resolver;
    if (typeof hostname !== "string") {
      throw new ERR_INVALID_ARG_TYPE("name", "string", hostname);
    }
    if (rrtype !== undefined) {
      validateString(rrtype, "rrtype");
      resolver = resolveMap[rrtype];
      if (typeof resolver !== "function") {
        throw new ERR_INVALID_ARG_VALUE("rrtype", rrtype);
      }
    } else {
      resolver = resolveMap.A;
    }
    return ReflectApply(resolver, this, [
      hostname
    ]);
  }
  // The Node implementation uses `bindDefaultResolver` to set the follow methods
  // on `module.exports` bound to the current `defaultResolver`. We don't have
  // the same ability in ESM but can simulate this (at some cost) by explicitly
  // exporting these methods which dynamically bind to the default resolver when
  // called.
  function getServers() {
    return FunctionPrototypeBind(Resolver.prototype.getServers, getDefaultResolver())();
  }
  function resolveAny(hostname) {
    return FunctionPrototypeBind(Resolver.prototype.resolveAny, getDefaultResolver())(hostname);
  }
  function resolve4(hostname, options) {
    return FunctionPrototypeBind(Resolver.prototype.resolve4, getDefaultResolver())(hostname, options);
  }
  function resolve6(hostname, options) {
    return FunctionPrototypeBind(Resolver.prototype.resolve6, getDefaultResolver())(hostname, options);
  }
  function resolveCaa(hostname) {
    return FunctionPrototypeBind(Resolver.prototype.resolveCaa, getDefaultResolver())(hostname);
  }
  function resolveCname(hostname) {
    return FunctionPrototypeBind(Resolver.prototype.resolveCname, getDefaultResolver())(hostname);
  }
  function resolveMx(hostname) {
    return FunctionPrototypeBind(Resolver.prototype.resolveMx, getDefaultResolver())(hostname);
  }
  function resolveNs(hostname) {
    return FunctionPrototypeBind(Resolver.prototype.resolveNs, getDefaultResolver())(hostname);
  }
  function resolveTxt(hostname) {
    return FunctionPrototypeBind(Resolver.prototype.resolveTxt, getDefaultResolver())(hostname);
  }
  function resolveSrv(hostname) {
    return FunctionPrototypeBind(Resolver.prototype.resolveSrv, getDefaultResolver())(hostname);
  }
  function resolvePtr(hostname) {
    return FunctionPrototypeBind(Resolver.prototype.resolvePtr, getDefaultResolver())(hostname);
  }
  function resolveNaptr(hostname) {
    return FunctionPrototypeBind(Resolver.prototype.resolveNaptr, getDefaultResolver())(hostname);
  }
  function resolveSoa(hostname) {
    return FunctionPrototypeBind(Resolver.prototype.resolveSoa, getDefaultResolver())(hostname);
  }
  function reverse(ip) {
    return FunctionPrototypeBind(Resolver.prototype.reverse, getDefaultResolver())(ip);
  }
  function resolve(hostname, rrtype) {
    return FunctionPrototypeBind(Resolver.prototype.resolve, getDefaultResolver())(hostname, rrtype);
  }
  return {
    default: {
      lookup,
      lookupService,
      Resolver,
      getDefaultResultOrder: getDefaultDnsOrder,
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
      reverse
    },
    Resolver,
    getDefaultResultOrder: getDefaultDnsOrder,
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
    reverse
  };
})());