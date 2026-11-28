import { helper } from "./helper.js";

export function lib(value) {
  return helper(value) * 2;
}

export { helper };
