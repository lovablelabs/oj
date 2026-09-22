"use strict"; return ((function() {
  const { core, primordials } = __bootstrap;
  const { fs: fsConstants } = core.loadExtScript("ext:deno_node/internal_binding/constants.ts");
  const { codeMap } = core.loadExtScript("ext:deno_node/internal_binding/uv.ts");
  const { getValidatedEncoding, isFd, isFileOptions, makeCallback, maybeCallback } = core.loadExtScript("ext:deno_node/_fs/_fs_common.ts");
  const { AbortError, denoErrorToNodeError, denoWriteFileErrorToNodeError, ERR_FS_FILE_TOO_LARGE } = core.loadExtScript("ext:deno_node/internal/errors.ts");
  const constants = core.loadExtScript("ext:deno_node/_fs/_fs_constants.ts");
  const { CFISBIS, convertFileInfoToBigIntStats, convertFileInfoToStats } = core.createLazyLoader("ext:deno_node/internal/fs/stat_utils.ts")();
  const { copyFile, copyFileSync } = core.createLazyLoader("ext:deno_node/_fs/_fs_copy.ts")();
  const { cp, cpSync } = core.loadExtScript("ext:deno_node/_fs/_fs_cp.ts");
  const { default: Dir } = core.createLazyLoader("ext:deno_node/_fs/_fs_dir.ts")();
  const { exists, existsSync } = core.createLazyLoader("ext:deno_node/_fs/_fs_exists.ts")();
  const { fstat, fstatSync } = core.loadExtScript("ext:deno_node/_fs/_fs_fstat.ts");
  const { lstat, lstatSync } = core.loadExtScript("ext:deno_node/_fs/_fs_lstat.ts");
  const { lutimes, lutimesSync } = core.createLazyLoader("ext:deno_node/_fs/_fs_lutimes.ts")();
  const { read, readSync } = core.createLazyLoader("ext:deno_node/_fs/_fs_read.ts")();
  const { readdir, readdirSync } = core.createLazyLoader("ext:deno_node/_fs/_fs_readdir.ts")();
  const { EventEmitter } = core.loadExtScript("ext:deno_node/_events.mjs");
  const lazyTimers = core.createLazyLoader("node:timers");
  const { clearTimeout, setTimeout } = lazyTimers();
  const { notImplemented } = core.loadExtScript("ext:deno_node/_utils.ts");
  const { deprecate, promisify } = core.loadExtScript("ext:deno_node/util.ts");
  // internal/fs/{promises,streams,handle}.ts call `lazyFs()` at top-level to
  // build promisified wrappers around members of `node:fs`. Loading them
  // eagerly from inside fs.ts would re-enter the partially-loaded `node:fs`
  // namespace and hit a TDZ error. Defer to first access of `fs.promises` etc.
  const lazyInternalPromises = core.createLazyLoader("ext:deno_node/internal/fs/promises.ts");
  const lazyInternalStreams = core.createLazyLoader("ext:deno_node/internal/fs/streams.mjs");
  const lazyInternalHandle = core.createLazyLoader("ext:deno_node/internal/fs/handle.ts");
  // Backing storage so the lazy getters below can be paired with setters;
  // some packages monkey-patch these on the `node:fs` namespace.
  let _createReadStream;
  let _createWriteStream;
  let _ReadStream;
  let _WriteStream;
  let _promises;
  const { default: SyncWriteStream } = core.loadExtScript("ext:deno_node/internal/fs/sync_write_stream.js");
  // Utf8Stream is only re-exported, never used at module body. Keep this as
  // a thunk so loading fs.ts doesn't immediately pull fast-utf8-stream.js
  // (which statically imports node:fs and triggers the whole stream subtree).
  const lazyUtf8Stream = core.createLazyLoader("ext:deno_node/internal/streams/fast-utf8-stream.js");
  const { arrayBufferViewToUint8Array, BigIntStats, constants: fsUtilConstants, copyObject, Dirent, getOptions, getValidatedFd, getValidatedPath, getValidatedPathToString, getValidMode, kMaxUserId, Stats, stringToFlags, toUnixTimestamp, validateBufferArray, validateOffsetLengthWrite, validateRmdirOptions, validateRmOptions, validateRmOptionsSync, validateStringAfterArrayBufferView, warnOnNonPortableTemplate } = core.createLazyLoader("ext:deno_node/internal/fs/utils.mjs")();
  const { glob, globSync } = core.createLazyLoader("ext:deno_node/_fs/_fs_glob.ts")();
  const { Buffer } = core.loadExtScript("ext:deno_node/internal/buffer.mjs");
  const lazyProcess = core.createLazyLoader("node:process");
  const { isIterable } = core.loadExtScript("ext:deno_node/internal/streams/utils.js");
  const { op_fs_read_file_async, op_fs_read_file_sync, op_node_fs_close, op_node_fs_fchmod, op_node_fs_fchmod_sync, op_node_fs_fchown, op_node_fs_fchown_sync, op_node_fs_fdatasync, op_node_fs_fdatasync_sync, op_node_fs_fstat_sync, op_node_fs_fsync, op_node_fs_fsync_sync, op_node_fs_ftruncate, op_node_fs_ftruncate_sync, op_node_fs_futimes, op_node_fs_futimes_sync, op_node_fs_read_deferred, op_node_fs_read_file_sync, op_node_fs_read_sync, op_node_fs_write_deferred, op_node_fs_write_sync, op_node_lchmod, op_node_lchmod_sync, op_node_lchown, op_node_lchown_sync, op_node_mkdtemp, op_node_mkdtemp_sync, op_node_open, op_node_open_sync, op_node_rmdir, op_node_rmdir_sync, op_node_statfs, op_node_statfs_sync } = core.ops;
  const { ERR_INVALID_ARG_TYPE, ERR_INVALID_ARG_VALUE, uvException } = core.loadExtScript("ext:deno_node/internal/errors.ts");
  const { isMacOS, isWindows } = core.loadExtScript("ext:deno_node/_util/os.ts");
  const { customPromisifyArgs, kEmptyObject, normalizeEncoding } = core.loadExtScript("ext:deno_node/internal/util.mjs");
  const lazyPath = core.createLazyLoader("node:path");
  const pathModule = lazyPath();
  const { basename, relative, resolve, toNamespacedPath } = pathModule;
  const { parseFileMode, validateAbortSignal, validateBoolean, validateEncoding, validateFunction, validateInt32, validateInteger, validateObject, validateOneOf, validateString } = core.loadExtScript("ext:deno_node/internal/validators.mjs");
  const { isArrayBufferView } = core.loadExtScript("ext:deno_node/internal/util/types.ts");
  const { Blob, markFileBackedBlob } = core.loadExtScript("ext:deno_web/09_file.js");
  // Re-exported under both names for tests.
  const _toUnixTimestamp = toUnixTimestamp;
  const { createFSReqCallback, unregisterActiveRequest } = core.loadExtScript("ext:deno_node/internal/process/active_resources.ts");
  const { ArrayBufferIsView, ArrayIsArray, BigInt, DateUTC, Error, FunctionPrototypeBind, ErrorPrototype, MapPrototypeDelete, MapPrototypeGet, MapPrototypeSet, MathMax, MathMin, MathTrunc, Number, NumberIsFinite, NumberIsNaN, ObjectDefineProperty, ObjectPrototypeIsPrototypeOf, Promise, PromisePrototypeThen, PromiseResolve, RegExpPrototype, RegExpPrototypeTest, SafeMap, StringPrototypeToString, SymbolAsyncIterator, SymbolDispose, SymbolFor, ArrayPrototypePush, TypedArrayPrototypeGetByteLength, TypedArrayPrototypeSet, TypedArrayPrototypeSubarray, Uint8Array, queueMicrotask } = primordials;
  const { TextEncoder } = core.loadExtScript("ext:deno_web/08_text_encoding.js");
  const abortSignal = core.loadExtScript("ext:deno_web/03_abort_signal.js");
  const { pathFromURL } = core.loadExtScript("ext:deno_web/00_infra.js");
  const { URLPrototype } = core.loadExtScript("ext:deno_web/00_url.js");
  const { kIoMaxLength, kReadFileUnknownBufferLength } = fsUtilConstants;
  const defaultStatOptions = {
    __proto__: null,
    bigint: false
  };
  const defaultStatSyncOptions = {
    __proto__: null,
    bigint: false,
    throwIfNoEntry: true
  };
  function stat(path, options = defaultStatOptions, callback) {
    if (typeof options === "function") {
      callback = options;
      options = defaultStatOptions;
    }
    callback = makeCallback(callback);
    path = getValidatedPathToString(path);
    PromisePrototypeThen(Deno.stat(path), (stat)=>callback(null, CFISBIS(stat, options.bigint)), (err)=>{
      // Match Node: `{ throwIfNoEntry: false }` suppresses ENOENT and yields
      // undefined stats, matching the behavior of binding.stat(..., false)
      // in lib/fs.js stat().
      if (options?.throwIfNoEntry === false && ObjectPrototypeIsPrototypeOf(Deno.errors.NotFound.prototype, err)) {
        callback(null, undefined);
        return;
      }
      callback(denoErrorToNodeError(err, {
        syscall: "stat",
        path
      }));
    });
  }
  function statSync(path, options = defaultStatSyncOptions) {
    path = getValidatedPathToString(path);
    try {
      const origin = Deno.statSync(path);
      return CFISBIS(origin, options.bigint);
    } catch (err) {
      if (options?.throwIfNoEntry === false && ObjectPrototypeIsPrototypeOf(Deno.errors.NotFound.prototype, err)) {
        return;
      }
      if (ObjectPrototypeIsPrototypeOf(ErrorPrototype, err)) {
        throw denoErrorToNodeError(err, {
          syscall: "stat",
          path
        });
      } else {
        throw err;
      }
    }
  }
  function encodeRealpathResult(result, options) {
    if (!options || !options.encoding || options.encoding === "utf8") {
      return result;
    }
    const asBuffer = Buffer.from(result);
    if (options.encoding === "buffer") {
      return asBuffer;
    }
    // deno-lint-ignore deno-internal/prefer-primordials
    return asBuffer.toString(options.encoding);
  }
  function realpathImpl(path, options, callback, syscall) {
    if (typeof options === "function") {
      callback = options;
    }
    validateFunction(callback, "cb");
    options = getOptions(options);
    const validatedPath = getValidatedPathToString(path);
    PromisePrototypeThen(Deno.realPath(validatedPath), (resolved)=>callback(null, encodeRealpathResult(resolved, options)), (err)=>callback(denoErrorToNodeError(err, {
        syscall,
        path: validatedPath
      })));
  }
  function realpath(path, options, callback) {
    // Match Node: `fs.realpath` uses lstat under the hood and emits
    // `syscall: 'lstat'`; `fs.realpath.native` calls the platform realpath
    // and emits `syscall: 'realpath'` (see lib/fs.js).
    realpathImpl(path, options, callback, "lstat");
  }
  realpath.native = function realpath_native(path, options, callback) {
    realpathImpl(path, options, callback, "realpath");
  };
  function realpathSyncImpl(path, options, syscall) {
    options = getOptions(options);
    const validatedPath = getValidatedPathToString(path);
    try {
      const result = Deno.realPathSync(validatedPath);
      return encodeRealpathResult(result, options);
    } catch (err) {
      throw denoErrorToNodeError(err, {
        syscall,
        path: validatedPath
      });
    }
  }
  function realpathSync(path, options) {
    return realpathSyncImpl(path, options, "lstat");
  }
  realpathSync.native = function realpathSync_native(path, options) {
    return realpathSyncImpl(path, options, "realpath");
  };
  function prepareReadvBuffers(buffers, allowDirect) {
    const views = [];
    const lengths = [];
    let length = 0;
    for(let i = 0; i < buffers.length; i++){
      const view = arrayBufferViewToUint8Array(buffers[i]);
      const viewLength = TypedArrayPrototypeGetByteLength(view);
      ArrayPrototypePush(views, view);
      ArrayPrototypePush(lengths, viewLength);
      length += viewLength;
    }
    if (allowDirect && views.length === 1) {
      return {
        buffer: views[0],
        lengths,
        needsScatter: false,
        views
      };
    }
    // A staging buffer preserves one-read semantics while safely supporting
    // duplicate and overlapping views.
    return {
      buffer: new Uint8Array(length),
      lengths,
      needsScatter: true,
      views
    };
  }
  function scatterReadvBuffer(source, views, lengths, bytesRead) {
    let offset = 0;
    for(let i = 0; i < views.length && offset < bytesRead; i++){
      const view = views[i];
      const length = MathMin(lengths[i], TypedArrayPrototypeGetByteLength(view), bytesRead - offset);
      if (length > 0) {
        TypedArrayPrototypeSet(view, TypedArrayPrototypeSubarray(source, offset, offset + length));
      }
      offset += lengths[i];
    }
  }
  function readv(fd, buffers, position, callback) {
    if (typeof fd !== "number") {
      throw new ERR_INVALID_ARG_TYPE("fd", "number", fd);
    }
    fd = getValidatedFd(fd);
    validateBufferArray(buffers);
    const cb = maybeCallback(callback || position);
    let pos = null;
    if (typeof position === "number") {
      validateInteger(position, "position", 0);
      pos = position;
    }
    if (buffers.length === 0) {
      lazyProcess().default.nextTick(cb, null, 0, buffers);
      return;
    }
    const { buffer, lengths, views } = prepareReadvBuffers(buffers, false);
    PromisePrototypeThen(op_node_fs_read_deferred(fd, buffer, pos === null ? -1n : BigInt(pos)), (numRead)=>{
      scatterReadvBuffer(buffer, views, lengths, numRead);
      cb(null, numRead, buffers);
    }, (err)=>{
      cb(denoErrorToNodeError(err, {
        syscall: "read"
      }), 0, buffers);
    });
  }
  ObjectDefineProperty(readv, customPromisifyArgs, {
    __proto__: null,
    value: [
      "bytesRead",
      "buffers"
    ],
    enumerable: false
  });
  function readvSync(fd, buffers, position = null) {
    if (typeof fd !== "number") {
      throw new ERR_INVALID_ARG_TYPE("fd", "number", fd);
    }
    fd = getValidatedFd(fd);
    validateBufferArray(buffers);
    if (buffers.length === 0) {
      return 0;
    }
    if (typeof position === "number") {
      validateInteger(position, "position", 0);
    }
    const { buffer, lengths, needsScatter, views } = prepareReadvBuffers(buffers, true);
    try {
      const numRead = op_node_fs_read_sync(fd, buffer, typeof position === "number" ? BigInt(position) : -1n);
      if (needsScatter) {
        scatterReadvBuffer(buffer, views, lengths, numRead);
      }
      return numRead;
    } catch (err) {
      throw denoErrorToNodeError(err, {
        syscall: "read"
      });
    }
  }
  function readvPromise(fd, buffers, position) {
    return new Promise((resolve, reject)=>{
      readv(fd, buffers, position ?? null, (err, bytesRead, buffers)=>{
        if (err) reject(err);
        else resolve({
          bytesRead,
          buffers
        });
      });
    });
  }
  // -- readFile --
  const readFileDefaultOptions = {
    __proto__: null,
    flag: "r"
  };
  function readFileMaybeDecode(data, encoding) {
    // deno-lint-ignore deno-internal/prefer-primordials
    const buffer = Buffer.from(data.buffer, data.byteOffset, data.byteLength);
    // deno-lint-ignore deno-internal/prefer-primordials
    if (encoding) return buffer.toString(encoding);
    return buffer;
  }
  async function readFileAsync(path, options) {
    let cancelRid;
    let abortHandler;
    const flagsNumber = stringToFlags(options.flag, "options.flag");
    if (options?.signal) {
      options.signal.throwIfAborted();
      cancelRid = core.createCancelHandle();
      abortHandler = ()=>core.tryClose(cancelRid);
      options.signal[abortSignal.add](abortHandler);
    }
    try {
      const data = await op_fs_read_file_async(path, cancelRid, flagsNumber);
      return data;
    } finally{
      if (options?.signal) {
        options.signal[abortSignal.remove](abortHandler);
        // always throw the abort error when aborted
        options.signal.throwIfAborted();
      }
    }
  }
  function readFileCheckAborted(signal) {
    if (signal?.aborted) {
      throw new AbortError(undefined, {
        cause: signal.reason
      });
    }
  }
  function readFileConcatBuffers(buffers) {
    let totalLen = 0;
    for(let i = 0; i < buffers.length; ++i){
      totalLen += TypedArrayPrototypeGetByteLength(buffers[i]);
    }
    const contents = new Uint8Array(totalLen);
    let n = 0;
    for(let i = 0; i < buffers.length; ++i){
      const buf = buffers[i];
      TypedArrayPrototypeSet(contents, buf, n);
      n += TypedArrayPrototypeGetByteLength(buf);
    }
    return contents;
  }
  async function readFileFromFd(fd, options) {
    const signal = options?.signal;
    readFileCheckAborted(signal);
    const statFields = op_node_fs_fstat_sync(fd);
    readFileCheckAborted(signal);
    const isFile = statFields.isFile;
    const size = isFile ? statFields.size : 0;
    if (size > kIoMaxLength) {
      throw new ERR_FS_FILE_TOO_LARGE(size);
    }
    if (isFile && size > 0) {
      // Known size: read into a single buffer with an advancing offset.
      // Mirrors Node's readFileHandle which avoids the subarray-aliasing trap
      // by writing successive reads into different regions of one buffer.
      const buffer = new Uint8Array(size);
      let totalRead = 0;
      while(totalRead < size){
        readFileCheckAborted(signal);
        const slice = TypedArrayPrototypeSubarray(buffer, totalRead);
        // Use the deferred op so we yield to the event loop between reads,
        // allowing abort signals scheduled via lazyProcess().default.nextTick to fire.
        const nread = await op_node_fs_read_deferred(fd, slice, -1n);
        if (nread === 0) break;
        totalRead += nread;
      }
      readFileCheckAborted(signal);
      return totalRead === size ? buffer : TypedArrayPrototypeSubarray(buffer, 0, totalRead);
    }
    // Unknown size (pipes, sockets, /dev/stdin): allocate a fresh buffer per
    // iteration so pushed subarrays don't alias a reused read buffer.
    const buffers = [];
    let totalRead = 0;
    while(true){
      readFileCheckAborted(signal);
      const chunk = new Uint8Array(kReadFileUnknownBufferLength);
      const nread = await op_node_fs_read_deferred(fd, chunk, -1n);
      if (nread === 0) break;
      totalRead += nread;
      if (totalRead > kIoMaxLength) {
        throw new ERR_FS_FILE_TOO_LARGE(totalRead);
      }
      ArrayPrototypePush(buffers, TypedArrayPrototypeSubarray(chunk, 0, nread));
    }
    return readFileConcatBuffers(buffers);
  }
  function readFile(pathOrRid, optOrCallback, callback) {
    if (ObjectPrototypeIsPrototypeOf(lazyInternalHandle().FileHandle.prototype, pathOrRid)) {
      pathOrRid = pathOrRid.fd;
    } else if (typeof pathOrRid !== "number") {
      pathOrRid = getValidatedPathToString(pathOrRid);
    }
    let cb;
    if (typeof optOrCallback === "function") {
      cb = optOrCallback;
    } else {
      cb = callback;
    }
    const options = getOptions(optOrCallback, readFileDefaultOptions);
    let p;
    if (typeof pathOrRid === "string") {
      p = readFileAsync(pathOrRid, options);
    } else {
      p = readFileFromFd(pathOrRid, options);
    }
    if (cb) {
      PromisePrototypeThen(p, (data)=>{
        const textOrBuffer = readFileMaybeDecode(data, options?.encoding);
        cb(null, textOrBuffer);
      }, (err)=>cb && cb(denoErrorToNodeError(err, {
          path: typeof pathOrRid === "string" ? pathOrRid : undefined,
          syscall: "open"
        })));
    }
  }
  function readFilePromise(path, options) {
    return new Promise((resolve, reject)=>{
      readFile(path, options, (err, data)=>{
        if (err) reject(err);
        else resolve(data);
      });
    });
  }
  function readFileSync(path, opt) {
    const options = getOptions(opt, readFileDefaultOptions);
    let data;
    if (typeof path === "number") {
      data = op_node_fs_read_file_sync(path);
    } else {
      // Validate/convert path to string (throws on invalid types)
      path = getValidatedPathToString(path);
      const flagsNumber = stringToFlags(options?.flag, "options.flag");
      try {
        data = op_fs_read_file_sync(path, flagsNumber);
      } catch (err) {
        throw denoErrorToNodeError(err, {
          path,
          syscall: "open"
        });
      }
    }
    const textOrBuffer = readFileMaybeDecode(data, options?.encoding);
    return textOrBuffer;
  }
  function readlinkMaybeEncode(data, encoding) {
    if (encoding === "buffer") {
      return new TextEncoder().encode(data);
    }
    return data;
  }
  function readlinkGetEncoding(optOrCallback) {
    if (!optOrCallback || typeof optOrCallback === "function") {
      return null;
    } else {
      if (optOrCallback.encoding) {
        if (optOrCallback.encoding === "utf8" || optOrCallback.encoding === "utf-8") {
          return "utf8";
        } else if (optOrCallback.encoding === "buffer") {
          return "buffer";
        } else {
          notImplemented(`fs.readlink encoding=${optOrCallback.encoding}`);
        }
      }
      return null;
    }
  }
  function readlink(path, optOrCallback, callback) {
    path = getValidatedPathToString(path);
    let cb;
    if (typeof optOrCallback === "function") {
      cb = optOrCallback;
    } else {
      cb = callback;
    }
    cb = makeCallback(cb);
    const encoding = readlinkGetEncoding(optOrCallback);
    PromisePrototypeThen(Deno.readLink(path), (data)=>{
      const res = readlinkMaybeEncode(data, encoding);
      if (cb) cb(null, res);
    }, (err)=>{
      if (cb) {
        cb(denoErrorToNodeError(err, {
          syscall: "readlink",
          path
        }));
      }
    });
  }
  const readlinkPromise = promisify(readlink);
  function readlinkSync(path, opt) {
    path = getValidatedPathToString(path);
    try {
      return readlinkMaybeEncode(Deno.readLinkSync(path), readlinkGetEncoding(opt));
    } catch (error) {
      throw denoErrorToNodeError(error, {
        syscall: "readlink",
        path
      });
    }
  }
  class StatFs {
    type;
    bsize;
    frsize;
    blocks;
    bfree;
    bavail;
    files;
    ffree;
    constructor(type, bsize, frsize, blocks, bfree, bavail, files, ffree){
      this.type = type;
      this.bsize = bsize;
      this.frsize = frsize;
      this.blocks = blocks;
      this.bfree = bfree;
      this.bavail = bavail;
      this.files = files;
      this.ffree = ffree;
    }
  }
  function opResultToStatFs(result, bigint) {
    if (!bigint) {
      return new StatFs(result.type, result.bsize, // Deno's statfs op does not expose a separate fragment size; it equals
      // the block size on the platforms we support (matches Node in practice).
      result.bsize, result.blocks, result.bfree, result.bavail, result.files, result.ffree);
    }
    return new StatFs(BigInt(result.type), BigInt(result.bsize), BigInt(result.bsize), BigInt(result.blocks), BigInt(result.bfree), BigInt(result.bavail), BigInt(result.files), BigInt(result.ffree));
  }
  function statfs(path, options, callback) {
    if (typeof options === "function") {
      callback = options;
      options = undefined;
    }
    // @ts-expect-error callback type is known to be valid
    callback = makeCallback(callback);
    path = getValidatedPathToString(path);
    const bigint = typeof options?.bigint === "boolean" ? options.bigint : false;
    PromisePrototypeThen(op_node_statfs(path, bigint), (statFs)=>{
      callback(null, opResultToStatFs(statFs, bigint));
    }, (err)=>callback(denoErrorToNodeError(err, {
        syscall: "statfs",
        path
      })));
  }
  function statfsSync(path, options) {
    path = getValidatedPathToString(path);
    const bigint = typeof options?.bigint === "boolean" ? options.bigint : false;
    try {
      const result = op_node_statfs_sync(path, bigint);
      return opResultToStatFs(result, bigint);
    } catch (err) {
      throw denoErrorToNodeError(err, {
        syscall: "statfs",
        path
      });
    }
  }
  function access(path, mode, callback) {
    if (typeof mode === "function") {
      callback = mode;
      mode = fsConstants.F_OK;
    }
    // deno-lint-ignore deno-internal/prefer-primordials
    path = getValidatedPath(path).toString();
    mode = getValidMode(mode, "access");
    const cb = makeCallback(callback);
    // deno-lint-ignore deno-internal/prefer-primordials
    Deno.lstat(path).then((info)=>{
      if (info.mode === null) {
        cb(null);
        return;
      }
      let m = +mode || 0;
      let fileMode = +info.mode || 0;
      if (Deno.build.os === "windows") {
        m &= ~fsConstants.X_OK;
      } else if (info.uid === Deno.uid()) {
        fileMode >>= 6;
      }
      if ((m & fileMode) === m) {
        cb(null);
      } else {
        const e = new Error(`EACCES: permission denied, access '${path}'`);
        e.path = path;
        e.syscall = "access";
        e.errno = codeMap.get("EACCES");
        e.code = "EACCES";
        cb(e);
      }
    }, (err)=>{
      // deno-lint-ignore deno-internal/prefer-primordials
      if (err instanceof Deno.errors.NotFound) {
        const e = new Error(`ENOENT: no such file or directory, access '${path}'`);
        e.path = path;
        e.syscall = "access";
        e.errno = codeMap.get("ENOENT");
        e.code = "ENOENT";
        cb(e);
      } else {
        cb(err);
      }
    });
  }
  function accessSync(path, mode) {
    // deno-lint-ignore deno-internal/prefer-primordials
    path = getValidatedPath(path).toString();
    mode = getValidMode(mode, "access");
    try {
      // deno-lint-ignore deno-internal/prefer-primordials
      const info = Deno.lstatSync(path.toString());
      if (info.mode === null) {
        return;
      }
      let m = +mode || 0;
      let fileMode = +info.mode || 0;
      if (Deno.build.os === "windows") {
        m &= ~fsConstants.X_OK;
      } else if (info.uid === Deno.uid()) {
        fileMode >>= 6;
      }
      if ((m & fileMode) === m) {
      // all required flags exist
      } else {
        const e = new Error(`EACCES: permission denied, access '${path}'`);
        e.path = path;
        e.syscall = "access";
        e.errno = codeMap.get("EACCES");
        e.code = "EACCES";
        throw e;
      }
    } catch (err) {
      // deno-lint-ignore deno-internal/prefer-primordials
      if (err instanceof Deno.errors.NotFound) {
        const e = new Error(`ENOENT: no such file or directory, access '${path}'`);
        e.path = path;
        e.syscall = "access";
        e.errno = codeMap.get("ENOENT");
        e.code = "ENOENT";
        throw e;
      } else {
        throw err;
      }
    }
  }
  /**
 * TODO: Also accept 'data' parameter as a Node polyfill Buffer type once these
 * are implemented. See https://github.com/denoland/deno/issues/3403
 */ function appendFile(path, data, options, callback) {
    callback = maybeCallback(callback || options);
    options = getOptions(options, {
      encoding: "utf8",
      mode: 0o666,
      flag: "a"
    });
    // Don't make changes directly on options object
    options = copyObject(options);
    // Force append behavior when using a supplied file descriptor
    if (!options.flag || isFd(path)) {
      options.flag = "a";
    }
    writeFile(path, data, options, callback);
  }
  /**
 * TODO: Also accept 'data' parameter as a Node polyfill Buffer type once these
 * are implemented. See https://github.com/denoland/deno/issues/3403
 */ function appendFileSync(path, data, options) {
    options = getOptions(options, {
      encoding: "utf8",
      mode: 0o666,
      flag: "a"
    });
    // Don't make changes directly on options object
    options = copyObject(options);
    // Force append behavior when using a supplied file descriptor
    if (!options.flag || isFd(path)) {
      options.flag = "a";
    }
    writeFileSync(path, data, options);
  }
  function chmod(path, mode, callback) {
    path = getValidatedPathToString(path);
    mode = parseFileMode(mode, "mode");
    PromisePrototypeThen(Deno.chmod(path, mode), ()=>callback(null), (err)=>callback(denoErrorToNodeError(err, {
        syscall: "chmod",
        path
      })));
  }
  function chmodSync(path, mode) {
    path = getValidatedPathToString(path);
    mode = parseFileMode(mode, "mode");
    try {
      Deno.chmodSync(path, mode);
    } catch (error) {
      throw denoErrorToNodeError(error, {
        syscall: "chmod",
        path
      });
    }
  }
  function chown(path, uid, gid, callback) {
    callback = makeCallback(callback);
    // deno-lint-ignore deno-internal/prefer-primordials
    path = getValidatedPath(path).toString();
    validateInteger(uid, "uid", -1, kMaxUserId);
    validateInteger(gid, "gid", -1, kMaxUserId);
    // deno-lint-ignore deno-internal/prefer-primordials
    Deno.chown(path, uid, gid).then(()=>callback(null), callback);
  }
  function chownSync(path, uid, gid) {
    // deno-lint-ignore deno-internal/prefer-primordials
    path = getValidatedPath(path).toString();
    validateInteger(uid, "uid", -1, kMaxUserId);
    validateInteger(gid, "gid", -1, kMaxUserId);
    Deno.chownSync(path, uid, gid);
  }
  function defaultCloseCallback(err) {
    if (err !== null) throw err;
  }
  function close(fd, callback = defaultCloseCallback) {
    fd = getValidatedFd(fd);
    if (callback !== defaultCloseCallback) {
      callback = makeCallback(callback);
    }
    // Defer to a microtask rather than a JS `setTimeout(0)`. Both make the
    // callback asynchronous, but a real timer trips Deno's test sanitizer as a
    // leaked timeout when a test ends before the timer fires. Node.js' libuv
    // libc-backed `close` is invisible to userland timer queues; a microtask
    // matches that more closely.
    queueMicrotask(()=>{
      let error = null;
      try {
        op_node_fs_close(fd);
      } catch (err) {
        error = ObjectPrototypeIsPrototypeOf(ErrorPrototype, err) ? err : new Error("[non-error thrown]");
      }
      callback(error);
    });
  }
  function closeSync(fd) {
    fd = getValidatedFd(fd);
    op_node_fs_close(fd);
  }
  function fchown(fd, uid, gid, callback) {
    validateInteger(fd, "fd", 0, 2147483647);
    validateInteger(uid, "uid", -1, kMaxUserId);
    validateInteger(gid, "gid", -1, kMaxUserId);
    callback = makeCallback(callback);
    PromisePrototypeThen(op_node_fs_fchown(fd, uid, gid), ()=>callback(null), callback);
  }
  function fchmod(fd, mode, callback) {
    validateInteger(fd, "fd", 0, 2147483647);
    mode = parseFileMode(mode, "mode");
    callback = makeCallback(callback);
    PromisePrototypeThen(op_node_fs_fchmod(fd, mode), ()=>callback(null), callback);
  }
  function fchownSync(fd, uid, gid) {
    validateInteger(fd, "fd", 0, 2147483647);
    validateInteger(uid, "uid", -1, kMaxUserId);
    validateInteger(gid, "gid", -1, kMaxUserId);
    op_node_fs_fchown_sync(fd, uid, gid);
  }
  function ftruncate(fd, lenOrCallback = 0, maybeCallback) {
    let len = 0;
    let callback;
    if (typeof lenOrCallback === "function") {
      callback = lenOrCallback;
    } else {
      len = lenOrCallback;
      callback = maybeCallback;
    }
    // Match Node: validate fd and len before any async work (lib/fs.js).
    if (typeof fd !== "number") {
      throw new ERR_INVALID_ARG_TYPE("fd", "number", fd);
    }
    validateInteger(len, "len");
    len = MathMax(0, len);
    if (!callback) throw new Error("No callback function supplied");
    PromisePrototypeThen(op_node_fs_ftruncate(fd, len), ()=>callback(null), callback);
  }
  function ftruncateSync(fd, len = 0) {
    if (typeof fd !== "number") {
      throw new ERR_INVALID_ARG_TYPE("fd", "number", fd);
    }
    validateInteger(len, "len");
    op_node_fs_ftruncate_sync(fd, MathMax(0, len));
  }
  function _getValidTime(time, name) {
    if (typeof time === "string") {
      time = Number(time);
    }
    if (typeof time === "number" && (NumberIsNaN(time) || !NumberIsFinite(time))) {
      throw new Deno.errors.InvalidData(`invalid ${name}, must not be infinity or NaN`);
    }
    return toUnixTimestamp(time);
  }
  function futimes(fd, atime, mtime, callback) {
    if (!callback) {
      throw new Deno.errors.InvalidData("No callback function supplied");
    }
    if (typeof fd !== "number") {
      throw new ERR_INVALID_ARG_TYPE("fd", "number", fd);
    }
    validateInteger(fd, "fd", 0, 2147483647);
    atime = _getValidTime(atime, "atime");
    mtime = _getValidTime(mtime, "mtime");
    const atimeSecs = MathTrunc(atime);
    const atimeNanos = MathTrunc((atime - atimeSecs) * 1e9);
    const mtimeSecs = MathTrunc(mtime);
    const mtimeNanos = MathTrunc((mtime - mtimeSecs) * 1e9);
    PromisePrototypeThen(op_node_fs_futimes(fd, atimeSecs, atimeNanos, mtimeSecs, mtimeNanos), ()=>callback(null), callback);
  }
  function fchmodSync(fd, mode) {
    validateInteger(fd, "fd", 0, 2147483647);
    op_node_fs_fchmod_sync(fd, parseFileMode(mode, "mode"));
  }
  function fdatasync(fd, callback) {
    validateInt32(fd, "fd", 0);
    PromisePrototypeThen(op_node_fs_fdatasync(fd), ()=>callback(null), callback);
  }
  function futimesSync(fd, atime, mtime) {
    if (typeof fd !== "number") {
      throw new ERR_INVALID_ARG_TYPE("fd", "number", fd);
    }
    validateInteger(fd, "fd", 0, 2147483647);
    atime = _getValidTime(atime, "atime");
    mtime = _getValidTime(mtime, "mtime");
    const atimeSecs = MathTrunc(atime);
    const atimeNanos = MathTrunc((atime - atimeSecs) * 1e9);
    const mtimeSecs = MathTrunc(mtime);
    const mtimeNanos = MathTrunc((mtime - mtimeSecs) * 1e9);
    op_node_fs_futimes_sync(fd, atimeSecs, atimeNanos, mtimeSecs, mtimeNanos);
  }
  const lchmod = !isMacOS ? undefined : (path, mode, callback)=>{
    path = getValidatedPathToString(path);
    mode = parseFileMode(mode, "mode");
    callback = makeCallback(callback);
    PromisePrototypeThen(op_node_lchmod(path, mode), ()=>callback(null), (err)=>callback(err));
  };
  const lchmodSync = !isMacOS ? undefined : (path, mode)=>{
    path = getValidatedPathToString(path);
    mode = parseFileMode(mode, "mode");
    return op_node_lchmod_sync(path, mode);
  };
  function lchown(path, uid, gid, callback) {
    callback = makeCallback(callback);
    path = getValidatedPathToString(path);
    validateInteger(uid, "uid", -1, kMaxUserId);
    validateInteger(gid, "gid", -1, kMaxUserId);
    PromisePrototypeThen(op_node_lchown(path, uid, gid), ()=>callback(null), callback);
  }
  function fdatasyncSync(fd) {
    validateInt32(fd, "fd", 0);
    op_node_fs_fdatasync_sync(fd);
  }
  function fsync(fd, callback) {
    validateInt32(fd, "fd", 0);
    PromisePrototypeThen(op_node_fs_fsync(fd), ()=>callback(null), callback);
  }
  function lchownSync(path, uid, gid) {
    path = getValidatedPathToString(path);
    validateInteger(uid, "uid", -1, kMaxUserId);
    validateInteger(gid, "gid", -1, kMaxUserId);
    op_node_lchown_sync(path, uid, gid);
  }
  function fsyncSync(fd) {
    validateInt32(fd, "fd", 0);
    op_node_fs_fsync_sync(fd);
  }
  function link(existingPath, newPath, callback) {
    existingPath = getValidatedPathToString(existingPath);
    newPath = getValidatedPathToString(newPath);
    // Match Node: errors carry both `path` and `dest` (see lib/fs.js link).
    PromisePrototypeThen(Deno.link(existingPath, newPath), ()=>callback(null), (err)=>callback(denoErrorToNodeError(err, {
        syscall: "link",
        path: existingPath,
        dest: newPath
      })));
  }
  function linkSync(existingPath, newPath) {
    existingPath = getValidatedPathToString(existingPath);
    newPath = getValidatedPathToString(newPath);
    try {
      Deno.linkSync(existingPath, newPath);
    } catch (err) {
      throw denoErrorToNodeError(err, {
        syscall: "link",
        path: existingPath,
        dest: newPath
      });
    }
  }
  function unlink(path, callback) {
    path = getValidatedPathToString(path);
    validateFunction(callback, "callback");
    PromisePrototypeThen(Deno.remove(path), ()=>callback(), (err)=>callback(denoErrorToNodeError(err, {
        syscall: "unlink",
        path
      })));
  }
  function unlinkSync(path) {
    path = getValidatedPathToString(path);
    try {
      Deno.removeSync(path);
    } catch (err) {
      throw denoErrorToNodeError(err, {
        syscall: "unlink",
        path
      });
    }
  }
  function rename(oldPath, newPath, callback) {
    oldPath = getValidatedPathToString(oldPath, "oldPath");
    newPath = getValidatedPathToString(newPath, "newPath");
    validateFunction(callback, "callback");
    PromisePrototypeThen(Deno.rename(oldPath, newPath), ()=>callback(), (err)=>callback(denoErrorToNodeError(err, {
        syscall: "rename",
        path: oldPath,
        dest: newPath
      })));
  }
  function renameSync(oldPath, newPath) {
    oldPath = getValidatedPathToString(oldPath, "oldPath");
    newPath = getValidatedPathToString(newPath, "newPath");
    try {
      Deno.renameSync(oldPath, newPath);
    } catch (err) {
      throw denoErrorToNodeError(err, {
        syscall: "rename",
        path: oldPath,
        dest: newPath
      });
    }
  }
  function rm(path, optionsOrCallback, maybeCallback) {
    const callback = typeof optionsOrCallback === "function" ? optionsOrCallback : maybeCallback;
    const options = typeof optionsOrCallback === "object" ? optionsOrCallback : undefined;
    if (!callback) throw new Error("No callback function supplied");
    validateRmOptions(path, options, false, (err, options)=>{
      if (err) {
        return callback(err);
      }
      PromisePrototypeThen(Deno.remove(path, {
        recursive: options?.recursive
      }), ()=>callback(null), (err)=>{
        if (options?.force && ObjectPrototypeIsPrototypeOf(Deno.errors.NotFound.prototype, err)) {
          return callback(null);
        }
        callback(denoErrorToNodeError(err, {
          syscall: "rm"
        }));
      });
    });
  }
  function rmSync(path, options) {
    options = validateRmOptionsSync(path, options, false);
    try {
      Deno.removeSync(path, {
        recursive: options?.recursive
      });
    } catch (err) {
      if (options?.force && ObjectPrototypeIsPrototypeOf(Deno.errors.NotFound.prototype, err)) {
        return;
      }
      throw denoErrorToNodeError(err, {
        syscall: "rm"
      });
    }
  }
  function rmdir(path, options, callback) {
    if (typeof options === "function") {
      callback = options;
      options = undefined;
    }
    if (options?.recursive !== undefined) {
      // The `recursive` option was deprecated and removed in Node. Throw with a
      // clear message rather than silently doing the wrong thing.
      throw new ERR_INVALID_ARG_VALUE("options.recursive", options.recursive, "is no longer supported");
    }
    validateFunction(callback, "cb");
    path = getValidatedPathToString(path);
    validateRmdirOptions(options);
    PromisePrototypeThen(op_node_rmdir(path), (_)=>callback(), (err)=>callback(denoErrorToNodeError(err, {
        syscall: "rmdir",
        path
      })));
  }
  function rmdirSync(path, options) {
    path = getValidatedPathToString(path);
    if (options?.recursive !== undefined) {
      throw new ERR_INVALID_ARG_VALUE("options.recursive", options.recursive, "is no longer supported");
    }
    validateRmdirOptions(options);
    try {
      op_node_rmdir_sync(path);
    } catch (err) {
      throw denoErrorToNodeError(err, {
        syscall: "rmdir",
        path
      });
    }
  }
  /**
 * On Windows, recursive mkdir through a file returns EEXIST instead of
 * ENOTDIR. Check if any component of the path is a file and fix the error.
 */ function fixMkdirError(err, path) {
    const nodeErr = denoErrorToNodeError(err, {
      syscall: "mkdir",
      path
    });
    if (!isWindows) return nodeErr;
    if (nodeErr.code !== "EEXIST") return nodeErr;
    let cursor = resolve(path, "..");
    while(true){
      try {
        const stat = Deno.statSync(cursor);
        if (!stat.isDirectory) {
          return uvException({
            errno: codeMap.get("ENOTDIR"),
            syscall: "mkdir",
            path
          });
        }
        break;
      } catch  {
        const parent = resolve(cursor, "..");
        if (parent === cursor) break;
        cursor = parent;
      }
    }
    return nodeErr;
  }
  /** Find the first component of `path` that does not exist. */ function findFirstNonExistent(path) {
    let cursor = resolve(path);
    while(true){
      try {
        Deno.statSync(cursor);
        return undefined;
      } catch  {
        const parent = resolve(cursor, "..");
        if (parent === cursor) {
          return toNamespacedPath(cursor);
        }
        try {
          Deno.statSync(parent);
          return toNamespacedPath(cursor);
        } catch  {
          cursor = parent;
        }
      }
    }
  }
  function mkdir(path, options, callback) {
    path = getValidatedPath(path);
    let mode = 0o777;
    let recursive = false;
    if (typeof options == "function") {
      callback = options;
    } else if (typeof options === "number" || typeof options === "string") {
      // Match Node: a number or string second arg is a file mode
      // (see lib/fs.js mkdir).
      mode = parseFileMode(options, "mode");
    } else if (typeof options === "boolean") {
      recursive = options;
    } else if (options) {
      if (options.recursive !== undefined) recursive = options.recursive;
      if (options.mode !== undefined) {
        mode = parseFileMode(options.mode, "options.mode");
      }
    }
    validateBoolean(recursive, "options.recursive");
    let firstNonExistent;
    try {
      firstNonExistent = recursive ? findFirstNonExistent(path) : undefined;
    } catch (err) {
      if (typeof callback === "function") {
        callback(denoErrorToNodeError(err, {
          syscall: "mkdir",
          path
        }));
      }
      return;
    }
    PromisePrototypeThen(Deno.mkdir(path, {
      recursive,
      mode
    }), ()=>{
      if (typeof callback === "function") {
        callback(null, firstNonExistent);
      }
    }, (err)=>{
      if (typeof callback === "function") {
        callback(recursive ? fixMkdirError(err, path) : denoErrorToNodeError(err, {
          syscall: "mkdir",
          path
        }));
      }
    });
  }
  function mkdirSync(path, options) {
    path = getValidatedPath(path);
    let mode = 0o777;
    let recursive = false;
    if (typeof options === "number" || typeof options === "string") {
      // Match Node: a number or string second arg is a file mode
      // (see lib/fs.js mkdirSync).
      mode = parseFileMode(options, "mode");
    } else if (typeof options === "boolean") {
      recursive = options;
    } else if (options) {
      if (options.recursive !== undefined) recursive = options.recursive;
      if (options.mode !== undefined) {
        mode = parseFileMode(options.mode, "options.mode");
      }
    }
    validateBoolean(recursive, "options.recursive");
    let firstNonExistent;
    try {
      firstNonExistent = recursive ? findFirstNonExistent(path) : undefined;
      Deno.mkdirSync(path, {
        recursive,
        mode
      });
    } catch (err) {
      throw recursive ? fixMkdirError(err, path) : denoErrorToNodeError(err, {
        syscall: "mkdir",
        path
      });
    }
    return firstNonExistent;
  }
  function mkdtemp(prefix, options, callback) {
    if (typeof options === "function") {
      callback = options;
      options = undefined;
    }
    callback = makeCallback(callback);
    const encoding = parseMkdtempEncoding(options);
    prefix = getValidatedPathToString(prefix, "prefix");
    warnOnNonPortableTemplate(prefix);
    PromisePrototypeThen(op_node_mkdtemp(prefix), (path)=>callback(null, decodeMkdtemp(path, encoding)), (err)=>callback(denoErrorToNodeError(err, {
        syscall: "mkdtemp",
        path: `${prefix}XXXXXX`
      })));
  }
  function mkdtempSync(prefix, options) {
    const encoding = parseMkdtempEncoding(options);
    prefix = getValidatedPathToString(prefix, "prefix");
    warnOnNonPortableTemplate(prefix);
    try {
      const path = op_node_mkdtemp_sync(prefix);
      return decodeMkdtemp(path, encoding);
    } catch (err) {
      throw denoErrorToNodeError(err, {
        syscall: "mkdtemp",
        path: `${prefix}XXXXXX`
      });
    }
  }
  // Mirrors Node's lib/fs.js mkdtempDisposableSync(): create the temp dir and
  // return an object with .path, .remove(), and Symbol.dispose. cwd is captured
  // at creation time so a later lazyProcess().default.chdir() doesn't break removal.
  function mkdtempDisposableSync(prefix, options) {
    const cwd = lazyProcess().default.cwd();
    const path = mkdtempSync(prefix, options);
    const fullPath = resolve(cwd, path);
    const remove = ()=>{
      rmSync(fullPath, {
        force: true,
        maxRetries: 0,
        recursive: true,
        retryDelay: 0
      });
    };
    return {
      path,
      remove,
      [SymbolDispose] () {
        remove();
      }
    };
  }
  function decodeMkdtemp(str, encoding) {
    if (encoding === "utf8") return str;
    const buffer = Buffer.from(str);
    if (encoding === "buffer") return buffer;
    // deno-lint-ignore deno-internal/prefer-primordials
    return buffer.toString(encoding);
  }
  function parseMkdtempEncoding(options) {
    let encoding;
    if (typeof options === "undefined" || options === null) {
      encoding = "utf8";
    } else if (typeof options === "string") {
      encoding = options;
    } else if (typeof options === "object") {
      encoding = options.encoding ?? "utf8";
    } else {
      throw new ERR_INVALID_ARG_TYPE("options", [
        "string",
        "Object"
      ], options);
    }
    if (encoding === "buffer") {
      return encoding;
    }
    const parsedEncoding = normalizeEncoding(encoding);
    if (!parsedEncoding) {
      throw new ERR_INVALID_ARG_TYPE("encoding", encoding, "is invalid encoding");
    }
    return parsedEncoding;
  }
  function open(path, flags, mode, callback) {
    path = getValidatedPathToString(path);
    if (arguments.length < 3) {
      callback = flags;
      flags = "r";
      mode = 0o666;
    } else if (typeof mode === "function") {
      callback = mode;
      mode = 0o666;
    } else {
      mode = parseFileMode(mode, "mode", 0o666);
    }
    flags = stringToFlags(flags);
    callback = makeCallback(callback);
    const request = createFSReqCallback();
    let openPromise;
    try {
      openPromise = op_node_open(path, flags, mode);
    } catch (err) {
      unregisterActiveRequest(request);
      throw err;
    }
    PromisePrototypeThen(openPromise, (rid)=>{
      unregisterActiveRequest(request);
      callback(null, rid);
    }, (err)=>{
      unregisterActiveRequest(request);
      callback(denoErrorToNodeError(err, {
        syscall: "open",
        path
      }));
    });
  }
  function openSync(path, flags = "r", maybeMode) {
    path = getValidatedPathToString(path);
    flags = stringToFlags(flags);
    const mode = parseFileMode(maybeMode, "mode", 0o666);
    try {
      return op_node_open_sync(path, flags, mode);
    } catch (err) {
      throw denoErrorToNodeError(err, {
        syscall: "open",
        path
      });
    }
  }
  function _opendirValidateFunction(callback) {
    validateFunction(callback, "callback");
  }
  function _opendirGetPathString(path) {
    if (Buffer.isBuffer(path)) {
      // deno-lint-ignore deno-internal/prefer-primordials
      return path.toString();
    }
    return StringPrototypeToString(path);
  }
  function opendir(path, options, callback) {
    callback = typeof options === "function" ? options : callback;
    _opendirValidateFunction(callback);
    path = _opendirGetPathString(getValidatedPath(path));
    let err, dir;
    try {
      const { bufferSize } = getOptions(options, {
        encoding: "utf8",
        bufferSize: 32
      });
      validateInteger(bufferSize, "options.bufferSize", 1, 4294967295);
      /** Throws if path is invalid */ Deno.readDirSync(path);
      dir = new Dir(path);
    } catch (error) {
      err = denoErrorToNodeError(error, {
        syscall: "opendir"
      });
    }
    if (err) {
      callback(err);
    } else {
      callback(null, dir);
    }
  }
  function opendirSync(path, options) {
    path = _opendirGetPathString(getValidatedPath(path));
    const { bufferSize } = getOptions(options, {
      encoding: "utf8",
      bufferSize: 32
    });
    validateInteger(bufferSize, "options.bufferSize", 1, 4294967295);
    try {
      /** Throws if path is invalid */ Deno.readDirSync(path);
      return new Dir(path);
    } catch (err) {
      throw denoErrorToNodeError(err, {
        syscall: "opendir"
      });
    }
  }
  /**
 * Returns a `Blob` whose data is read from the given file.
 */ function openAsBlob(path, options = {
    __proto__: null
  }) {
    validateObject(options, "options");
    const type = options.type || "";
    validateString(type, "options.type");
    path = getValidatedPath(path);
    return PromisePrototypeThen(op_fs_read_file_async(path, undefined, 0), (data)=>markFileBackedBlob(new Blob([
        data
      ], {
        type
      })));
  }
  function writeSync(fd, buffer, offsetOrOptions, length, position) {
    fd = getValidatedFd(fd);
    const innerWriteSync = (fd, buffer, offset, length, position)=>{
      buffer = arrayBufferViewToUint8Array(buffer);
      const pos = typeof position === "number" && position >= 0 ? position : -1;
      return op_node_fs_write_sync(fd, buffer.subarray(offset, offset + length), pos);
    };
    let offset = offsetOrOptions;
    if (isArrayBufferView(buffer)) {
      if (typeof offset === "object") {
        ({ offset = 0, // deno-lint-ignore deno-internal/prefer-primordials
        length = buffer.byteLength - offset, position = null } = offsetOrOptions ?? kEmptyObject);
      }
      if (position === undefined) {
        position = null;
      }
      if (offset == null) {
        offset = 0;
      } else {
        validateInteger(offset, "offset", 0);
      }
      if (typeof length !== "number") {
        // deno-lint-ignore deno-internal/prefer-primordials
        length = buffer.byteLength - offset;
      }
      // deno-lint-ignore deno-internal/prefer-primordials
      validateOffsetLengthWrite(offset, length, buffer.byteLength);
      return innerWriteSync(fd, buffer, offset, length, position);
    }
    validateStringAfterArrayBufferView(buffer, "buffer");
    validateEncoding(buffer, length);
    buffer = Buffer.from(buffer, length);
    return innerWriteSync(fd, buffer, 0, buffer.length, position);
  }
  /** Writes the buffer to the file of the given descriptor.
 * https://nodejs.org/api/fs.html#fswritefd-buffer-offset-length-position-callback
 * https://github.com/nodejs/node/blob/42ad4137aadda69c51e1df48eee9bc2e5cebca5c/lib/fs.js#L797
 */ function write(fd, buffer, offsetOrOptions, length, position, callback) {
    fd = getValidatedFd(fd);
    const innerWrite = async (fd, buffer, offset, length, position)=>{
      buffer = arrayBufferViewToUint8Array(buffer);
      const pos = typeof position === "number" && position >= 0 ? position : -1;
      return await op_node_fs_write_deferred(fd, buffer.subarray(offset, offset + length), pos);
    };
    let offset = offsetOrOptions;
    if (isArrayBufferView(buffer)) {
      callback = maybeCallback(callback || position || length || offset);
      if (typeof offset === "object") {
        ({ offset = 0, // deno-lint-ignore deno-internal/prefer-primordials
        length = buffer.byteLength - offset, position = null } = offsetOrOptions ?? kEmptyObject);
      }
      if (offset == null || typeof offset === "function") {
        offset = 0;
      } else {
        validateInteger(offset, "offset", 0);
      }
      if (typeof length !== "number") {
        // deno-lint-ignore deno-internal/prefer-primordials
        length = buffer.byteLength - offset;
      }
      if (typeof position !== "number") {
        position = null;
      }
      // deno-lint-ignore deno-internal/prefer-primordials
      validateOffsetLengthWrite(offset, length, buffer.byteLength);
      // deno-lint-ignore deno-internal/prefer-primordials
      innerWrite(fd, buffer, offset, length, position).then((nwritten)=>{
        callback(null, nwritten, buffer);
      }, (err)=>callback(err));
      return;
    }
    // Here the call signature is
    // `fs.write(fd, string[, position[, encoding]], callback)`
    validateStringAfterArrayBufferView(buffer, "buffer");
    if (typeof position !== "function") {
      if (typeof offset === "function") {
        position = offset;
        offset = null;
      } else {
        position = length;
      }
      length = "utf-8";
    }
    const str = buffer;
    validateEncoding(str, length);
    callback = maybeCallback(position);
    buffer = Buffer.from(str, length);
    // deno-lint-ignore deno-internal/prefer-primordials
    innerWrite(fd, buffer, 0, buffer.length, offset).then((nwritten)=>{
      callback(null, nwritten, buffer);
    }, (err)=>callback(err));
  }
  ObjectDefineProperty(write, customPromisifyArgs, {
    __proto__: null,
    value: [
      "bytesWritten",
      "buffer"
    ],
    enumerable: false
  });
  /**
 * Write an array of `ArrayBufferView`s to the file specified by `fd` using`writev()`.
 *
 * `position` is the offset from the beginning of the file where this data
 * should be written. If `typeof position !== 'number'`, the data will be written
 * at the current position.
 *
 * The callback will be given three arguments: `err`, `bytesWritten`, and`buffers`. `bytesWritten` is how many bytes were written from `buffers`.
 *
 * If this method is `util.promisify()` ed, it returns a promise for an`Object` with `bytesWritten` and `buffers` properties.
 *
 * It is unsafe to use `fs.writev()` multiple times on the same file without
 * waiting for the callback. For this scenario, use {@link createWriteStream}.
 *
 * On Linux, positional writes don't work when the file is opened in append mode.
 * The kernel ignores the position argument and always appends the data to
 * the end of the file.
 * @since v12.9.0
 */ function writev(fd, buffers, position, callback) {
    const innerWritev = async (fd, buffers, position)=>{
      const chunks = [];
      for(let i = 0; i < buffers.length; i++){
        if (Buffer.isBuffer(buffers[i])) {
          // deno-lint-ignore deno-internal/prefer-primordials
          chunks.push(buffers[i]);
        } else {
          // deno-lint-ignore deno-internal/prefer-primordials
          chunks.push(Buffer.from(buffers[i]));
        }
      }
      const pos = typeof position === "number" ? position : -1;
      // deno-lint-ignore deno-internal/prefer-primordials
      const buffer = Buffer.concat(chunks);
      return await op_node_fs_write_deferred(fd, buffer, pos);
    };
    fd = getValidatedFd(fd);
    validateBufferArray(buffers);
    callback = maybeCallback(callback || position);
    if (buffers.length === 0) {
      lazyProcess().default.nextTick(callback, null, 0, buffers);
      return;
    }
    if (typeof position !== "number") position = null;
    // deno-lint-ignore deno-internal/prefer-primordials
    innerWritev(fd, buffers, position).then((nwritten)=>callback(null, nwritten, buffers), (err)=>callback(err));
  }
  /**
 * For detailed information, see the documentation of the asynchronous version of
 * this API: {@link writev}.
 * @since v12.9.0
 * @return The number of bytes written.
 */ function writevSync(fd, buffers, position) {
    const innerWritev = (fd, buffers, position)=>{
      const chunks = [];
      for(let i = 0; i < buffers.length; i++){
        if (Buffer.isBuffer(buffers[i])) {
          // deno-lint-ignore deno-internal/prefer-primordials
          chunks.push(buffers[i]);
        } else {
          // deno-lint-ignore deno-internal/prefer-primordials
          chunks.push(Buffer.from(buffers[i]));
        }
      }
      const pos = typeof position === "number" ? position : -1;
      // deno-lint-ignore deno-internal/prefer-primordials
      const buffer = Buffer.concat(chunks);
      return op_node_fs_write_sync(fd, buffer, pos);
    };
    fd = getValidatedFd(fd);
    validateBufferArray(buffers);
    if (buffers.length === 0) {
      return 0;
    }
    if (typeof position !== "number") position = null;
    return innerWritev(fd, buffers, position);
  }
  const { kWriteFileMaxChunkSize } = fsUtilConstants;
  async function _writeFileGetRid(pathOrRid, flag = "w") {
    if (typeof pathOrRid === "number") {
      return pathOrRid;
    }
    try {
      return await op_node_open(pathOrRid, stringToFlags(flag), 0o666);
    } catch (err) {
      throw denoErrorToNodeError(err, {
        syscall: "open",
        path: pathOrRid
      });
    }
  }
  function _writeFileGetRidSync(pathOrRid, flag = "w") {
    if (typeof pathOrRid === "number") {
      return pathOrRid;
    }
    try {
      return op_node_open_sync(pathOrRid, stringToFlags(flag), 0o666);
    } catch (err) {
      throw denoErrorToNodeError(err, {
        syscall: "open",
        path: pathOrRid
      });
    }
  }
  function writeFile(pathOrRid, data, options, callback) {
    let flag;
    let mode;
    let signal;
    if (typeof options === "function") {
      callback = options;
      options = undefined;
    }
    validateFunction(callback, "callback");
    if (ObjectPrototypeIsPrototypeOf(URLPrototype, pathOrRid)) {
      pathOrRid = pathFromURL(pathOrRid);
    } else if (ObjectPrototypeIsPrototypeOf(lazyInternalHandle().FileHandle.prototype, pathOrRid)) {
      pathOrRid = pathOrRid.fd;
    }
    if (isFileOptions(options)) {
      flag = options.flag;
      mode = options.mode;
      signal = options.signal;
    }
    const flush = typeof options === "object" && options !== null ? options.flush ?? false : false;
    validateBoolean(flush, "options.flush");
    const encoding = getValidatedEncoding(options) || "utf8";
    if (!ArrayBufferIsView(data) && !_isCustomIterable(data)) {
      validateStringAfterArrayBufferView(data, "data");
      data = Buffer.from(data, encoding);
    }
    const isRid = typeof pathOrRid === "number";
    let file;
    let error = null;
    let syscall = "write";
    (async ()=>{
      try {
        const fd = await _writeFileGetRid(pathOrRid, flag);
        file = {
          write (p) {
            // Use the deferred op to yield to the event loop between writes,
            // allowing abort signals scheduled via lazyProcess().default.nextTick to fire.
            return op_node_fs_write_deferred(fd, p, -1);
          },
          writeSync (p) {
            return op_node_fs_write_sync(fd, p, -1);
          },
          close () {
            op_node_fs_close(fd);
          }
        };
        _checkAborted(signal);
        if (!isRid && mode) {
          await Deno.chmod(pathOrRid, mode);
          _checkAborted(signal);
        }
        await _writeAll(file, data, encoding, signal);
        if (flush) {
          syscall = "fsync";
          await new Promise((resolve, reject)=>{
            fsExports.fsync(fd, (err)=>{
              if (err) reject(err);
              else resolve();
            });
          });
        }
      } catch (e) {
        error = denoWriteFileErrorToNodeError(e, {
          syscall
        });
      } finally{
        // Make sure to close resource
        if (!isRid && file) file.close();
        callback(error);
      }
    })();
  }
  function writeFileSync(pathOrRid, data, options) {
    let flag;
    let mode;
    pathOrRid = ObjectPrototypeIsPrototypeOf(URLPrototype, pathOrRid) ? pathFromURL(pathOrRid) : pathOrRid;
    if (isFileOptions(options)) {
      flag = options.flag;
      mode = options.mode;
    }
    const flush = typeof options === "object" && options !== null ? options.flush ?? false : false;
    validateBoolean(flush, "options.flush");
    const encoding = getValidatedEncoding(options) || "utf8";
    // Match Node: fs.writeFileSync only accepts string or ArrayBufferView for
    // data (see lib/fs.js). The async Promise variant supports custom
    // iterables, but the sync version does not.
    if (!ArrayBufferIsView(data)) {
      validateStringAfterArrayBufferView(data, "data");
      data = Buffer.from(data, encoding);
    }
    const isRid = typeof pathOrRid === "number";
    let file;
    let error = null;
    let syscall = "write";
    try {
      const fd = _writeFileGetRidSync(pathOrRid, flag);
      file = {
        write (p) {
          return PromiseResolve(op_node_fs_write_sync(fd, p, -1));
        },
        writeSync (p) {
          return op_node_fs_write_sync(fd, p, -1);
        },
        close () {
          op_node_fs_close(fd);
        }
      };
      if (!isRid && mode) {
        Deno.chmodSync(pathOrRid, mode);
      }
      _writeAllSync(file, data, encoding);
      if (flush) {
        syscall = "fsync";
        fsExports.fsyncSync(fd);
      }
    } catch (e) {
      error = denoWriteFileErrorToNodeError(e, {
        syscall
      });
    } finally{
      // Make sure to close resource
      if (!isRid && file) file.close();
    }
    if (error) throw error;
  }
  function _writeAllSync(w, data, encoding) {
    if (!_isCustomIterable(data)) {
      // deno-lint-ignore deno-internal/prefer-primordials
      data = new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
      // deno-lint-ignore deno-internal/prefer-primordials
      let remaining = data.byteLength;
      while(remaining > 0){
        const bytesWritten = w.writeSync(// deno-lint-ignore deno-internal/prefer-primordials
        data.subarray(data.byteLength - remaining));
        remaining -= bytesWritten;
      }
    } else {
      // deno-lint-ignore deno-internal/prefer-primordials
      for (const buf of data){
        let toWrite = ArrayBufferIsView(buf) ? buf : Buffer.from(buf, encoding);
        toWrite = new Uint8Array(// deno-lint-ignore deno-internal/prefer-primordials
        toWrite.buffer, // deno-lint-ignore deno-internal/prefer-primordials
        toWrite.byteOffset, // deno-lint-ignore deno-internal/prefer-primordials
        toWrite.byteLength);
        // deno-lint-ignore deno-internal/prefer-primordials
        let remaining = toWrite.byteLength;
        while(remaining > 0){
          const bytesWritten = w.writeSync(// deno-lint-ignore deno-internal/prefer-primordials
          toWrite.subarray(toWrite.byteLength - remaining));
          remaining -= bytesWritten;
        }
      }
    }
  }
  async function _writeAll(w, data, encoding, signal) {
    if (!_isCustomIterable(data)) {
      // deno-lint-ignore deno-internal/prefer-primordials
      data = new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
      // deno-lint-ignore deno-internal/prefer-primordials
      let remaining = data.byteLength;
      while(remaining > 0){
        const writeSize = MathMin(kWriteFileMaxChunkSize, remaining);
        // deno-lint-ignore deno-internal/prefer-primordials
        const offset = data.byteLength - remaining;
        const bytesWritten = await w.write(data.subarray(offset, offset + writeSize));
        remaining -= bytesWritten;
        _checkAborted(signal);
      }
    } else {
      // deno-lint-ignore deno-internal/prefer-primordials
      for await (const buf of data){
        _checkAborted(signal);
        let toWrite = ArrayBufferIsView(buf) ? buf : Buffer.from(buf, encoding);
        toWrite = new Uint8Array(// deno-lint-ignore deno-internal/prefer-primordials
        toWrite.buffer, // deno-lint-ignore deno-internal/prefer-primordials
        toWrite.byteOffset, // deno-lint-ignore deno-internal/prefer-primordials
        toWrite.byteLength);
        // deno-lint-ignore deno-internal/prefer-primordials
        let remaining = toWrite.byteLength;
        while(remaining > 0){
          const writeSize = MathMin(kWriteFileMaxChunkSize, remaining);
          // deno-lint-ignore deno-internal/prefer-primordials
          const offset = toWrite.byteLength - remaining;
          const bytesWritten = await w.write(toWrite.subarray(offset, offset + writeSize));
          remaining -= bytesWritten;
          _checkAborted(signal);
        }
      }
    }
    _checkAborted(signal);
  }
  function _isCustomIterable(obj) {
    return isIterable(obj) && !ArrayBufferIsView(obj) && typeof obj !== "string";
  }
  function _checkAborted(signal) {
    if (signal?.aborted) {
      throw new AbortError();
    }
  }
  // -- truncate --
  function truncate(path, lenOrCallback = 0, maybeCallback) {
    let len = 0;
    let callback;
    if (typeof lenOrCallback === "function") {
      callback = lenOrCallback;
    } else {
      len = lenOrCallback;
      callback = maybeCallback;
    }
    // Match Node: validate len before any async work (lib/fs.js truncate).
    validateInteger(len, "len");
    len = MathMax(0, len);
    if (!callback) throw new Error("No callback function supplied");
    // Match Node: open with 'r+', ftruncate, close so ENOENT and other
    // errors surface with a path/syscall='open' rather than the raw
    // truncate error.
    open(path, "r+", (openErr, fd)=>{
      if (openErr) {
        callback(openErr);
        return;
      }
      ftruncate(fd, len, (ftErr)=>{
        close(fd, (closeErr)=>{
          callback(ftErr || closeErr || null);
        });
      });
    });
  }
  function truncateSync(path, len = 0) {
    validateInteger(len, "len");
    len = MathMax(0, len);
    // Match Node: open with 'r+', ftruncate, close.
    const fd = openSync(path, "r+");
    try {
      ftruncateSync(fd, len);
    } finally{
      closeSync(fd);
    }
  }
  // -- utimes --
  function getValidTime(time, name) {
    if (typeof time === "string") {
      time = Number(time);
    }
    if (typeof time === "number" && (NumberIsNaN(time) || !NumberIsFinite(time))) {
      throw new Deno.errors.InvalidData(`invalid ${name}, must not be infinity or NaN`);
    }
    return toUnixTimestamp(time);
  }
  function utimes(path, atime, mtime, callback) {
    // deno-lint-ignore deno-internal/prefer-primordials
    path = getValidatedPath(path).toString();
    if (!callback) {
      throw new Deno.errors.InvalidData("No callback function supplied");
    }
    atime = getValidTime(atime, "atime");
    mtime = getValidTime(mtime, "mtime");
    PromisePrototypeThen(Deno.utime(path, atime, mtime), ()=>callback(null), callback);
  }
  function utimesSync(path, atime, mtime) {
    // deno-lint-ignore deno-internal/prefer-primordials
    path = getValidatedPath(path).toString();
    atime = getValidTime(atime, "atime");
    mtime = getValidTime(mtime, "mtime");
    Deno.utimeSync(path, atime, mtime);
  }
  function symlink(target, path, linkType, callback) {
    if (callback === undefined) {
      callback = linkType;
      linkType = undefined;
    } else {
      validateOneOf(linkType, "type", [
        "dir",
        "file",
        "junction",
        null,
        undefined
      ]);
    }
    callback = makeCallback(callback);
    target = getValidatedPathToString(target, "target");
    path = getValidatedPathToString(path);
    if (isWindows && !linkType) {
      let absoluteTarget;
      try {
        // Symlinks targets can be relative to the newly created path.
        // Calculate absolute file name of the symlink target, and check
        // if it is a directory. Ignore resolve error to keep symlink
        // errors consistent between platforms if invalid path is
        // provided.
        absoluteTarget = pathModule.resolve(path, "..", target);
      } catch  {
      // Continue regardless of error.
      }
      if (absoluteTarget !== undefined) {
        stat(absoluteTarget, (err, stat)=>{
          const resolvedType = !err && stat.isDirectory() ? "dir" : "file";
          PromisePrototypeThen(Deno.symlink(target, path, {
            type: resolvedType
          }), ()=>callback(null), callback);
        });
        return;
      }
    }
    PromisePrototypeThen(Deno.symlink(target, path, {
      type: linkType ?? "file"
    }), ()=>callback(null), callback);
  }
  function symlinkSync(target, path, type) {
    validateOneOf(type, "type", [
      "dir",
      "file",
      "junction",
      null,
      undefined
    ]);
    target = getValidatedPathToString(target, "target");
    path = getValidatedPathToString(path);
    if (isWindows && !type) {
      const absoluteTarget = pathModule.resolve(path, "..", target);
      if (statSync(absoluteTarget, {
        bigint: false,
        throwIfNoEntry: false
      })?.isDirectory()) {
        type = "dir";
      }
    }
    Deno.symlinkSync(target, path, {
      type: type ?? "file"
    });
  }
  // -- watch --
  const statPromisified = promisify(stat);
  const statAsync = async (filename, bigint)=>{
    try {
      return bigint ? await statPromisified(filename, {
        bigint: true
      }) : await statPromisified(filename, {
        bigint: false
      });
    } catch  {
      return bigint ? emptyBigIntStats : emptyStats;
    }
  };
  const emptyStats = new Stats(0, 0, 0, 0, 0, 0, 0, 0, 0, 0, DateUTC(1970, 0, 1, 0, 0, 0), DateUTC(1970, 0, 1, 0, 0, 0), DateUTC(1970, 0, 1, 0, 0, 0), DateUTC(1970, 0, 1, 0, 0, 0));
  const emptyBigIntStats = new BigIntStats(0n, 0n, 0n, 0n, 0n, 0n, 0n, 0n, 0n, 0n, 0n, 0n, 0n, 0n);
  // Mirrors libuv's `uv_fs_poll_t` field comparison so chmod/chown,
  // file replacement, and sub-mtime-resolution changes all fire "change".
  function statsChanged(prev, curr) {
    return prev.mtimeMs !== curr.mtimeMs || prev.ctimeMs !== curr.ctimeMs || prev.size !== curr.size || prev.mode !== curr.mode || prev.uid !== curr.uid || prev.gid !== curr.gid || prev.ino !== curr.ino || prev.dev !== curr.dev;
  }
  function asyncIterableToCallback(iter, callback, errCallback) {
    const iterator = iter[SymbolAsyncIterator]();
    function next() {
      // deno-lint-ignore deno-internal/prefer-primordials
      PromisePrototypeThen(iterator.next(), (obj)=>{
        if (obj.done) {
          callback(obj.value, true);
          return;
        }
        callback(obj.value);
        next();
      }, errCallback);
    }
    next();
  }
  let _lazyMinimatch = null;
  function getMinimatch() {
    _lazyMinimatch ??= core.createLazyLoader("ext:deno_node/deps/minimatch.js");
    return _lazyMinimatch();
  }
  function validateIgnoreOptionElement(value, name) {
    if (typeof value === "string") {
      if (value.length === 0) {
        throw new ERR_INVALID_ARG_VALUE(name, value, "must be a non-empty string");
      }
      return;
    }
    if (ObjectPrototypeIsPrototypeOf(RegExpPrototype, value)) return;
    if (typeof value === "function") return;
    throw new ERR_INVALID_ARG_TYPE(name, [
      "string",
      "RegExp",
      "Function"
    ], value);
  }
  function validateIgnoreOption(value, name) {
    if (value == null) return;
    if (ArrayIsArray(value)) {
      for(let i = 0; i < value.length; i++){
        validateIgnoreOptionElement(value[i], `${name}[${i}]`);
      }
      return;
    }
    validateIgnoreOptionElement(value, name);
  }
  function createIgnoreMatcher(ignore) {
    if (ignore == null) return null;
    const matchers = ArrayIsArray(ignore) ? ignore : [
      ignore
    ];
    const compiled = [];
    for(let i = 0; i < matchers.length; i++){
      const matcher = matchers[i];
      if (typeof matcher === "string") {
        const { Minimatch } = getMinimatch().default;
        const mm = new Minimatch(matcher, {
          nocase: isMacOS || isWindows,
          windowsPathsNoEscape: true,
          nonegate: true,
          nocomment: true,
          optimizationLevel: 2,
          platform: isWindows ? "win32" : "posix",
          // Allow patterns without slashes to match the basename
          // e.g. '*.log' matches 'subdir/file.log'.
          matchBase: true
        });
        ArrayPrototypePush(compiled, // deno-lint-ignore deno-internal/prefer-primordials
        (filename)=>mm.match(filename));
      } else if (ObjectPrototypeIsPrototypeOf(RegExpPrototype, matcher)) {
        ArrayPrototypePush(compiled, (filename)=>RegExpPrototypeTest(matcher, filename));
      } else {
        // Function
        ArrayPrototypePush(compiled, matcher);
      }
    }
    return (filename)=>{
      for(let i = 0; i < compiled.length; i++){
        if (compiled[i](filename)) return true;
      }
      return false;
    };
  }
  // Match Node: `encoding: 'buffer'` returns a Buffer, any other named encoding
  // returns the filename re-encoded from utf8. Default ('utf8' or absent) leaves
  // the string unchanged. https://github.com/nodejs/node/blob/main/lib/internal/fs/watchers.js
  function encodeWatchFilename(filename, encoding) {
    if (!encoding || encoding === "utf8" || encoding === "utf-8") {
      return filename;
    }
    const asBuffer = Buffer.from(filename);
    if (encoding === "buffer") {
      return asBuffer;
    }
    // deno-lint-ignore deno-internal/prefer-primordials
    return asBuffer.toString(encoding);
  }
  function watch(filename, optionsOrListener, optionsOrListener2) {
    const listener = typeof optionsOrListener === "function" ? optionsOrListener : typeof optionsOrListener2 === "function" ? optionsOrListener2 : undefined;
    const options = typeof optionsOrListener === "object" ? optionsOrListener : typeof optionsOrListener2 === "object" ? optionsOrListener2 : undefined;
    validateIgnoreOption(options?.ignore, "options.ignore");
    // deno-lint-ignore deno-internal/prefer-primordials
    const watchPath = getValidatedPath(filename).toString();
    // Match Node: validate non-boolean `recursive`/`persistent` up front.
    // https://github.com/nodejs/node/blob/main/lib/internal/fs/recursive_watch.js
    if (options != null && options.recursive != null) {
      validateBoolean(options.recursive, "options.recursive");
    }
    if (options != null && options.persistent != null) {
      validateBoolean(options.persistent, "options.persistent");
    }
    const recursive = options?.recursive || false;
    const encoding = options?.encoding;
    validateIgnoreOption(options?.ignore, "options.ignore");
    const ignoreMatcher = createIgnoreMatcher(options?.ignore);
    // Open the underlying Deno.FsWatcher, but defer any failure to an
    // 'error' event on the returned FSWatcher rather than throwing
    // synchronously with a raw `Deno.errors.NotFound`. Editors that
    // atomically save (write to <file>.tmp.<pid>.<ts> then rename over
    // the original) can race the inotify watch and produce a transient
    // ENOENT here; callers using EventEmitter-style error handling
    // (chokidar, vite) can't recover from a sync throw of a Deno error.
    // See denoland/deno#34396.
    const notFoundProto = Deno.errors.NotFound.prototype;
    const makeWatchNodeError = (e)=>{
      // The notify crate's PathNotFound/WatchNotFound error messages don't
      // include the "(os error N)" suffix that `denoErrorToNodeError` parses,
      // so detect NotFound by class and build a Node-style ENOENT manually.
      if (ObjectPrototypeIsPrototypeOf(notFoundProto, e)) {
        return uvException({
          errno: codeMap.get("ENOENT"),
          syscall: "watch",
          path: watchPath
        });
      }
      return denoErrorToNodeError(e, {
        syscall: "watch",
        path: watchPath
      });
    };
    let iterator;
    let openError;
    let resolvedWatchPath = watchPath;
    try {
      // Pre-validate path existence so missing-path failures surface as a
      // typed `Deno.errors.NotFound` consistently across platforms. The
      // notify crate's Windows backend (`add_watch` in src/windows.rs)
      // reports a missing path as a Generic error rather than a typed
      // NotFound; runtime/ops/fs_events.rs maps that message to NotFound so
      // the prototype check in makeWatchNodeError above also covers paths
      // that vanish between this check and the watch (or mid-watch), see
      // denoland/deno#35855.
      Deno.lstatSync(watchPath);
      iterator = Deno.watchFs(watchPath, {
        recursive
      });
      // Resolve the watched path once so we can compute relative paths.
      // Use realPathSync to resolve symlinks (e.g. macOS /var -> /private/var)
      // since Deno.watchFs returns real (symlink-resolved) paths.
      resolvedWatchPath = realpathSync(watchPath);
    } catch (e) {
      if (iterator) {
        try {
          iterator.close();
        } catch  {}
        iterator = undefined;
      }
      openError = makeWatchNodeError(e);
    }
    if (iterator) {
      asyncIterableToCallback(iterator, (val, done)=>{
        if (done) return;
        // Node.js returns the relative path from the watched directory for
        // recursive watches, but just the basename for non-recursive watches.
        const filename = recursive ? relative(resolvedWatchPath, val.paths[0]) : basename(val.paths[0]);
        if (ignoreMatcher !== null && ignoreMatcher(filename)) {
          return;
        }
        fsWatcher.emit("change", convertDenoFsEventToNodeFsEvent(val.kind), encodeWatchFilename(filename, encoding));
      }, (e)=>{
        fsWatcher.emit("error", makeWatchNodeError(e));
      });
    }
    const fsWatcher = new FSWatcher(()=>{
      if (!iterator) return;
      try {
        iterator.close();
      } catch (e) {
        if (ObjectPrototypeIsPrototypeOf(Deno.errors.BadResource.prototype, e)) {
          // already closed
          return;
        }
        throw e;
      }
    }, ()=>iterator);
    if (listener) {
      fsWatcher.on("change", FunctionPrototypeBind(listener, {
        _handle: fsWatcher
      }));
    }
    // Match Node's `fs.watch` AbortSignal handling:
    // https://github.com/nodejs/node/blob/main/lib/fs.js
    validateAbortSignal(options?.signal, "options.signal");
    if (options?.signal) {
      const signal = options.signal;
      if (signal.aborted) {
        lazyProcess().default.nextTick(()=>fsWatcher.close());
      } else {
        const onAbort = ()=>fsWatcher.close();
        signal.addEventListener("abort", onAbort, {
          once: true
        });
        fsWatcher.once("close", ()=>{
          signal.removeEventListener("abort", onAbort);
        });
      }
    }
    if (openError) {
      lazyProcess().default.nextTick(()=>{
        fsWatcher.emit("error", openError);
      });
    }
    return fsWatcher;
  }
  function watchPromise(filename, options) {
    // deno-lint-ignore deno-internal/prefer-primordials
    const watchPath = getValidatedPath(filename).toString();
    const recursive = options?.recursive ?? false;
    const signal = options?.signal;
    validateAbortSignal(signal, "options.signal");
    validateIgnoreOption(options?.ignore, "options.ignore");
    const ignoreMatcher = createIgnoreMatcher(options?.ignore);
    const watcher = Deno.watchFs(watchPath, {
      recursive
    });
    const resolvedWatchPath = realpathSync(watchPath);
    let onAbort = null;
    function cleanupAbort() {
      if (signal && onAbort) {
        signal.removeEventListener("abort", onAbort);
        onAbort = null;
      }
    }
    if (signal) {
      if (signal.aborted) {
        watcher.close();
      } else {
        onAbort = ()=>watcher.close();
        signal.addEventListener("abort", onAbort, {
          once: true
        });
      }
    }
    // Match Node: surface signal abort as a thrown AbortError carrying
    // `signal.reason` as `cause`.
    // https://github.com/nodejs/node/blob/main/lib/internal/fs/watchers.js
    function abortError() {
      return new AbortError(undefined, {
        cause: signal?.reason
      });
    }
    const fsIterable = watcher[SymbolAsyncIterator]();
    const result = {
      async next () {
        if (signal?.aborted) {
          cleanupAbort();
          throw abortError();
        }
        while(true){
          // deno-lint-ignore deno-internal/prefer-primordials
          const iterResult = await fsIterable.next();
          if (iterResult.done) {
            cleanupAbort();
            if (signal?.aborted) {
              throw abortError();
            }
            return iterResult;
          }
          const eventType = convertDenoFsEventToNodeFsEvent(iterResult.value.kind);
          const fname = recursive ? relative(resolvedWatchPath, iterResult.value.paths[0]) : basename(iterResult.value.paths[0]);
          if (ignoreMatcher !== null && ignoreMatcher(fname)) {
            continue;
          }
          return {
            value: {
              eventType,
              filename: fname
            },
            done: false
          };
        }
      },
      return (value) {
        cleanupAbort();
        watcher.close();
        return PromiseResolve({
          value,
          done: true
        });
      },
      [SymbolAsyncIterator] () {
        return this;
      }
    };
    return result;
  }
  function watchFile(filename, listenerOrOptions, listener) {
    // deno-lint-ignore deno-internal/prefer-primordials
    const watchPath = getValidatedPath(filename).toString();
    const handler = typeof listenerOrOptions === "function" ? listenerOrOptions : listener;
    validateFunction(handler, "listener");
    const { bigint = false, persistent = true, interval = 5007 } = typeof listenerOrOptions === "object" ? listenerOrOptions : {};
    let watcher = MapPrototypeGet(statWatchers, watchPath);
    if (watcher === undefined) {
      watcher = new StatWatcher(bigint);
      watcher[kFSStatWatcherStart](watchPath, persistent, interval);
      MapPrototypeSet(statWatchers, watchPath, watcher);
    }
    watcher.addListener("change", handler);
    return watcher;
  }
  function unwatchFile(filename, listener) {
    // deno-lint-ignore deno-internal/prefer-primordials
    const watchPath = getValidatedPath(filename).toString();
    const watcher = MapPrototypeGet(statWatchers, watchPath);
    if (!watcher) {
      return;
    }
    if (typeof listener === "function") {
      const beforeListenerCount = watcher.listenerCount("change");
      watcher.removeListener("change", listener);
      if (watcher.listenerCount("change") < beforeListenerCount) {
        watcher[kFSStatWatcherAddOrCleanRef]("clean");
      }
    } else {
      watcher.removeAllListeners("change");
      watcher[kFSStatWatcherAddOrCleanRef]("cleanAll");
    }
    if (watcher.listenerCount("change") === 0) {
      watcher.stop();
      MapPrototypeDelete(statWatchers, watchPath);
    }
  }
  const statWatchers = new SafeMap();
  const kFSStatWatcherStart = SymbolFor("kFSStatWatcherStart");
  const kFSStatWatcherAddOrCleanRef = SymbolFor("kFSStatWatcherAddOrCleanRef");
  class StatWatcher extends EventEmitter {
    #bigint;
    #refCount = 0;
    #abortController = new AbortController();
    #refed = true;
    // The current in-flight interval timer, ref'd / unref'd when ref()/unref()
    // is called between polls so the process can exit while nothing is changing.
    #timer = null;
    constructor(bigint){
      super();
      this.#bigint = bigint;
    }
    [kFSStatWatcherStart](filename, persistent, interval) {
      if (persistent) {
        this.#refCount++;
      }
      const bigint = this.#bigint;
      (async ()=>{
        let prev = await statAsync(filename, bigint);
        // libuv emits an initial "change" only when the first stat fails.
        if (prev === emptyStats || prev === emptyBigIntStats) {
          this.emit("change", prev, prev);
        }
        try {
          while(true){
            await this.#sleep(interval);
            const curr = await statAsync(filename, bigint);
            if (statsChanged(prev, curr)) {
              this.emit("change", curr, prev);
              prev = curr;
            }
          }
        } catch (e) {
          if (ObjectPrototypeIsPrototypeOf(DOMException.prototype, e) && e.name === "AbortError") {
            return;
          }
          this.emit("error", e);
        }
      })();
    }
    #sleep(ms) {
      return new Promise((resolve, reject)=>{
        const signal = this.#abortController.signal;
        if (signal.aborted) {
          reject(signal.reason);
          return;
        }
        const abort = ()=>{
          clearTimeout(timer);
          this.#timer = null;
          reject(signal.reason);
        };
        const done = ()=>{
          signal.removeEventListener("abort", abort);
          this.#timer = null;
          resolve();
        };
        const timer = setTimeout(done, ms);
        if (!this.#refed) {
          timer.unref();
        }
        this.#timer = timer;
        signal.addEventListener("abort", abort, {
          once: true
        });
      });
    }
    [kFSStatWatcherAddOrCleanRef](addOrClean) {
      if (addOrClean === "add") {
        this.#refCount++;
      } else if (addOrClean === "clean") {
        this.#refCount--;
      } else {
        this.#refCount = 0;
      }
    }
    stop() {
      if (this.#abortController.signal.aborted) {
        return;
      }
      this.#abortController.abort();
      // Match Node: stop fires asynchronously so listeners removed
      // synchronously after stop() are not called (see
      // StatWatcher.prototype.stop in lib/internal/fs/watchers.js).
      lazyProcess().default.nextTick(()=>this.emit("stop"));
    }
    // Node's ref/unref toggle whether the StatWatcher's internal handle keeps
    // the event loop alive (see lib/internal/fs/watchers.js). In Deno the
    // handle is the interval Timeout used between poll iterations, so we
    // ref/unref that.
    ref() {
      this.#refed = true;
      this.#timer?.ref();
      return this;
    }
    unref() {
      this.#refed = false;
      this.#timer?.unref();
      return this;
    }
  }
  class FSWatcher extends EventEmitter {
    #closer;
    #closed = false;
    #watcher;
    constructor(closer, getter){
      super();
      this.#closer = closer;
      this.#watcher = getter;
    }
    close() {
      if (this.#closed) {
        return;
      }
      this.#closed = true;
      this.emit("close");
      this.#closer();
    }
    ref() {
      this.#watcher()?.ref();
    }
    unref() {
      this.#watcher()?.unref();
    }
  }
  function convertDenoFsEventToNodeFsEvent(kind) {
    if (kind === "create" || kind === "remove") {
      return "rename";
    } else if (kind === "rename") {
      return "rename";
    } else {
      return "change";
    }
  }
  // Match Node: the public `fs.Stats` export is deprecated (DEP0180).
  // Internal call sites use the un-deprecated `Stats` directly (see
  // emptyStats above). See lib/internal/fs/utils.js `Stats: deprecate(...)`.
  const DeprecatedStats = deprecate(Stats, "fs.Stats constructor is deprecated.", "DEP0180");
  const fsExports = {
    // For tests
    _toUnixTimestamp,
    access,
    accessSync,
    appendFile,
    appendFileSync,
    BigIntStats,
    CFISBIS,
    chmod,
    chmodSync,
    chown,
    chownSync,
    close,
    closeSync,
    constants,
    convertFileInfoToBigIntStats,
    convertFileInfoToStats,
    copyFile,
    copyFileSync,
    cp,
    cpSync,
    get createReadStream () {
      return _createReadStream ?? (_createReadStream = lazyInternalStreams().createReadStream);
    },
    set createReadStream (v){
      _createReadStream = v;
    },
    get createWriteStream () {
      return _createWriteStream ?? (_createWriteStream = lazyInternalStreams().createWriteStream);
    },
    set createWriteStream (v){
      _createWriteStream = v;
    },
    Dir,
    Dirent,
    exists,
    existsSync,
    fchmod,
    fchmodSync,
    fchown,
    fchownSync,
    fdatasync,
    fdatasyncSync,
    fstat,
    fstatSync,
    fsync,
    fsyncSync,
    ftruncate,
    ftruncateSync,
    futimes,
    futimesSync,
    glob,
    globSync,
    lchmod,
    lchmodSync,
    lchown,
    lchownSync,
    link,
    linkSync,
    lstat,
    lstatSync,
    lutimes,
    lutimesSync,
    mkdir,
    mkdirSync,
    mkdtemp,
    mkdtempDisposableSync,
    mkdtempSync,
    open,
    openAsBlob,
    opendir,
    opendirSync,
    openSync,
    get promises () {
      return _promises ?? (_promises = lazyInternalPromises().default);
    },
    set promises (v){
      _promises = v;
    },
    read,
    readdir,
    readdirSync,
    readFile,
    readFilePromise,
    readFileSync,
    readlink,
    readlinkPromise,
    readlinkSync,
    get ReadStream () {
      return _ReadStream ?? (_ReadStream = lazyInternalStreams().ReadStream);
    },
    set ReadStream (v){
      _ReadStream = v;
    },
    readSync,
    readv,
    readvPromise,
    readvSync,
    realpath,
    realpathSync,
    rename,
    renameSync,
    rm,
    rmdir,
    rmdirSync,
    rmSync,
    stat,
    Stats: DeprecatedStats,
    statfs,
    statfsSync,
    statSync,
    symlink,
    symlinkSync,
    SyncWriteStream,
    truncate,
    truncateSync,
    unlink,
    unlinkSync,
    unwatchFile,
    get Utf8Stream () {
      return lazyUtf8Stream().default;
    },
    utimes,
    utimesSync,
    watch,
    watchFile,
    watchPromise,
    write,
    writeFile,
    writeFileSync,
    get WriteStream () {
      return _WriteStream ?? (_WriteStream = lazyInternalStreams().WriteStream);
    },
    set WriteStream (v){
      _WriteStream = v;
    },
    writeSync,
    writev,
    writevSync
  };
  // `writeFile`/`writeFileSync` call `fsExports.fsync`/`fsyncSync` (rather than
  // the local bindings) so the `flush` option honors monkey-patches/mocks made
  // on the `node:fs` namespace, matching Node's `lib/fs.js`.
  return fsExports;
})());