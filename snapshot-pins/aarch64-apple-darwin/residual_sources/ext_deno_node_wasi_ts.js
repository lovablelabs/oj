"use strict"; return ((function() {
  const { core, primordials } = __bootstrap;
  const { WasiContext } = core.ops;
  const { statSync } = core.loadExtScript("ext:deno_node/fs.ts");
  const { exit } = core.loadExtScript("ext:deno_os/30_os.js");
  const { ERR_INVALID_ARG_TYPE, ERR_INVALID_ARG_VALUE, ERR_WASI_ALREADY_STARTED, ERR_WASI_NOT_STARTED } = core.loadExtScript("ext:deno_node/internal/errors.ts");
  let warningEmitted = false;
  function emitExperimentalWarning() {
    if (warningEmitted) return;
    warningEmitted = true;
    // Match Node's "WASI is an experimental feature ..." warning. The
    // node_compat test runner asserts on this exact message via
    // common.expectWarning('ExperimentalWarning', ...).
    // deno-lint-ignore no-explicit-any
    const proc = globalThis.process;
    if (proc && typeof proc.emitWarning === "function") {
      proc.emitWarning("WASI is an experimental feature and might change at any time", "ExperimentalWarning");
    }
  }
  const { ArrayPrototypeMap, ArrayPrototypePush, ArrayIsArray, Error, NumberIsInteger, ObjectEntries, ObjectPrototypeIsPrototypeOf, ObjectPrototypeToString, SafeArrayIterator, String, TypeError, Uint8Array } = primordials;
  // UVWASI error for path not found
  class UVWASIError extends Error {
    code;
    constructor(code, message){
      super(message);
      this.code = code;
      this.name = "Error";
    }
  }
  // Check if value is a WebAssembly.Memory, works across VM contexts
  function isWasmMemory(value) {
    // instanceof fails across VM contexts, use Object.prototype.toString
    return ObjectPrototypeToString(value) === "[object WebAssembly.Memory]";
  }
  // Custom TypeError with ERR_INVALID_ARG_TYPE code for memory validation.
  // Node uses native THROW_ERR_INVALID_ARG_TYPE which bypasses the JS formatter.
  function createMemoryTypeError(actual) {
    const received = actual === undefined ? "undefined" : actual === null ? "null" : `type ${typeof actual}`;
    const err = new TypeError(`The "instance.exports.memory" property must be a WebAssembly.Memory object. Received ${received}`);
    err.code = "ERR_INVALID_ARG_TYPE";
    return err;
  }
  class WASIProcExit {
    code;
    constructor(code){
      this.code = code;
    }
  }
  function validateObject(value, name) {
    if (value === null || typeof value !== "object" || ArrayIsArray(value)) {
      throw new ERR_INVALID_ARG_TYPE(name, "object", value);
    }
  }
  function validateArray(value, name) {
    if (!ArrayIsArray(value)) {
      throw new ERR_INVALID_ARG_TYPE(name, "Array", value);
    }
  }
  function validateBoolean(value, name) {
    if (typeof value !== "boolean") {
      throw new ERR_INVALID_ARG_TYPE(name, "boolean", value);
    }
  }
  function validateInt32(value, name) {
    if (!NumberIsInteger(value)) {
      throw new ERR_INVALID_ARG_TYPE(name, "int32", value);
    }
  }
  function validateString(value, name) {
    if (typeof value !== "string") {
      throw new ERR_INVALID_ARG_TYPE(name, "string", value);
    }
  }
  // WASI preopens use direct host filesystem access in the native ops. They do
  // not read from the in-memory VFS that `deno compile` uses for embedded files.
  class WASI {
    #ctx;
    #version;
    #started = false;
    #returnOnExit;
    #wasiImport;
    constructor(options){
      emitExperimentalWarning();
      if (options === undefined) {
        throw new ERR_INVALID_ARG_TYPE("options.version", "string", undefined);
      }
      validateObject(options, "options");
      if (options.version === undefined) {
        throw new ERR_INVALID_ARG_TYPE("options.version", "string", undefined);
      }
      validateString(options.version, "options.version");
      if (options.version !== "preview1" && options.version !== "unstable") {
        throw new ERR_INVALID_ARG_VALUE("options.version", options.version, 'must be "preview1" or "unstable"');
      }
      const argsValue = options.args ?? [];
      if (options.args !== undefined) {
        validateArray(options.args, "options.args");
      }
      const args = ArrayPrototypeMap(argsValue, (arg)=>String(arg));
      const envObj = options.env ?? {};
      if (options.env !== undefined) {
        validateObject(options.env, "options.env");
      }
      const envPairs = [];
      for (const entry of new SafeArrayIterator(ObjectEntries(envObj))){
        const key = entry[0];
        const value = entry[1];
        ArrayPrototypePush(envPairs, [
          key,
          String(value)
        ]);
      }
      if (options.preopens !== undefined) {
        validateObject(options.preopens, "options.preopens");
      }
      const preopens = [];
      if (options.preopens) {
        for (const entry of new SafeArrayIterator(ObjectEntries(options.preopens))){
          const virtualPath = entry[0];
          const realPath = entry[1];
          const realPathString = String(realPath);
          try {
            statSync(realPathString);
          } catch  {
            throw new UVWASIError("UVWASI_ENOENT", `uvwasi_init: failed to open preopen "${realPathString}"`);
          }
          ArrayPrototypePush(preopens, [
            String(virtualPath),
            realPathString
          ]);
        }
      }
      if (options.returnOnExit !== undefined) {
        validateBoolean(options.returnOnExit, "options.returnOnExit");
      }
      if (options.stdin !== undefined) {
        validateInt32(options.stdin, "options.stdin");
      }
      if (options.stdout !== undefined) {
        validateInt32(options.stdout, "options.stdout");
      }
      if (options.stderr !== undefined) {
        validateInt32(options.stderr, "options.stderr");
      }
      const stdinFd = options.stdin ?? 0;
      const stdoutFd = options.stdout ?? 1;
      const stderrFd = options.stderr ?? 2;
      this.#returnOnExit = options.returnOnExit ?? true;
      this.#version = options.version;
      this.#ctx = new WasiContext(args, envPairs, preopens, stdinFd, stdoutFd, stderrFd, this.#returnOnExit);
      // Build the wasiImport object. Each function delegates to the cppgc object
      // method, passing the wasm memory buffer.
      const ctx = this.#ctx;
      const self = this;
      this.#wasiImport = {
        args_get (argv, argvBuf) {
          return ctx.argsGet(argv, argvBuf, self.#getMemoryBuffer());
        },
        args_sizes_get (argc, argvBufSize) {
          return ctx.argsSizesGet(argc, argvBufSize, self.#getMemoryBuffer());
        },
        environ_get (environ, environBuf) {
          return ctx.environGet(environ, environBuf, self.#getMemoryBuffer());
        },
        environ_sizes_get (environCount, environBufSize) {
          return ctx.environSizesGet(environCount, environBufSize, self.#getMemoryBuffer());
        },
        clock_res_get (clockId, resolution) {
          return ctx.clockResGet(clockId, resolution, self.#getMemoryBuffer());
        },
        clock_time_get (clockId, precision, time) {
          return ctx.clockTimeGet(clockId, precision, time, self.#getMemoryBuffer());
        },
        random_get (bufPtr, bufLen) {
          return ctx.randomGet(bufPtr, bufLen, self.#getMemoryBuffer());
        },
        proc_exit (code) {
          const exitCode = ctx.procExit(code);
          if (self.#returnOnExit) {
            throw new WASIProcExit(exitCode);
          }
          exit(exitCode);
        },
        proc_raise (sig) {
          return ctx.procRaise(sig);
        },
        fd_write (fd, iovsPtr, iovsLen, nwrittenPtr) {
          return ctx.fdWrite(fd, iovsPtr, iovsLen, nwrittenPtr, self.#getMemoryBuffer());
        },
        fd_read (fd, iovsPtr, iovsLen, nreadPtr) {
          return ctx.fdRead(fd, iovsPtr, iovsLen, nreadPtr, self.#getMemoryBuffer());
        },
        fd_seek (fd, offset, whence, newoffsetPtr) {
          return ctx.fdSeek(fd, offset, whence, newoffsetPtr, self.#getMemoryBuffer());
        },
        fd_close (fd) {
          return ctx.fdClose(fd);
        },
        fd_fdstat_get (fd, fdstatPtr) {
          return ctx.fdFdstatGet(fd, fdstatPtr, self.#getMemoryBuffer());
        },
        fd_fdstat_set_flags (fd, flags) {
          return ctx.fdFdstatSetFlags(fd, flags);
        },
        fd_fdstat_set_rights (fd, fsRightsBase, fsRightsInheriting) {
          return ctx.fdFdstatSetRights(fd, fsRightsBase, fsRightsInheriting);
        },
        fd_prestat_get (fd, prestatPtr) {
          return ctx.fdPrestatGet(fd, prestatPtr, self.#getMemoryBuffer());
        },
        fd_prestat_dir_name (fd, pathPtr, pathLen) {
          return ctx.fdPrestatDirName(fd, pathPtr, pathLen, self.#getMemoryBuffer());
        },
        fd_tell (fd, offsetPtr) {
          return ctx.fdTell(fd, offsetPtr, self.#getMemoryBuffer());
        },
        fd_sync (fd) {
          return ctx.fdSync(fd);
        },
        fd_datasync (fd) {
          return ctx.fdDatasync(fd);
        },
        fd_advise (fd, offset, len, advice) {
          return ctx.fdAdvise(fd, offset, len, advice);
        },
        fd_allocate (fd, offset, len) {
          return ctx.fdAllocate(fd, offset, len);
        },
        fd_filestat_get (fd, filestatPtr) {
          return ctx.fdFilestatGet(fd, filestatPtr, self.#getMemoryBuffer());
        },
        fd_filestat_set_size (fd, size) {
          return ctx.fdFilestatSetSize(fd, size);
        },
        fd_filestat_set_times (fd, atim, mtim, fstFlags) {
          return ctx.fdFilestatSetTimes(fd, atim, mtim, fstFlags);
        },
        fd_renumber (from, to) {
          return ctx.fdRenumber(from, to);
        },
        fd_readdir (fd, bufPtr, bufLen, cookie, bufusedPtr) {
          return ctx.fdReaddir(fd, bufPtr, bufLen, cookie, bufusedPtr, self.#getMemoryBuffer());
        },
        fd_pread (fd, iovsPtr, iovsLen, offset, nreadPtr) {
          return ctx.fdPread(fd, iovsPtr, iovsLen, offset, nreadPtr, self.#getMemoryBuffer());
        },
        fd_pwrite (fd, iovsPtr, iovsLen, offset, nwrittenPtr) {
          return ctx.fdPwrite(fd, iovsPtr, iovsLen, offset, nwrittenPtr, self.#getMemoryBuffer());
        },
        path_open (dirfd, dirflags, pathPtr, pathLen, oflags, fsRightsBase, fsRightsInheriting, fdflags, fdPtr) {
          return ctx.pathOpen(dirfd, dirflags, pathPtr, pathLen, oflags, fsRightsBase, fsRightsInheriting, fdflags, fdPtr, self.#getMemoryBuffer());
        },
        path_create_directory (dirfd, pathPtr, pathLen) {
          return ctx.pathCreateDirectory(dirfd, pathPtr, pathLen, self.#getMemoryBuffer());
        },
        path_remove_directory (dirfd, pathPtr, pathLen) {
          return ctx.pathRemoveDirectory(dirfd, pathPtr, pathLen, self.#getMemoryBuffer());
        },
        path_unlink_file (dirfd, pathPtr, pathLen) {
          return ctx.pathUnlinkFile(dirfd, pathPtr, pathLen, self.#getMemoryBuffer());
        },
        path_rename (oldDirfd, oldPathPtr, oldPathLen, newDirfd, newPathPtr, newPathLen) {
          return ctx.pathRename(oldDirfd, oldPathPtr, oldPathLen, newDirfd, newPathPtr, newPathLen, self.#getMemoryBuffer());
        },
        path_filestat_get (dirfd, flags, pathPtr, pathLen, filestatPtr) {
          return ctx.pathFilestatGet(dirfd, flags, pathPtr, pathLen, filestatPtr, self.#getMemoryBuffer());
        },
        path_readlink (dirfd, pathPtr, pathLen, bufPtr, bufLen, bufusedPtr) {
          return ctx.pathReadlink(dirfd, pathPtr, pathLen, bufPtr, bufLen, bufusedPtr, self.#getMemoryBuffer());
        },
        path_symlink (oldPathPtr, oldPathLen, dirfd, newPathPtr, newPathLen) {
          return ctx.pathSymlink(oldPathPtr, oldPathLen, dirfd, newPathPtr, newPathLen, self.#getMemoryBuffer());
        },
        path_link (oldDirfd, oldFlags, oldPathPtr, oldPathLen, newDirfd, newPathPtr, newPathLen) {
          return ctx.pathLink(oldDirfd, oldFlags, oldPathPtr, oldPathLen, newDirfd, newPathPtr, newPathLen, self.#getMemoryBuffer());
        },
        path_filestat_set_times (dirfd, flags, pathPtr, pathLen, atim, mtim, fstFlags) {
          return ctx.pathFilestatSetTimes(dirfd, flags, pathPtr, pathLen, atim, mtim, fstFlags, self.#getMemoryBuffer());
        },
        poll_oneoff (inPtr, outPtr, nsubscriptions, neventsPtr) {
          return ctx.pollOneoff(inPtr, outPtr, nsubscriptions, neventsPtr, self.#getMemoryBuffer());
        },
        sched_yield () {
          return ctx.schedYield();
        },
        sock_recv (fd, riDataPtr, riDataLen, riFlags, roDatalenPtr, roFlagsPtr) {
          return ctx.sockRecv(fd, riDataPtr, riDataLen, riFlags, roDatalenPtr, roFlagsPtr, self.#getMemoryBuffer());
        },
        sock_send (fd, siDataPtr, siDataLen, siFlags, soDatalenPtr) {
          return ctx.sockSend(fd, siDataPtr, siDataLen, siFlags, soDatalenPtr, self.#getMemoryBuffer());
        },
        sock_shutdown (fd, how) {
          return ctx.sockShutdown(fd, how);
        },
        sock_accept (fd, flags, fdPtr) {
          return ctx.sockAccept(fd, flags, fdPtr, self.#getMemoryBuffer());
        }
      };
    }
    #memory = null;
    #getMemoryBuffer() {
      if (!this.#memory) {
        throw new ERR_WASI_NOT_STARTED();
      }
      // deno-lint-ignore deno-internal/prefer-primordials -- WebAssembly.Memory.prototype.buffer getter; no primordial equivalent
      return new Uint8Array(this.#memory.buffer);
    }
    get wasiImport() {
      return this.#wasiImport;
    }
    getImportObject() {
      if (this.#version === "unstable") {
        return {
          wasi_unstable: this.#wasiImport
        };
      }
      return {
        wasi_snapshot_preview1: this.#wasiImport
      };
    }
    start(instance) {
      if (this.#started) {
        throw new ERR_WASI_ALREADY_STARTED();
      }
      if (instance === undefined || instance === null) {
        throw new ERR_INVALID_ARG_TYPE("instance", "object", instance);
      }
      if (typeof instance !== "object") {
        throw new ERR_INVALID_ARG_TYPE("instance", "object", instance);
      }
      const exports = instance.exports;
      if (exports === null || typeof exports !== "object") {
        throw new ERR_INVALID_ARG_TYPE("instance.exports", "object", exports);
      }
      if (typeof exports._start !== "function") {
        throw new ERR_INVALID_ARG_TYPE("instance.exports._start", "function", exports._start);
      }
      if (exports._initialize !== undefined) {
        throw new ERR_INVALID_ARG_TYPE("instance.exports._initialize", "undefined", exports._initialize);
      }
      if (!isWasmMemory(exports.memory)) {
        throw createMemoryTypeError(exports.memory);
      }
      this.#memory = exports.memory;
      this.#started = true;
      try {
        exports._start();
      } catch (e) {
        if (ObjectPrototypeIsPrototypeOf(WASIProcExit.prototype, e)) {
          return e.code;
        }
        throw e;
      }
      return 0;
    }
    // Node.js exposes finalizeBindings() so worker threads spawned via
    // `wasi.thread-spawn` can bind a wasm instance that shares the main
    // thread's WASI state (the existing #ctx) to an external WebAssembly.Memory.
    // We don't manage thread bookkeeping ourselves - that lives in the user's
    // thread-spawn shim - but we must accept the call so the user's bindings
    // can call wasi_thread_start() in the worker.
    finalizeBindings(instance, options) {
      if (instance === undefined || instance === null) {
        throw new ERR_INVALID_ARG_TYPE("instance", "object", instance);
      }
      if (typeof instance !== "object") {
        throw new ERR_INVALID_ARG_TYPE("instance", "object", instance);
      }
      const exports = instance.exports;
      if (exports === null || typeof exports !== "object") {
        throw new ERR_INVALID_ARG_TYPE("instance.exports", "object", exports);
      }
      const memory = options?.memory ?? exports.memory;
      if (!isWasmMemory(memory)) {
        throw createMemoryTypeError(memory);
      }
      this.#memory = memory;
      // finalizeBindings is intentionally idempotent: the same WASI instance
      // can be bound to multiple wasm instances across threads.
      //
      // Setting #started here is a one-way transition shared with start()
      // and initialize(): once finalizeBindings has been called on this
      // instance (typically from the thread-spawn worker path), a later
      // start()/initialize() call on the same WASI object will throw
      // ERR_WASI_ALREADY_STARTED. Node's wasi_thread_start path has the
      // same behavior - re-entering start/initialize on a bound WASI is
      // never valid, the spawning thread is expected to construct a fresh
      // WASI per wasm module.
      this.#started = true;
    }
    initialize(instance) {
      if (this.#started) {
        throw new ERR_WASI_ALREADY_STARTED();
      }
      if (instance === undefined || instance === null) {
        throw new ERR_INVALID_ARG_TYPE("instance", "object", instance);
      }
      if (typeof instance !== "object") {
        throw new ERR_INVALID_ARG_TYPE("instance", "object", instance);
      }
      const exports = instance.exports;
      if (exports === null || typeof exports !== "object") {
        throw new ERR_INVALID_ARG_TYPE("instance.exports", "object", exports);
      }
      if (exports._initialize !== undefined && typeof exports._initialize !== "function") {
        throw new ERR_INVALID_ARG_TYPE("instance.exports._initialize", "function", exports._initialize);
      }
      if (exports._start !== undefined) {
        throw new ERR_INVALID_ARG_TYPE("instance.exports._start", "undefined", exports._start);
      }
      if (!isWasmMemory(exports.memory)) {
        throw createMemoryTypeError(exports.memory);
      }
      this.#memory = exports.memory;
      this.#started = true;
      if (typeof exports._initialize === "function") {
        exports._initialize();
      }
    }
  }
  return {
    default: {
      WASI
    },
    WASI
  };
})());