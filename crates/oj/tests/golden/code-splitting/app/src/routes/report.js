import { chart } from "../vendor/charts/index.js";
import "./report.css";

export function render(messages) {
  return `${messages.report}: ${chart([4, 5])}`;
}
