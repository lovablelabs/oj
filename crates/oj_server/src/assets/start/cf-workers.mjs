// SPDX-License-Identifier: MIT
//
// Dev-only stub for the `cloudflare:workers` runtime module. In the worker
// build `cloudflare:*` stays external and workerd provides it (see
// crates/oj/src/start_bundle/cloudflare.rs); in oj's Node SSR loader (server functions, prewarm renders)
// there is no workerd, so Node's default ESM loader would throw
// ERR_UNSUPPORTED_ESM_URL_SCHEME on the `cloudflare:` scheme and crash the dev
// server. This stub keeps the scheme resolvable: `env` is backed by the same
// wrangler vars cf-server.mjs reads, and the runtime classes/helpers are
// minimal shims so a server function that only reads `env` runs unchanged.
import { cloudflareEnv } from "./cf-server.mjs";

export const env = cloudflareEnv();

// The RPC/entrypoint base classes: shims that keep `class X extends
// WorkerEntrypoint` and `new RpcStub()` from throwing when SSR-imported. They
// carry no workerd behavior (there is no worker here), only the shape.
export class WorkerEntrypoint {
  constructor(ctx, env) {
    this.ctx = ctx;
    this.env = env;
  }
}
export class DurableObject {
  constructor(ctx, env) {
    this.ctx = ctx;
    this.env = env;
  }
}
export class WorkflowEntrypoint {
  constructor(ctx, env) {
    this.ctx = ctx;
    this.env = env;
  }
}
export class RpcTarget {}
export class RpcStub {}

export function waitUntil() {}
export function withEnv(_env, fn) {
  return fn();
}

export default {
  env,
  WorkerEntrypoint,
  DurableObject,
  WorkflowEntrypoint,
  RpcTarget,
  RpcStub,
  waitUntil,
  withEnv,
};
