// Copyright 2018-2026 the Deno authors. MIT license.
import { core, primordials } from "ext:core/mod.js";
import { direntFromDeno } from "ext:deno_node/internal/fs/utils.mjs";
const { default: assert } = core.loadExtScript("ext:deno_node/assert.ts");
const { ERR_MISSING_ARGS, ERR_DIR_CLOSED } = core.loadExtScript("ext:deno_node/internal/errors.ts");
const { TextDecoder } = core.loadExtScript("ext:deno_web/08_text_encoding.js");
const { Promise, ReflectApply, ObjectPrototypeIsPrototypeOf, Uint8ArrayPrototype, PromisePrototypeThen, SymbolAsyncIterator, SymbolAsyncDispose, SymbolDispose, ArrayIteratorPrototypeNext, SymbolIterator } = primordials;
// Note: unlike `fs.readdir`, `fs.opendir`/`Dir` streams entries in filesystem
// order and does NOT sort them, matching Node.js (libuv's `uv_fs_readdir`, as
// opposed to `uv_fs_scandir` which backs `readdir`). Do not add sorting here.
export default class Dir {
  #dirPath;
  #syncIterator;
  #asyncIterator;
  #closed = false;
  constructor(path){
    if (!path) {
      throw new ERR_MISSING_ARGS("path");
    }
    this.#dirPath = path;
  }
  get path() {
    if (ObjectPrototypeIsPrototypeOf(Uint8ArrayPrototype, this.#dirPath)) {
      return new TextDecoder().decode(this.#dirPath);
    }
    return this.#dirPath;
  }
  // deno-lint-ignore no-explicit-any
  read(callback) {
    return new Promise((resolve, reject)=>{
      if (this.#closed) {
        const err = new ERR_DIR_CLOSED();
        if (callback) {
          callback(err);
          resolve(null);
        } else {
          reject(err);
        }
        return;
      }
      if (!this.#asyncIterator) {
        this.#asyncIterator = Deno.readDir(this.path)[SymbolAsyncIterator]();
      }
      assert(this.#asyncIterator);
      PromisePrototypeThen(ReflectApply(this.#asyncIterator.next, this.#asyncIterator, []), (iteratorResult)=>{
        resolve(iteratorResult.done ? null : direntFromDeno(iteratorResult.value, this.#dirPath));
        if (callback) {
          callback(null, iteratorResult.done ? null : direntFromDeno(iteratorResult.value, this.#dirPath));
        }
      }, (err)=>{
        if (callback) {
          callback(err);
        }
        reject(err);
      });
    });
  }
  readSync() {
    if (this.#closed) {
      throw new ERR_DIR_CLOSED();
    }
    if (!this.#syncIterator) {
      this.#syncIterator = Deno.readDirSync(this.path)[SymbolIterator]();
    }
    const iteratorResult = ArrayIteratorPrototypeNext(this.#syncIterator);
    if (iteratorResult.done) {
      return null;
    } else {
      return direntFromDeno(iteratorResult.value, this.#dirPath);
    }
  }
  /**
   * Unlike Node, Deno does not require managing resource ids for reading
   * directories, and therefore does not need to close directories when
   * finished reading.
   */ // deno-lint-ignore no-explicit-any
  close(callback) {
    return new Promise((resolve)=>{
      this.#closed = true;
      if (callback) {
        callback(null);
      }
      resolve();
    });
  }
  /**
   * Unlike Node, Deno does not require managing resource ids for reading
   * directories, and therefore does not need to close directories when
   * finished reading
   */ closeSync() {
    this.#closed = true;
  }
  [SymbolDispose]() {
    this.closeSync();
  }
  [SymbolAsyncDispose]() {
    return this.close();
  }
  async *[SymbolAsyncIterator]() {
    try {
      while(true){
        const dirent = await this.read();
        if (dirent === null) {
          break;
        }
        yield dirent;
      }
    } finally{
      await this.close();
    }
  }
}
