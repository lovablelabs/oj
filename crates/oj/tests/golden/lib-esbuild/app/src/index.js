import { helper } from "./helper.js";

export function greet(name) {
  const message = helper(name);
  return /* @__PURE__ */ message.toUpperCase();
}

export const kinds = ["a", "b"];
