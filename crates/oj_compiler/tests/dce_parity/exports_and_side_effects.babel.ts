import "./effect";
import { unusedFromStart } from "./unused";
export function exported() {
  return 1;
}
let counter = 0;
function bump() {
  counter = counter + 1;
}
function deadAlready() {
  return 2;
}
export const f = keep(undefined);
bump();
