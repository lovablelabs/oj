"use strict"; return ((function() {
  const { core, primordials } = __bootstrap;
  const { ArrayPrototypeFilter, ArrayPrototypeJoin, ArrayPrototypeMap, ArrayPrototypePush, ArrayPrototypeReverse, ArrayPrototypeSort, Error, MathMin, MathPow, Number, NumberParseInt, NumberPrototypeToString, ObjectPrototypeIsPrototypeOf, PromisePrototypeThen, SafeArrayIterator, SafeRegExp, SafeSet, SafeSetIterator, SetPrototypeAdd, SetPrototypeClear, SetPrototypeDelete, SetPrototypeHas, StringPrototypeIncludes, StringPrototypePadStart, StringPrototypeReplace, StringPrototypeSplit, Symbol } = primordials;
  const { op_dns_resolve, op_net_get_ips_from_perm_token, op_net_get_system_dns_servers, op_node_getaddrinfo, op_node_getnameinfo } = core.ops;
  const { isIPv4, isIPv6 } = core.loadExtScript("ext:deno_node/internal/net.ts");
  const { codeMap } = core.loadExtScript("ext:deno_node/internal_binding/uv.ts");
  const { AsyncWrap, providerType } = core.loadExtScript("ext:deno_node/internal_binding/async_wrap.ts");
  const { ares_strerror } = core.loadExtScript("ext:deno_node/internal_binding/ares.ts");
  const DNS_ORDER_VERBATIM = 0;
  const DNS_ORDER_IPV4_FIRST = 1;
  const DNS_ORDER_IPV6_FIRST = 2;
  // Module-private marker placed on the getaddrinfo completion callback used by
  // net.connect's *built-in* lookup. Only a callback bearing this symbol is
  // handed the NetPermToken from a lookup, so the token never escapes to a
  // user-supplied dns.lookup callback or a custom net.connect `lookup` function.
  // User code cannot reference this symbol, so it can neither receive the token
  // nor forge the marker. See GHSA-fhjh-jqv7-m238.
  const kPermTokenSink = Symbol("kPermTokenSink");
  class GetAddrInfoReqWrap extends AsyncWrap {
    family;
    hostname;
    port;
    callback;
    resolve;
    reject;
    oncomplete;
    constructor(){
      super(providerType.GETADDRINFOREQWRAP);
    }
  }
  function getaddrinfo(req, hostname, family, _hints, order) {
    let addresses = [];
    // TODO(cmorten): use hints
    // REF: https://nodejs.org/api/dns.html#dns_supported_getaddrinfo_flags
    (async ()=>{
      let error = 0;
      let netPermToken;
      try {
        netPermToken = await op_node_getaddrinfo(hostname, req.port || undefined, family);
        ArrayPrototypePush(addresses, ...new SafeArrayIterator(op_net_get_ips_from_perm_token(netPermToken)));
        if (addresses.length === 0) {
          error = codeMap.get("EAI_NODATA");
        }
      } catch (e) {
        if (ObjectPrototypeIsPrototypeOf(Deno.errors.NotCapable.prototype, e)) {
          error = codeMap.get("EPERM");
        } else if (typeof e?.uv_errcode === "number" && e.uv_errcode !== 0) {
          // Propagate the real libuv error code reported by `getaddrinfo`
          // (e.g. `EAI_NONAME`/`ENOTFOUND`) instead of flattening every failure
          // to `EAI_NODATA`, so the resulting error matches Node.js.
          error = e.uv_errcode;
        } else {
          error = codeMap.get("EAI_NODATA");
        }
      }
      // REF: https://github.com/nodejs/node/blob/0e157b6cd8694424ea9d8a1c1854fd1d08cbb064/src/cares_wrap.cc#L1739
      if (order === DNS_ORDER_IPV4_FIRST) {
        ArrayPrototypeSort(addresses, (a, b)=>{
          if (isIPv4(a)) {
            return -1;
          } else if (isIPv4(b)) {
            return 1;
          }
          return 0;
        });
      } else if (order === DNS_ORDER_IPV6_FIRST) {
        ArrayPrototypeSort(addresses, (a, b)=>{
          if (isIPv6(a)) {
            return -1;
          } else if (isIPv6(b)) {
            return 1;
          }
          return 0;
        });
      }
      if (family === 4) {
        addresses = ArrayPrototypeFilter(addresses, (addr)=>isIPv4(addr));
      } else if (family === 6) {
        addresses = ArrayPrototypeFilter(addresses, (addr)=>isIPv6(addr));
      }
      req.oncomplete(error, addresses, netPermToken);
    })();
    return 0;
  }
  class GetNameInfoReqWrap extends AsyncWrap {
    address;
    port;
    callback;
    resolve;
    reject;
    oncomplete;
    constructor(){
      super(providerType.GETNAMEINFOREQWRAP);
    }
  }
  function getnameinfo(req, address, port) {
    (async ()=>{
      try {
        const result = await op_node_getnameinfo(address, port);
        req.oncomplete(null, result[0], result[1]);
      } catch (err) {
        req.oncomplete(err);
      }
    })();
    return 0;
  }
  class QueryReqWrap extends AsyncWrap {
    bindingName;
    hostname;
    ttl;
    callback;
    // deno-lint-ignore no-explicit-any
    resolve;
    reject;
    oncomplete;
    constructor(){
      super(providerType.QUERYWRAP);
    }
  }
  const trailingDotRegExp = new SafeRegExp(/\.$/);
  function fqdnToHostname(fqdn) {
    return StringPrototypeReplace(fqdn, trailingDotRegExp, "");
  }
  let systemDnsServers = null;
  function getSystemDnsServers() {
    if (systemDnsServers !== null) {
      return systemDnsServers;
    }
    systemDnsServers = op_net_get_system_dns_servers();
    return systemDnsServers;
  }
  class ChannelWrap extends AsyncWrap {
    #servers = null;
    #timeout;
    #tries;
    #maxTimeout;
    #pendingQueries = new SafeSet();
    #cancelRids = new SafeSet();
    // Local bind address(es) set via `setLocalAddress`. Currently only stored so
    // the call matches Node's behavior; not yet applied to outgoing queries.
    #localAddress = null;
    constructor(timeout, tries, maxTimeout){
      super(providerType.DNSCHANNEL);
      this.#timeout = timeout;
      this.#tries = tries;
      this.#maxTimeout = maxTimeout;
    }
    async #query(query, recordType, ttl) {
      // deno-lint-ignore no-explicit-any
      let code;
      let ret;
      if (this.#servers !== null && this.#servers.length) {
        for (const server of new SafeArrayIterator(this.#servers)){
          const ipAddr = server[0];
          const port = server[1];
          const resolveOptions = {
            nameServer: {
              ipAddr,
              port
            }
          };
          ({ code, ret } = await this.#resolve(query, recordType, resolveOptions, ttl));
          if (code === 0 || code === codeMap.get("EAI_NODATA") || code === "ENOTFOUND" || code === "ENODATA" || code === "ETIMEOUT") {
            break;
          }
        }
      } else {
        ({ code, ret } = await this.#resolve(query, recordType, null, ttl));
      }
      return {
        code: code,
        ret: ret
      };
    }
    async #resolve(query, recordType, resolveOptions, ttl) {
      const tries = this.#tries > 0 ? this.#tries : 1;
      for(let attempt = 0; attempt < tries; attempt++){
        let ret = [];
        // deno-lint-ignore no-explicit-any
        let code = 0;
        // Always create a cancel handle so cancel() can abort in-flight ops.
        const cancelRid = core.createCancelHandle();
        this.#cancelRids.add(cancelRid);
        let timer;
        // Whether this attempt was aborted by its own timeout timer (rather than
        // by an explicit cancel()). The op is not given a timeout, so the manual
        // timer below is what enforces the per-attempt `timeout`; when it fires
        // the op throws `Interrupted`, which must be treated as a timeout and
        // retried, exactly like the `TimedOut` hickory raises when it wins first.
        let timedOut = false;
        try {
          if (this.#timeout >= 0) {
            // c-ares doubles timeout on each retry, capped by maxTimeout
            let currentTimeout = this.#timeout * MathPow(2, attempt);
            if (this.#maxTimeout >= 0) {
              currentTimeout = MathMin(currentTimeout, this.#maxTimeout);
            }
            timer = setTimeout(()=>{
              timedOut = true;
              this.#cancelRids.delete(cancelRid);
              core.tryClose(cancelRid);
            }, currentTimeout);
          }
          const res = await op_dns_resolve({
            query,
            recordType,
            options: resolveOptions,
            cancelRid
          }, /* useEdns0 */ false);
          if (ttl) {
            ret = res;
          } else {
            ret = ArrayPrototypeMap(res, (recordWithTtl)=>recordWithTtl.data);
          }
          return {
            code,
            ret
          };
        } catch (e) {
          if (ObjectPrototypeIsPrototypeOf(Deno.errors.Interrupted.prototype, e)) {
            if (timedOut) {
              // Our own timeout timer aborted the op - this is a timeout, so
              // retry with the next (longer) timeout if attempts remain.
              if (attempt < tries - 1) continue;
            }
            // Either the attempts are exhausted or this was an explicit cancel();
            // in both cases stop and report a timeout.
            code = "ETIMEOUT";
          } else if (ObjectPrototypeIsPrototypeOf(Deno.errors.TimedOut.prototype, e)) {
            // TimedOut from hickory - retry if attempts remain
            if (attempt < tries - 1) continue;
            code = "ETIMEOUT";
          } else if (typeof e?.ares_code === "string" && e.ares_code !== "") {
            // The Rust op classified the underlying resolver error into a
            // c-ares style code (e.g. `EBADNAME`, `ENOTFOUND`, `ENODATA`).
            // Pass the string code through so `dnsException` reports the same
            // `code` (and `errno: undefined`) as Node.js.
            code = e.ares_code;
          } else if (ObjectPrototypeIsPrototypeOf(Deno.errors.NotFound.prototype, e)) {
            // A "not found" error without a more specific c-ares code, e.g. an
            // empty answer. Report `ENOTFOUND`, matching Node.js.
            code = "ENOTFOUND";
          } else {
            // TODO(cmorten): map errors to appropriate error codes.
            code = codeMap.get("UNKNOWN");
          }
          return {
            code,
            ret
          };
        } finally{
          if (timer !== undefined) clearTimeout(timer);
          this.#cancelRids.delete(cancelRid);
          core.tryClose(cancelRid);
        }
      }
      return {
        code: codeMap.get("UNKNOWN"),
        ret: []
      };
    }
    queryAny(req, name) {
      SetPrototypeAdd(this.#pendingQueries, req);
      PromisePrototypeThen(// deno-lint-ignore no-explicit-any
      this.#query(name, "ANY", true), ({ code, ret })=>{
        if (!SetPrototypeHas(this.#pendingQueries, req)) return;
        SetPrototypeDelete(this.#pendingQueries, req);
        if (code !== 0) {
          req.oncomplete(code, []);
          return;
        }
        const records = [];
        for (const entry of new SafeArrayIterator(ret)){
          const data = entry?.data ?? entry;
          const ttl = entry?.ttl ?? 0;
          const rt = entry?.recordType;
          switch(rt){
            case "A":
              ArrayPrototypePush(records, {
                type: "A",
                address: data,
                ttl
              });
              break;
            case "AAAA":
              ArrayPrototypePush(records, {
                type: "AAAA",
                address: data,
                ttl
              });
              break;
            case "MX":
              ArrayPrototypePush(records, {
                type: "MX",
                priority: data.preference,
                exchange: fqdnToHostname(data.exchange)
              });
              break;
            case "NS":
              ArrayPrototypePush(records, {
                type: "NS",
                value: fqdnToHostname(data)
              });
              break;
            case "TXT":
              ArrayPrototypePush(records, {
                type: "TXT",
                entries: data
              });
              break;
            case "PTR":
              ArrayPrototypePush(records, {
                type: "PTR",
                value: fqdnToHostname(data)
              });
              break;
            case "SOA":
              ArrayPrototypePush(records, {
                type: "SOA",
                nsname: fqdnToHostname(data.mname),
                hostmaster: fqdnToHostname(data.rname),
                serial: data.serial,
                refresh: data.refresh,
                retry: data.retry,
                expire: data.expire,
                minttl: data.minimum
              });
              break;
            case "CAA":
              ArrayPrototypePush(records, {
                type: "CAA",
                [data.tag]: data.value,
                critical: +data.critical && 128
              });
              break;
            case "CNAME":
              ArrayPrototypePush(records, {
                type: "CNAME",
                value: data
              });
              break;
            case "NAPTR":
              ArrayPrototypePush(records, {
                type: "NAPTR",
                order: data.order,
                preference: data.preference,
                flags: data.flags,
                service: data.services,
                regexp: data.regexp,
                replacement: data.replacement
              });
              break;
            case "SRV":
              ArrayPrototypePush(records, {
                type: "SRV",
                priority: data.priority,
                weight: data.weight,
                port: data.port,
                name: fqdnToHostname(data.target)
              });
              break;
          }
        }
        const err = records.length ? 0 : codeMap.get("EAI_NODATA");
        req.oncomplete(err, records);
      });
      return 0;
    }
    queryA(req, name) {
      SetPrototypeAdd(this.#pendingQueries, req);
      PromisePrototypeThen(this.#query(name, "A", req.ttl), ({ code, ret })=>{
        if (!SetPrototypeHas(this.#pendingQueries, req)) return;
        SetPrototypeDelete(this.#pendingQueries, req);
        let recordsWithTtl;
        if (req.ttl) {
          recordsWithTtl = ArrayPrototypeMap(ret, (val)=>({
              address: val?.data,
              ttl: val?.ttl
            }));
        }
        req.oncomplete(code, recordsWithTtl ?? ret);
      });
      return 0;
    }
    queryAaaa(req, name) {
      SetPrototypeAdd(this.#pendingQueries, req);
      PromisePrototypeThen(this.#query(name, "AAAA", req.ttl), ({ code, ret })=>{
        if (!SetPrototypeHas(this.#pendingQueries, req)) return;
        SetPrototypeDelete(this.#pendingQueries, req);
        let recordsWithTtl;
        if (req.ttl) {
          recordsWithTtl = ArrayPrototypeMap(ret, (val)=>({
              address: val?.data,
              ttl: val?.ttl
            }));
        }
        req.oncomplete(code, recordsWithTtl ?? ret);
      });
      return 0;
    }
    queryCaa(req, name) {
      SetPrototypeAdd(this.#pendingQueries, req);
      PromisePrototypeThen(this.#query(name, "CAA"), ({ code, ret })=>{
        if (!SetPrototypeHas(this.#pendingQueries, req)) return;
        SetPrototypeDelete(this.#pendingQueries, req);
        const records = ArrayPrototypeMap(ret, ({ critical, tag, value })=>({
            [tag]: value,
            critical: +critical && 128
          }));
        req.oncomplete(code, records);
      });
      return 0;
    }
    queryCname(req, name) {
      SetPrototypeAdd(this.#pendingQueries, req);
      PromisePrototypeThen(this.#query(name, "CNAME"), ({ code, ret })=>{
        if (!SetPrototypeHas(this.#pendingQueries, req)) return;
        SetPrototypeDelete(this.#pendingQueries, req);
        req.oncomplete(code, ret);
      });
      return 0;
    }
    queryMx(req, name) {
      SetPrototypeAdd(this.#pendingQueries, req);
      PromisePrototypeThen(this.#query(name, "MX"), ({ code, ret })=>{
        if (!SetPrototypeHas(this.#pendingQueries, req)) return;
        SetPrototypeDelete(this.#pendingQueries, req);
        const records = ArrayPrototypeMap(ret, ({ preference, exchange })=>({
            priority: preference,
            exchange: fqdnToHostname(exchange)
          }));
        req.oncomplete(code, records);
      });
      return 0;
    }
    queryNaptr(req, name) {
      SetPrototypeAdd(this.#pendingQueries, req);
      PromisePrototypeThen(this.#query(name, "NAPTR"), ({ code, ret })=>{
        if (!SetPrototypeHas(this.#pendingQueries, req)) return;
        SetPrototypeDelete(this.#pendingQueries, req);
        const records = ArrayPrototypeMap(ret, ({ order, preference, flags, services, regexp, replacement })=>({
            flags,
            service: services,
            regexp,
            replacement,
            order,
            preference
          }));
        req.oncomplete(code, records);
      });
      return 0;
    }
    queryNs(req, name) {
      SetPrototypeAdd(this.#pendingQueries, req);
      PromisePrototypeThen(this.#query(name, "NS"), ({ code, ret })=>{
        if (!SetPrototypeHas(this.#pendingQueries, req)) return;
        SetPrototypeDelete(this.#pendingQueries, req);
        const records = ArrayPrototypeMap(ret, (record)=>fqdnToHostname(record));
        req.oncomplete(code, records);
      });
      return 0;
    }
    queryPtr(req, name) {
      SetPrototypeAdd(this.#pendingQueries, req);
      PromisePrototypeThen(this.#query(name, "PTR"), ({ code, ret })=>{
        if (!SetPrototypeHas(this.#pendingQueries, req)) return;
        SetPrototypeDelete(this.#pendingQueries, req);
        const records = ArrayPrototypeMap(ret, (record)=>fqdnToHostname(record));
        req.oncomplete(code, records);
      });
      return 0;
    }
    querySoa(req, name) {
      SetPrototypeAdd(this.#pendingQueries, req);
      PromisePrototypeThen(this.#query(name, "SOA"), ({ code, ret })=>{
        if (!SetPrototypeHas(this.#pendingQueries, req)) return;
        SetPrototypeDelete(this.#pendingQueries, req);
        let record = {};
        if (ret.length) {
          const { mname, rname, serial, refresh, retry, expire, minimum } = ret[0];
          record = {
            nsname: fqdnToHostname(mname),
            hostmaster: fqdnToHostname(rname),
            serial,
            refresh,
            retry,
            expire,
            minttl: minimum
          };
        }
        req.oncomplete(code, record);
      });
      return 0;
    }
    querySrv(req, name) {
      SetPrototypeAdd(this.#pendingQueries, req);
      PromisePrototypeThen(this.#query(name, "SRV"), ({ code, ret })=>{
        if (!SetPrototypeHas(this.#pendingQueries, req)) return;
        SetPrototypeDelete(this.#pendingQueries, req);
        const records = ArrayPrototypeMap(ret, ({ priority, weight, port, target })=>({
            priority,
            weight,
            port,
            name: fqdnToHostname(target)
          }));
        req.oncomplete(code, records);
      });
      return 0;
    }
    queryTxt(req, name) {
      SetPrototypeAdd(this.#pendingQueries, req);
      PromisePrototypeThen(this.#query(name, "TXT"), ({ code, ret })=>{
        if (!SetPrototypeHas(this.#pendingQueries, req)) return;
        SetPrototypeDelete(this.#pendingQueries, req);
        req.oncomplete(code, ret);
      });
      return 0;
    }
    getHostByAddr(req, name) {
      let reverseName;
      if (isIPv4(name)) {
        const octets = StringPrototypeSplit(name, ".");
        reverseName = ArrayPrototypeJoin(ArrayPrototypeReverse(octets), ".") + ".in-addr.arpa";
      } else if (isIPv6(name)) {
        // Expand the IPv6 address to full form
        const parts = StringPrototypeSplit(name, ":");
        const expanded = [];
        let emptyFound = false;
        for (const part of new SafeArrayIterator(parts)){
          if (part === "" && !emptyFound) {
            emptyFound = true;
            const missing = 8 - ArrayPrototypeFilter(parts, (p)=>p !== "").length;
            for(let j = 0; j < missing; j++){
              ArrayPrototypePush(expanded, "0000");
            }
          } else if (part !== "" && StringPrototypeIncludes(part, ".")) {
            // IPv4-mapped IPv6 (e.g. ::ffff:1.2.3.4) - convert dotted
            // quad to two 16-bit hex groups
            const octets = ArrayPrototypeMap(StringPrototypeSplit(part, "."), Number);
            ArrayPrototypePush(expanded, StringPrototypePadStart(NumberPrototypeToString(octets[0] << 8 | octets[1], 16), 4, "0"));
            ArrayPrototypePush(expanded, StringPrototypePadStart(NumberPrototypeToString(octets[2] << 8 | octets[3], 16), 4, "0"));
          } else if (part !== "") {
            ArrayPrototypePush(expanded, StringPrototypePadStart(part, 4, "0"));
          }
        }
        const fullHex = ArrayPrototypeJoin(expanded, "");
        reverseName = ArrayPrototypeJoin(ArrayPrototypeReverse(StringPrototypeSplit(fullHex, "")), ".") + ".ip6.arpa";
      } else {
        req.oncomplete(codeMap.get("EINVAL"), []);
        return 0;
      }
      SetPrototypeAdd(this.#pendingQueries, req);
      PromisePrototypeThen(this.#query(reverseName, "PTR"), ({ code, ret })=>{
        if (!SetPrototypeHas(this.#pendingQueries, req)) return;
        SetPrototypeDelete(this.#pendingQueries, req);
        const records = ArrayPrototypeMap(ret, (record)=>fqdnToHostname(record));
        req.oncomplete(code, records);
      });
      return 0;
    }
    getServers() {
      if (this.#servers === null) {
        return getSystemDnsServers();
      }
      return this.#servers;
    }
    setServers(servers) {
      if (typeof servers === "string") {
        const tuples = [];
        for(let i = 0; i < servers.length; i += 2){
          ArrayPrototypePush(tuples, [
            servers[i],
            NumberParseInt(servers[i + 1])
          ]);
        }
        this.#servers = tuples;
      } else {
        this.#servers = ArrayPrototypeMap(servers, (server)=>[
            server[1],
            server[2]
          ]);
      }
      return 0;
    }
    setLocalAddress(addr0, addr1) {
      // Mirror Node's c-ares `ChannelWrap::SetLocalAddress`: the first argument
      // may be either an IPv4 or IPv6 address; if a second argument is given it
      // must be the *other* family (so exactly one IPv4 and one IPv6 address, in
      // either order). The caller (`Resolver.setLocalAddress` in
      // `internal/dns/utils.ts`) has already validated the arguments are strings.
      //
      // We validate the addresses here and record them so the call no longer
      // throws and matches Node's observable behavior. The underlying resolver op
      // (`op_dns_resolve`) does not yet expose a per-query bind address, so the
      // stored value is not applied to outgoing queries; see
      // https://github.com/denoland/deno/issues/36518.
      let type0;
      if (isIPv4(addr0)) {
        type0 = 4;
      } else if (isIPv6(addr0)) {
        type0 = 6;
      } else {
        throw new Error(`Invalid IP address: ${addr0}`);
      }
      if (addr1 !== undefined) {
        if (isIPv4(addr1)) {
          if (type0 === 4) {
            throw new Error("Cannot specify two IPv4 addresses");
          }
        } else if (isIPv6(addr1)) {
          if (type0 === 6) {
            throw new Error("Cannot specify two IPv6 addresses");
          }
        } else {
          throw new Error(`Invalid IP address: ${addr1}`);
        }
      }
      this.#localAddress = {
        ipv4: type0 === 4 ? addr0 : addr1,
        ipv6: type0 === 6 ? addr0 : addr1
      };
    }
    cancel() {
      for (const req of new SafeSetIterator(this.#pendingQueries)){
        req.oncomplete("ECANCELLED", []);
      }
      SetPrototypeClear(this.#pendingQueries);
      // Abort in-flight DNS operations so the process can exit.
      for (const rid of new SafeSetIterator(this.#cancelRids)){
        core.tryClose(rid);
      }
      SetPrototypeClear(this.#cancelRids);
    }
  }
  const DNS_ESETSRVPENDING = -1000;
  const EMSG_ESETSRVPENDING = "There are pending queries.";
  function strerror(code) {
    return code === DNS_ESETSRVPENDING ? EMSG_ESETSRVPENDING : ares_strerror(code);
  }
  return {
    DNS_ORDER_VERBATIM,
    DNS_ORDER_IPV4_FIRST,
    DNS_ORDER_IPV6_FIRST,
    GetAddrInfoReqWrap,
    getaddrinfo,
    GetNameInfoReqWrap,
    getnameinfo,
    QueryReqWrap,
    ChannelWrap,
    strerror,
    kPermTokenSink,
    default: {
      DNS_ORDER_VERBATIM,
      DNS_ORDER_IPV4_FIRST,
      DNS_ORDER_IPV6_FIRST,
      GetAddrInfoReqWrap,
      getaddrinfo,
      getnameinfo,
      QueryReqWrap,
      ChannelWrap,
      strerror
    }
  };
})());