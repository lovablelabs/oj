"use strict"; return ((function() {
  "use strict";
  const { core, primordials } = __bootstrap;
  const { op_node_idna_to_ascii, op_node_idna_domain_to_ascii, op_node_idna_domain_to_unicode } = core.ops;
  const { ArrayPrototypePush, SafeArrayIterator, StringFromCodePoint, StringPrototypeCharCodeAt } = primordials;
  /**
 * Creates an array containing the numeric code points of each Unicode
 * character in the string. While JavaScript uses UCS-2 internally,
 * this function will convert a pair of surrogate halves (each of which
 * UCS-2 exposes as separate characters) into a single code point,
 * matching UTF-16.
 *
 * @param str The Unicode input string (UCS-2).
 * @return The new array of code points.
 */ function ucs2decode(str) {
    const output = [];
    let counter = 0;
    const length = str.length;
    while(counter < length){
      const value = StringPrototypeCharCodeAt(str, counter++);
      if (value >= 0xD800 && value <= 0xDBFF && counter < length) {
        // It's a high surrogate, and there is a next character.
        const extra = StringPrototypeCharCodeAt(str, counter++);
        if ((extra & 0xFC00) == 0xDC00) {
          ArrayPrototypePush(output, ((value & 0x3FF) << 10) + (extra & 0x3FF) + 0x10000);
        } else {
          // It's an unmatched surrogate; only append this code unit, in case the
          // next code unit is the high surrogate of a surrogate pair.
          ArrayPrototypePush(output, value);
          counter--;
        }
      } else {
        ArrayPrototypePush(output, value);
      }
    }
    return output;
  }
  /**
 * Creates a string based on an array of numeric code points.
 * @see `punycode.ucs2.decode`
 * @memberOf punycode.ucs2
 * @name encode
 * @param codePoints The array of numeric code points.
 * @returns The new Unicode string (UCS-2).
 */ function ucs2encode(array) {
    return StringFromCodePoint(...new SafeArrayIterator(array));
  }
  const ucs2 = {
    decode: ucs2decode,
    encode: ucs2encode
  };
  /**
 *  Converts a domain to ASCII as per the IDNA spec (UTS #46 ToASCII).
 *  Returns an empty string if the domain is invalid.
 *
 *  This is Node's `internal/idna` `toASCII`, used by `node:dns` and `node:tls`.
 *  Prefer it over `domainToASCII` anywhere a hostname is on its way to a
 *  resolver: it does not truncate, percent-decode or normalize the host.
 */ function toASCII(domain) {
    return op_node_idna_to_ascii(domain);
  }
  /**
 *  Converts a domain to ASCII the way the WHATWG URL host parser does.
 *  Returns an empty string if the domain is invalid.
 *
 *  This is Node's `url.domainToASCII`, and is deliberately stricter than
 *  `toASCII`: it terminates the host at `/`, `\`, `?` or `#`, strips ASCII
 *  tab/newline, percent-decodes, normalizes IPv4/IPv6 literals and rejects
 *  forbidden host code points.
 */ function domainToASCII(domain) {
    return op_node_idna_domain_to_ascii(domain);
  }
  /**
 *  Converts a domain to Unicode as per the IDNA spec
 */ function domainToUnicode(domain) {
    return op_node_idna_domain_to_unicode(domain);
  }
  return {
    toASCII,
    domainToASCII,
    domainToUnicode,
    ucs2
  };
})());