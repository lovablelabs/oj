"use strict"; return ((function() {
  const ARES_AI_CANONNAME = 1 << 0;
  const ARES_AI_NUMERICHOST = 1 << 1;
  const ARES_AI_PASSIVE = 1 << 2;
  const ARES_AI_NUMERICSERV = 1 << 3;
  const AI_V4MAPPED = 1 << 4;
  const AI_ALL = 1 << 5;
  const AI_ADDRCONFIG = 1 << 6;
  const ARES_AI_NOSORT = 1 << 7;
  const ARES_AI_ENVHOSTS = 1 << 8;
  // REF: https://github.com/nodejs/node/blob/master/deps/cares/src/lib/ares_strerror.c
  // deno-lint-ignore camelcase
  function ares_strerror(code) {
    /* Return a string literal from a table. */ const errorText = [
      "Successful completion",
      "DNS server returned answer with no data",
      "DNS server claims query was misformatted",
      "DNS server returned general failure",
      "Domain name not found",
      "DNS server does not implement requested operation",
      "DNS server refused query",
      "Misformatted DNS query",
      "Misformatted domain name",
      "Unsupported address family",
      "Misformatted DNS reply",
      "Could not contact DNS servers",
      "Timeout while contacting DNS servers",
      "End of file",
      "Error reading file",
      "Out of memory",
      "Channel is being destroyed",
      "Misformatted string",
      "Illegal flags specified",
      "Given hostname is not numeric",
      "Illegal hints flags specified",
      "c-ares library initialization not yet performed",
      "Error loading iphlpapi.dll",
      "Could not find GetNetworkParams function",
      "DNS query cancelled"
    ];
    if (code >= 0 && code < errorText.length) {
      return errorText[code];
    } else {
      return "unknown";
    }
  }
  return {
    ares_strerror,
    ARES_AI_CANONNAME,
    ARES_AI_NUMERICHOST,
    ARES_AI_PASSIVE,
    ARES_AI_NUMERICSERV,
    AI_V4MAPPED,
    AI_ALL,
    AI_ADDRCONFIG,
    ARES_AI_NOSORT,
    ARES_AI_ENVHOSTS
  };
})());