"use strict"; return ((function() {
  const { core, primordials } = __bootstrap;
  const { ArrayBufferIsView, ArrayBufferPrototype, ArrayBufferPrototypeGetByteLength, ArrayPrototypeIncludes, Error, FunctionPrototypeApply, FunctionPrototypeCall, MathMin, ObjectPrototypeIsPrototypeOf, ObjectSetPrototypeOf, ReflectHas, StringFromCharCode, StringPrototypeIncludes, StringPrototypeToLowerCase, SymbolSpecies, TypeError, TypedArrayPrototypeGetBuffer, TypedArrayPrototypeGetByteLength, TypedArrayPrototypeGetByteOffset, Uint8Array, Uint8ArrayPrototype } = primordials;
  const { op_node_create_private_key, op_node_create_public_key, op_node_derive_public_key_from_private_key, op_node_get_asymmetric_key_details, op_node_get_asymmetric_key_type, op_node_sign, op_node_sign_ed25519, op_node_sign_ed448, op_node_verify, op_node_verify_ed25519, op_node_verify_ed448 } = core.ops;
  const { validateFunction, validateString } = core.loadExtScript("ext:deno_node/internal/validators.mjs");
  const { Buffer } = core.loadExtScript("ext:deno_node/internal/buffer.mjs");
  const lazyWritable = core.createLazyLoader("node:_stream_writable");
  const { kConsumePrivate, kConsumePublic, KeyObject, prepareAsymmetricKey, PrivateKeyObject, PublicKeyObject } = core.loadExtScript("ext:deno_node/internal/crypto/keys.ts");
  const { createHash } = core.loadExtScript("ext:deno_node/internal/crypto/hash.ts");
  const { ERR_CRYPTO_SIGN_KEY_REQUIRED, ERR_INVALID_ARG_TYPE, ERR_INVALID_ARG_VALUE } = core.loadExtScript("ext:deno_node/internal/errors.ts");
  const FastBuffer = Buffer[SymbolSpecies];
  function getPadding(options) {
    return getIntOption("padding", options);
  }
  function getSaltLength(options) {
    return getIntOption("saltLength", options);
  }
  function getDSASignatureEncoding(options) {
    if (typeof options === "object") {
      const { dsaEncoding = "der" } = options;
      if (dsaEncoding === "der") {
        return 0;
      } else if (dsaEncoding === "ieee-p1363") {
        return 1;
      }
      throw new ERR_INVALID_ARG_VALUE("options.dsaEncoding", dsaEncoding);
    }
    return 0;
  }
  function getIntOption(name, options) {
    const value = options[name];
    if (value !== undefined) {
      if (value === value >> 0) {
        return value;
      }
      throw new ERR_INVALID_ARG_VALUE(`options.${name}`, value);
    }
    return undefined;
  }
  // Private key types that need to be converted to public keys for verification
  const PRIVATE_KEY_TYPES = [
    "pkcs8",
    "sec1"
  ];
  function isPrivateKeyType(type) {
    return type !== undefined && ArrayPrototypeIncludes(PRIVATE_KEY_TYPES, type);
  }
  function isPrivateKeyPem(data) {
    const bytes = ObjectPrototypeIsPrototypeOf(ArrayBufferPrototype, data) ? new Uint8Array(data, 0, MathMin(ArrayBufferPrototypeGetByteLength(data), 100)) : new Uint8Array(TypedArrayPrototypeGetBuffer(data), TypedArrayPrototypeGetByteOffset(data), MathMin(TypedArrayPrototypeGetByteLength(data), 100));
    const prefix = FunctionPrototypeApply(StringFromCharCode, null, bytes);
    return StringPrototypeIncludes(prefix, "PRIVATE KEY");
  }
  let Writable;
  function getWritable() {
    if (!Writable) Writable = lazyWritable().default;
    return Writable;
  }
  class SignImpl {
    hash;
    #digestType;
    constructor(algorithm, _options){
      validateString(algorithm, "algorithm");
      ensureSignProtoSetup();
      const W = getWritable();
      FunctionPrototypeCall(W, this, {
        write (chunk, enc, callback) {
          this.update(chunk, enc);
          callback();
        }
      });
      algorithm = StringPrototypeToLowerCase(algorithm);
      this.#digestType = algorithm;
      try {
        this.hash = createHash(this.#digestType);
      } catch  {
        throw new Error(`Invalid digest: ${algorithm}`);
      }
    }
    sign(privateKey, encoding) {
      if (!privateKey) {
        throw new ERR_CRYPTO_SIGN_KEY_REQUIRED();
      }
      const res = prepareAsymmetricKey(privateKey, kConsumePrivate);
      // Options specific to RSA
      const rsaPadding = getPadding(privateKey);
      // Options specific to RSA-PSS
      const pssSaltLength = getSaltLength(privateKey);
      // Options specific to (EC)DSA
      const dsaSigEnc = getDSASignatureEncoding(privateKey);
      let handle;
      if (ReflectHas(res, "handle")) {
        handle = res.handle;
      } else {
        try {
          handle = op_node_create_private_key(res.data, res.format, res.type ?? "", res.passphrase);
        } catch (err) {
          // Trigger any prototype setter for `library` (Node.js compatibility)
          err.library = "PEM routines";
          throw err;
        }
      }
      let ret;
      try {
        ret = Buffer.from(op_node_sign(handle, this.hash.digest(), this.#digestType, pssSaltLength, rsaPadding, dsaSigEnc));
      } catch (err) {
        // Decorate RSA sign errors with OpenSSL-compatible properties.
        if (err && typeof err === "object" && ReflectHas(err, "message") && typeof err.message === "string" && StringPrototypeIncludes(err.message, "rsa routines") && !ReflectHas(err, "library")) {
          err.library = "rsa routines";
        }
        throw err;
      }
      // deno-lint-ignore deno-internal/prefer-primordials -- Buffer.prototype.toString(encoding) is not a primordial
      return encoding && encoding !== "buffer" ? ret.toString(encoding) : ret;
    }
    update(data, encoding) {
      this.hash.update(data, encoding);
      return this;
    }
  }
  function Sign(algorithm, options) {
    return new SignImpl(algorithm, options);
  }
  // Defer prototype setup
  let _signProtoSetup = false;
  function ensureSignProtoSetup() {
    if (_signProtoSetup) return;
    _signProtoSetup = true;
    const W = getWritable();
    ObjectSetPrototypeOf(SignImpl.prototype, W.prototype);
    ObjectSetPrototypeOf(SignImpl, W);
    ObjectSetPrototypeOf(VerifyImpl.prototype, W.prototype);
    ObjectSetPrototypeOf(VerifyImpl, W);
  }
  Sign.prototype = SignImpl.prototype;
  class VerifyImpl {
    hash;
    #digestType;
    constructor(algorithm, _options){
      validateString(algorithm, "algorithm");
      ensureSignProtoSetup();
      const W = getWritable();
      FunctionPrototypeCall(W, this, {
        write (chunk, enc, callback) {
          this.update(chunk, enc);
          callback();
        }
      });
      algorithm = StringPrototypeToLowerCase(algorithm);
      this.#digestType = algorithm;
      try {
        this.hash = createHash(this.#digestType);
      } catch  {
        throw new Error(`Invalid digest: ${algorithm}`);
      }
    }
    update(data, encoding) {
      this.hash.update(data, encoding);
      return this;
    }
    verify(publicKey, signature, encoding) {
      if (typeof signature !== "string" && !ArrayBufferIsView(signature)) {
        throw new ERR_INVALID_ARG_TYPE("signature", [
          "Buffer",
          "TypedArray",
          "DataView"
        ], signature);
      }
      const res = prepareAsymmetricKey(publicKey, kConsumePublic);
      // Options specific to RSA
      const rsaPadding = getPadding(publicKey);
      // Options specific to RSA-PSS
      const pssSaltLength = getSaltLength(publicKey);
      // Options specific to (EC)DSA
      const dsaSigEnc = getDSASignatureEncoding(publicKey);
      let handle;
      if (ReflectHas(res, "handle")) {
        handle = res.handle;
      } else if (isPrivateKeyType(res.type) || res.type === undefined && isPrivateKeyPem(res.data)) {
        const privateHandle = op_node_create_private_key(res.data, res.format, res.type ?? "", res.passphrase);
        handle = op_node_derive_public_key_from_private_key(privateHandle);
      } else {
        handle = op_node_create_public_key(res.data, res.format, res.type ?? "", res.passphrase);
      }
      return op_node_verify(handle, this.hash.digest(), this.#digestType, Buffer.from(signature, encoding), pssSaltLength, rsaPadding, dsaSigEnc);
    }
  }
  function Verify(algorithm, options) {
    return new VerifyImpl(algorithm, options);
  }
  Verify.prototype = VerifyImpl.prototype;
  function signOneShot(algorithm, data, key, callback) {
    if (algorithm != null) {
      validateString(algorithm, "algorithm");
    }
    if (callback !== undefined) {
      validateFunction(callback, "callback");
    }
    if (!ArrayBufferIsView(data) && typeof data !== "string") {
      throw new ERR_INVALID_ARG_TYPE("data", [
        "Buffer",
        "TypedArray",
        "DataView"
      ], data);
    }
    if (!key) {
      throw new ERR_CRYPTO_SIGN_KEY_REQUIRED();
    }
    // Validate dsaEncoding early so it takes precedence over key errors
    if (typeof key === "object" && key !== null && !ObjectPrototypeIsPrototypeOf(KeyObject.prototype, key)) {
      getDSASignatureEncoding(key);
    }
    // Normalize ArrayBufferView data to Uint8Array for Rust ops
    const dataBytes = ArrayBufferIsView(data) && !ObjectPrototypeIsPrototypeOf(Uint8ArrayPrototype, data) ? new Uint8Array(TypedArrayPrototypeGetBuffer(data), TypedArrayPrototypeGetByteOffset(data), TypedArrayPrototypeGetByteLength(data)) : data;
    try {
      const res = prepareAsymmetricKey(key, kConsumePrivate);
      let handle;
      if (ReflectHas(res, "handle")) {
        handle = res.handle;
      } else {
        handle = op_node_create_private_key(res.data, res.format, res.type ?? "", res.passphrase);
      }
      let result;
      const keyType = op_node_get_asymmetric_key_type(handle);
      if (keyType === "ed25519") {
        if (algorithm != null && algorithm !== "sha512") {
          throw new TypeError("Only 'sha512' is supported for Ed25519 keys");
        }
        result = new FastBuffer(64);
        op_node_sign_ed25519(handle, dataBytes, result);
      } else if (keyType === "ed448") {
        const keyOpts = typeof key === "object" && key !== null && !ObjectPrototypeIsPrototypeOf(KeyObject.prototype, key) ? key : null;
        const ctx = keyOpts?.context;
        if (ObjectPrototypeIsPrototypeOf(Uint8ArrayPrototype, ctx) && ctx.length > 0) {
          throw new TypeError("Context parameter is unsupported");
        }
        result = new FastBuffer(114);
        op_node_sign_ed448(handle, dataBytes, result);
      } else {
        let digest = algorithm;
        if (digest == null) {
          if (keyType === "rsa-pss") {
            const details = op_node_get_asymmetric_key_details(handle);
            if (details.hashAlgorithm) {
              digest = details.hashAlgorithm;
            }
          }
          if (digest == null) {
            throw new TypeError("Algorithm must be specified when using non-Ed25519 keys");
          }
        }
        // Preserve padding/saltLength options from the original key
        const privateKeyObject = new PrivateKeyObject(handle);
        const signKey = typeof key === "object" && !ObjectPrototypeIsPrototypeOf(KeyObject.prototype, key) ? {
          ...key,
          key: privateKeyObject
        } : privateKeyObject;
        result = Sign(digest).update(dataBytes).sign(signKey);
      }
      if (callback) {
        setTimeout(()=>callback(null, result));
      } else {
        return result;
      }
    } catch (err) {
      if (callback) {
        setTimeout(()=>callback(err, Buffer.alloc(0)));
      } else {
        throw err;
      }
    }
  }
  function verifyOneShot(algorithm, data, key, signature, callback) {
    if (algorithm != null) {
      validateString(algorithm, "algorithm");
    }
    if (callback !== undefined) {
      validateFunction(callback, "callback");
    }
    if (!ArrayBufferIsView(data) && typeof data !== "string") {
      throw new ERR_INVALID_ARG_TYPE("data", [
        "Buffer",
        "TypedArray",
        "DataView"
      ], data);
    }
    if (!ArrayBufferIsView(signature) && typeof signature !== "string") {
      throw new ERR_INVALID_ARG_TYPE("signature", [
        "Buffer",
        "TypedArray",
        "DataView"
      ], signature);
    }
    if (!key) {
      throw new ERR_CRYPTO_SIGN_KEY_REQUIRED();
    }
    // Normalize ArrayBufferView data to Uint8Array for Rust ops
    const dataBytes = ArrayBufferIsView(data) && !ObjectPrototypeIsPrototypeOf(Uint8ArrayPrototype, data) ? new Uint8Array(TypedArrayPrototypeGetBuffer(data), TypedArrayPrototypeGetByteOffset(data), TypedArrayPrototypeGetByteLength(data)) : data;
    try {
      const res = prepareAsymmetricKey(key, kConsumePublic);
      let handle;
      if (ReflectHas(res, "handle")) {
        handle = res.handle;
      } else if (isPrivateKeyType(res.type) || res.type === undefined && isPrivateKeyPem(res.data)) {
        const privateHandle = op_node_create_private_key(res.data, res.format, res.type ?? "", res.passphrase);
        handle = op_node_derive_public_key_from_private_key(privateHandle);
      } else {
        handle = op_node_create_public_key(res.data, res.format, res.type ?? "", res.passphrase);
      }
      let result;
      const keyType = op_node_get_asymmetric_key_type(handle);
      if (keyType === "ed25519") {
        if (algorithm != null && algorithm !== "sha512") {
          throw new TypeError("Only 'sha512' is supported for Ed25519 keys");
        }
        result = op_node_verify_ed25519(handle, dataBytes, signature);
      } else if (keyType === "ed448") {
        const keyOpts = typeof key === "object" && key !== null && !ObjectPrototypeIsPrototypeOf(KeyObject.prototype, key) ? key : null;
        const ctx = keyOpts?.context;
        if (ObjectPrototypeIsPrototypeOf(Uint8ArrayPrototype, ctx) && ctx.length > 0) {
          throw new TypeError("Context parameter is unsupported");
        }
        result = op_node_verify_ed448(handle, dataBytes, signature);
      } else if (keyType === "x25519" || keyType === "x448" || keyType === "dh") {
        throw new TypeError("operation not supported for this keytype");
      } else {
        let digest = algorithm;
        if (digest == null) {
          if (keyType === "rsa-pss") {
            const details = op_node_get_asymmetric_key_details(handle);
            if (details.hashAlgorithm) {
              digest = details.hashAlgorithm;
            }
          }
          if (digest == null) {
            throw new TypeError("no default digest");
          }
        }
        // Preserve padding/saltLength options from the original key
        const publicKeyObject = new PublicKeyObject(handle);
        const verifyKey = typeof key === "object" && !ObjectPrototypeIsPrototypeOf(KeyObject.prototype, key) ? {
          ...key,
          key: publicKeyObject
        } : publicKeyObject;
        result = Verify(digest).update(dataBytes).verify(verifyKey, signature);
      }
      if (callback) {
        setTimeout(()=>callback(null, result));
      } else {
        return result;
      }
    } catch (err) {
      if (callback) {
        setTimeout(()=>callback(err, false));
      } else {
        throw err;
      }
    }
  }
  return {
    signOneShot,
    verifyOneShot,
    Sign,
    Verify,
    SignImpl,
    VerifyImpl,
    default: {
      signOneShot,
      verifyOneShot,
      Sign,
      Verify
    }
  };
})());