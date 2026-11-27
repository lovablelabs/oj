import { format } from "../shared.js";
import { table } from "../table.js";
import { chart } from "../vendor/charts/index.js";
import "./dashboard.css";

export function render(messages) {
  return format(`${messages.dashboard}: ${table(["a", "b"])} ${chart([1, 2, 3])}`);
}
