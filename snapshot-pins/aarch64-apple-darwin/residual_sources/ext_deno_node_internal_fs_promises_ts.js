// Copyright 2018-2026 the Deno authors. MIT license.
// deno-lint-ignore-file no-explicit-any
import { core } from "ext:core/mod.js";
const { promisify } = core.loadExtScript("ext:deno_node/internal/util.mjs");
const constants = core.loadExtScript("ext:deno_node/_fs/_fs_constants.ts");
import { copyFilePromise } from "ext:deno_node/_fs/_fs_copy.ts";
const { cpPromise } = core.loadExtScript("ext:deno_node/_fs/_fs_cp.ts");
import { lutimesPromise } from "ext:deno_node/_fs/_fs_lutimes.ts";
import { readdirPromise } from "ext:deno_node/_fs/_fs_readdir.ts";
const { lstatPromise } = core.loadExtScript("ext:deno_node/_fs/_fs_lstat.ts");
const lazyFs = core.createLazyLoader("node:fs");
import { globPromise } from "ext:deno_node/_fs/_fs_glob.ts";
import { getValidatedPathToString } from "ext:deno_node/internal/fs/utils.mjs";
import { FileHandle } from "ext:deno_node/internal/fs/handle.ts";
import { primordials } from "ext:core/mod.js";
const { parseFileMode } = core.loadExtScript("ext:deno_node/internal/validators.mjs");
import { op_node_lchmod } from "ext:core/ops";
const { isMacOS } = core.loadExtScript("ext:deno_node/_util/os.ts");
const { ERR_METHOD_NOT_IMPLEMENTED, aggregateTwoErrors } = core.loadExtScript("ext:deno_node/internal/errors.ts");
const lazyPath = core.createLazyLoader("node:path");
const lazyProcess = core.createLazyLoader("node:process");
const { ObjectPrototypeIsPrototypeOf, Promise, PromiseReject, SafeArrayIterator, SymbolAsyncDispose } = primordials;
// Promisified fs.X wrappers MUST NOT be built at module body. handle.ts /
// internal/fs/promises.ts are loaded during the initial `fs.promises`
// access, and calling `lazyFs()` here re-enters `node:fs`'s evaluating body
// (its `export const promises = mod.promises` line re-triggers `get
// promises` on fs.ts, whose lazyInternalPromises() then hits a TDZ on the
// in-flight `default` binding). Build wrappers lazily on first call.
const _promisifyCache = {
  __proto__: null
};
// `arity` is the max positional arg count the underlying fs callback method
// accepts (excluding the callback). Extra args are dropped so idiomatic
// patterns like `paths.map(fs.promises.unlink)` -- which Array#map invokes as
// `unlink(elem, index, array)` -- don't trip the promisify wrapper's appended
// callback (the wrapper would otherwise call `fs.unlink(path, index, array,
// cb)` and `fs.unlink` reads the second positional as the callback). Node's
// own `fs.promises.*` wrappers don't go through `util.promisify` and so don't
// have this issue.
function lazyPromisifyFs(name, arity) {
  return (...args)=>{
    let fn = _promisifyCache[name];
    if (fn === undefined) {
      fn = promisify(lazyFs()[name]);
      _promisifyCache[name] = fn;
    }
    if (args.length > arity) args.length = arity;
    return fn(...new SafeArrayIterator(args));
  };
}
// Mirrors Node's lib/internal/fs/promises.js handleFdClose(): run the file op,
// then close the FileHandle. Looks up `fh.close` lazily so tests that
// monkey-patch the prototype/instance close still take effect.
//   op ok, close ok       -> resolve(result)
//   op ok, close throws   -> throw closeError
//   op throws, close ok   -> throw opError
//   op throws, close throws -> throw AggregateError([opError, closeError])
async function handleFdClose(fileOpPromise, closeFunc) {
  let result;
  let opError;
  let opFailed = false;
  try {
    result = await fileOpPromise;
  } catch (err) {
    opError = err;
    opFailed = true;
  }
  try {
    await closeFunc();
  } catch (closeError) {
    if (opFailed) {
      // Mirrors Node's aggregateTwoErrors(): preserves opError.code on the
      // AggregateError so callers asserting err.code see the op's code.
      throw aggregateTwoErrors(closeError, opError);
    }
    throw closeError;
  }
  if (opFailed) {
    throw opError;
  }
  return result;
}
// -- access --
const accessPromise = lazyPromisifyFs("access", 2);
// -- appendFile --
// Delegates to writeFilePromise with an "a" flag, mirroring Node's
// lib/internal/fs/promises.js appendFile(). Per Node semantics, when given a
// FileHandle the existing flag stays in effect.
function appendFilePromise(path, data, options) {
  let opts;
  if (typeof options === "string") {
    opts = {
      encoding: options
    };
  } else if (options == null || typeof options !== "object") {
    opts = {};
  } else {
    opts = {
      ...options
    };
  }
  opts.flag = opts.flag || "a";
  return writeFilePromise(path, data, opts);
}
// -- chmod --
const chmodPromise = lazyPromisifyFs("chmod", 2);
// -- chown --
const chownPromise = lazyPromisifyFs("chown", 3);
const lchmodPromise = !isMacOS ? ()=>PromiseReject(new ERR_METHOD_NOT_IMPLEMENTED("lchmod()")) : async (path, mode)=>{
  path = getValidatedPathToString(path);
  mode = parseFileMode(mode, "mode");
  return await op_node_lchmod(path, mode);
};
const lchownPromise = lazyPromisifyFs("lchown", 3);
const linkPromise = lazyPromisifyFs("link", 2);
const unlinkPromise = lazyPromisifyFs("unlink", 1);
const renamePromise = lazyPromisifyFs("rename", 2);
const rmPromise = lazyPromisifyFs("rm", 2);
const rmdirPromise = lazyPromisifyFs("rmdir", 2);
const mkdirPromise = lazyPromisifyFs("mkdir", 2);
const mkdtempPromise = lazyPromisifyFs("mkdtemp", 2);
// Mirrors Node's lib/internal/fs/promises.js mkdtempDisposable(): create the
// temp dir, then return an object with .path, .remove(), and Symbol.asyncDispose
// that recursively removes the directory. Capture cwd at creation time so a
// later process.chdir() doesn't break removal.
async function mkdtempDisposablePromise(prefix, options) {
  const cwd = lazyProcess().default.cwd();
  const path = await mkdtempPromise(prefix, options);
  const fullPath = lazyPath().resolve(cwd, path);
  // `force: true` makes the second remove() a no-op when the dir is already
  // gone (Node's rimraf-based implementation treats ENOENT as success); other
  // errors (EACCES, EPERM, ...) still propagate.
  const remove = async ()=>{
    await rmPromise(fullPath, {
      force: true,
      maxRetries: 0,
      recursive: true,
      retryDelay: 0
    });
  };
  return {
    __proto__: null,
    path,
    remove,
    async [SymbolAsyncDispose] () {
      await remove();
    }
  };
}
function openPromise(path, flags = "r", mode = 0o666) {
  return new Promise((resolve, reject)=>{
    lazyFs().open(path, flags, mode, (err, fd)=>{
      if (err) reject(err);
      else resolve(new FileHandle(fd));
    });
  });
}
const opendirPromise = lazyPromisifyFs("opendir", 2);
// -- symlink --
const symlinkPromise = lazyPromisifyFs("symlink", 3);
// -- truncate --
// Mirrors Node's lib/internal/fs/promises.js truncate(): open the path as a
// FileHandle, delegate to its truncate method, then close via handleFdClose
// so callers that monkey-patch FileHandle still observe the fd access and
// AggregateError-on-double-failure semantics that Node tests rely on.
async function truncatePromise(path, len) {
  const fh = await openPromise(path, "r+");
  return handleFdClose(fh.truncate(len), ()=>fh.close());
}
// -- utimes --
const utimesPromise = lazyPromisifyFs("utimes", 3);
// -- writeFile --
// Low-level callback writeFile, used when we already have an fd/FileHandle
// (i.e. avoid recursing back through writeFilePromise via FileHandle.writeFile).
const rawWriteFilePromise = lazyPromisifyFs("writeFile", 3);
// Mirrors Node's lib/internal/fs/promises.js writeFile(): when given a path,
// open a FileHandle and delegate via handleFdClose so the close error
// semantics are observable; when given an fd/FileHandle, write directly.
function writeFilePromise(pathOrRid, data, options) {
  if (typeof pathOrRid === "number" || ObjectPrototypeIsPrototypeOf(FileHandle.prototype, pathOrRid)) {
    return rawWriteFilePromise(pathOrRid, data, options);
  }
  const opts = typeof options === "string" ? {
    encoding: options
  } : options ?? {};
  const flag = opts.flag ?? "w";
  const mode = opts.mode ?? 0o666;
  return (async ()=>{
    // Match the existing path-based behavior: surface the same `DOMException`
    // that `signal.throwIfAborted()` produces (the fd-based fallback would
    // throw Deno's `AbortError` instead). Inside the async IIFE so the throw
    // becomes a promise rejection, not a sync throw.
    if (opts.signal?.aborted) opts.signal.throwIfAborted();
    const fh = await openPromise(pathOrRid, flag, mode);
    return handleFdClose(fh.writeFile(data, opts), ()=>fh.close());
  })();
}
// -- realpath --
const realpathPromise = lazyPromisifyFs("realpath", 2);
// -- stat --
const statPromise = lazyPromisifyFs("stat", 2);
// -- statfs --
const statfsPromise = lazyPromisifyFs("statfs", 2);
// -- readFile / readlink --
// Low-level callback readFile, used when we already have an fd/FileHandle
// (i.e. avoid recursing back through readFilePromise via FileHandle.readFile).
const rawReadFilePromise = lazyPromisifyFs("readFile", 2);
// Mirrors Node's lib/internal/fs/promises.js readFile(): when given a path,
// open a FileHandle and delegate via handleFdClose so the close error
// semantics are observable; when given an fd/FileHandle, read directly.
function readFilePromise(path, options) {
  if (typeof path === "number" || ObjectPrototypeIsPrototypeOf(FileHandle.prototype, path)) {
    return rawReadFilePromise(path, options);
  }
  const opts = typeof options === "string" ? {
    encoding: options
  } : options ?? {};
  const flag = opts.flag ?? "r";
  return (async ()=>{
    // Match the existing path-based behavior: surface the same `DOMException`
    // that `signal.throwIfAborted()` produces (the fd-based fallback would
    // throw Deno's `AbortError` instead). Inside the async IIFE so the throw
    // becomes a promise rejection, not a sync throw.
    if (opts.signal?.aborted) opts.signal.throwIfAborted();
    const fh = await openPromise(path, flag);
    return handleFdClose(fh.readFile(opts), ()=>fh.close());
  })();
}
const readlinkPromise = lazyPromisifyFs("readlink", 2);
// -- promises object --
const promises = {
  access: accessPromise,
  constants,
  copyFile: copyFilePromise,
  cp: cpPromise,
  glob: globPromise,
  open: openPromise,
  opendir: opendirPromise,
  rename: renamePromise,
  truncate: truncatePromise,
  rm: rmPromise,
  rmdir: rmdirPromise,
  mkdir: mkdirPromise,
  readdir: readdirPromise,
  readlink: readlinkPromise,
  symlink: symlinkPromise,
  lstat: lstatPromise,
  stat: statPromise,
  statfs: statfsPromise,
  link: linkPromise,
  unlink: unlinkPromise,
  chmod: chmodPromise,
  lchmod: lchmodPromise,
  lchown: lchownPromise,
  chown: chownPromise,
  utimes: utimesPromise,
  lutimes: lutimesPromise,
  realpath: realpathPromise,
  mkdtemp: mkdtempPromise,
  mkdtempDisposable: mkdtempDisposablePromise,
  writeFile: writeFilePromise,
  appendFile: appendFilePromise,
  readFile: readFilePromise,
  watch: (...args)=>lazyFs().watchPromise(...new SafeArrayIterator(args))
};
export default promises;
export { constants, FileHandle, mkdirPromise, opendirPromise };
