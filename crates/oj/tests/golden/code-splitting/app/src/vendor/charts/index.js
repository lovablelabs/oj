import { scale } from "./scale.js";

export function chart(values) {
  return values.map(scale).join(" ");
}
