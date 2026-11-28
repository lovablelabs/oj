import { used } from "./lib.js";

export const version = "1.0.0";
export function unusedExport() {
  return "never called";
}
export { used };

document.title = used();
