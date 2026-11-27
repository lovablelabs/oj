import { shared } from "./shared.js";
import { marker } from "virtual:build-info";
import "./tracking.css";

export function track(event) {
  return `${shared()}:${event}:${marker}`;
}
