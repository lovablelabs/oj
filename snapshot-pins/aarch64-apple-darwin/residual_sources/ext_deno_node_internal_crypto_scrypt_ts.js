"use strict"; return ((function() {
  const { core, primordials } = __bootstrap;
  const { BigInt, MathLog2, PromisePrototypeCatch, PromisePrototypeThen, TypedArrayPrototypeGetBuffer } = primordials;
  const { Buffer } = core.loadExtScript("ext:deno_node/internal/buffer.mjs");
  const { op_node_scrypt_async, op_node_scrypt_sync } = core.ops;
  const { validateFunction, validateInt32, validateInteger, validateUint32 } = core.loadExtScript("ext:deno_node/internal/validators.mjs");
  const { ERR_CRYPTO_INVALID_SCRYPT_PARAMS, ERR_INCOMPATIBLE_OPTION_PAIR } = core.loadExtScript("ext:deno_node/internal/errors.ts");
  const { getArrayBufferOrView } = core.loadExtScript("ext:deno_node/internal/crypto/keys.ts");
  function scryptSync(password, salt, keylen, _opts) {
    const options = check(password, salt, keylen, _opts);
    const { N, r, p, maxmem } = options;
    validateScryptParams(N, r, p, maxmem);
    if (keylen === 0) {
      return Buffer.alloc(0);
    }
    const buf = Buffer.alloc(keylen);
    op_node_scrypt_sync(password, salt, keylen, MathLog2(N), r, p, maxmem, TypedArrayPrototypeGetBuffer(buf));
    return buf;
  }
  function scrypt(password, salt, keylen, _opts, cb) {
    if (!cb) {
      cb = _opts;
      _opts = null;
    }
    const options = check(password, salt, keylen, _opts);
    const { N, r, p, maxmem } = options;
    validateFunction(cb, "callback");
    validateScryptParams(N, r, p, maxmem);
    if (keylen === 0) {
      cb(null, Buffer.alloc(0));
      return;
    }
    PromisePrototypeCatch(PromisePrototypeThen(op_node_scrypt_async(password, salt, keylen, MathLog2(N), r, p, maxmem), (buf)=>{
      cb(null, Buffer.from(TypedArrayPrototypeGetBuffer(buf)));
    }), (err)=>cb(err));
  }
  const defaults = {
    N: 16384,
    r: 8,
    p: 1,
    maxmem: 32 << 20
  };
  function check(password, salt, keylen, options) {
    password = getArrayBufferOrView(password, "password");
    salt = getArrayBufferOrView(salt, "salt");
    validateInt32(keylen, "keylen", 0);
    let { N, r, p, maxmem } = defaults;
    if (options && options !== defaults) {
      const hasN = options.N !== undefined;
      if (hasN) {
        N = options.N;
        validateUint32(N, "N");
      }
      if (options.cost !== undefined) {
        if (hasN) throw new ERR_INCOMPATIBLE_OPTION_PAIR("N", "cost");
        N = options.cost;
        validateUint32(N, "cost");
      }
      const hasR = options.r !== undefined;
      if (hasR) {
        r = options.r;
        validateUint32(r, "r");
      }
      if (options.blockSize !== undefined) {
        if (hasR) throw new ERR_INCOMPATIBLE_OPTION_PAIR("r", "blockSize");
        r = options.blockSize;
        validateUint32(r, "blockSize");
      }
      const hasP = options.p !== undefined;
      if (hasP) {
        p = options.p;
        validateUint32(p, "p");
      }
      if (options.parallelization !== undefined) {
        if (hasP) {
          throw new ERR_INCOMPATIBLE_OPTION_PAIR("p", "parallelization");
        }
        p = options.parallelization;
        validateUint32(p, "parallelization");
      }
      if (options.maxmem !== undefined) {
        maxmem = options.maxmem;
        validateInteger(maxmem, "maxmem", 0);
      }
      if (N === 0) N = defaults.N;
      if (r === 0) r = defaults.r;
      if (p === 0) p = defaults.p;
      if (maxmem === 0) maxmem = defaults.maxmem;
    }
    return {
      password,
      salt,
      keylen,
      N,
      r,
      p,
      maxmem
    };
  }
  function validateScryptParams(N, r, p, maxmem) {
    if (N < 2 || (N & N - 1) !== 0) {
      throw new ERR_CRYPTO_INVALID_SCRYPT_PARAMS();
    }
    const NBig = BigInt(N);
    const rBig = BigInt(r);
    const pBig = BigInt(p);
    const maxmemBig = BigInt(maxmem);
    const rTimes16 = rBig * 16n;
    if (rTimes16 <= 32n && NBig >= 1n << rTimes16 || pBig * rBig > (1n << 30n) - 1n || 128n * NBig * rBig >= maxmemBig) {
      throw new ERR_CRYPTO_INVALID_SCRYPT_PARAMS("error:1C800066:Provider routines::MEMORY_LIMIT_EXCEEDED");
    }
  }
  return {
    scrypt,
    scryptSync,
    default: {
      scrypt,
      scryptSync
    }
  };
})());