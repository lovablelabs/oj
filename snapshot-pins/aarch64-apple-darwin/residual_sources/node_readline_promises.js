// Copyright 2018-2026 the Deno authors. MIT license.
// Copyright Joyent and Node contributors. All rights reserved. MIT license.
import { core, primordials } from "ext:core/mod.js";
const { Promise } = primordials;
const { Readline } = core.loadExtScript("ext:deno_node/internal/readline/promises.mjs");
const { Interface: _Interface, kQuestion, kQuestionCancel } = core.loadExtScript("ext:deno_node/internal/readline/interface.mjs");
const { AbortError } = core.loadExtScript("ext:deno_node/internal/errors.ts");
const { validateAbortSignal } = core.loadExtScript("ext:deno_node/internal/validators.mjs");
const { kEmptyObject } = core.loadExtScript("ext:deno_node/internal/util.mjs");
export class Interface extends _Interface {
  constructor(input, output, completer, terminal){
    super(input, output, completer, terminal);
  }
  question(query, options = kEmptyObject) {
    return new Promise((resolve, reject)=>{
      let cb = resolve;
      if (options?.signal) {
        validateAbortSignal(options.signal, "options.signal");
        if (options.signal.aborted) {
          return reject(new AbortError(undefined, {
            cause: options.signal.reason
          }));
        }
        const onAbort = ()=>{
          this[kQuestionCancel]();
          reject(new AbortError(undefined, {
            cause: options.signal.reason
          }));
        };
        options.signal.addEventListener("abort", onAbort, {
          once: true
        });
        cb = (answer)=>{
          options.signal.removeEventListener("abort", onAbort);
          resolve(answer);
        };
      }
      this[kQuestion](query, cb);
    });
  }
}
export function createInterface(inputOrOptions, output, completer, terminal) {
  return new Interface(inputOrOptions, output, completer, terminal);
}
export { Readline };
export default {
  Interface,
  Readline,
  createInterface
};
