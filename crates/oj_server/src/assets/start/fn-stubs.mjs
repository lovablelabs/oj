// SPDX-License-Identifier: MIT

const isServerRuntime = typeof document === "undefined";

function make(clientImpl, serverImpl) {
  const fn = (...args) => (isServerRuntime ? serverImpl : clientImpl)?.(...args);
  fn.server = (s) => make(clientImpl, s);
  fn.client = (c) => make(c, serverImpl);
  return fn;
}

export function createIsomorphicFn() {
  return make(undefined, undefined);
}
export const createServerOnlyFn =
  (fn) =>
  (...a) => {
    if (isServerRuntime) return fn(...a);
    throw new Error("createServerOnlyFn: called on the client");
  };
export const createClientOnlyFn =
  (fn) =>
  (...a) => {
    if (!isServerRuntime) return fn(...a);
    throw new Error("createClientOnlyFn: called on the server");
  };
