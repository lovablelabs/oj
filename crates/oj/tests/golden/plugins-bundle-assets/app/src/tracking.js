import { shared } from "./shared.js";
import "./tracking.css";

export function track(event) {
  return `${shared()}:${event}`;
}
