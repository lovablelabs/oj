import { format } from "./format.js";
import "./panel.css";

export function panel(label) {
  return `${format("panel")} ${label}`;
}
