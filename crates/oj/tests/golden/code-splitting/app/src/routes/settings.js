import { format } from "../shared.js";
import { table } from "../table.js";
import { today } from "../vendor/dates/index.js";

export function render(messages) {
  return format(`${messages.settings}: ${table(["x"])} ${today()}`);
}
