import { shared } from "./shared.js";
import "./main.css";

console.log(shared());
import("./tracking.js").then((m) => console.log(m.track("open")));
import("./analytics-panel.js").then((m) => console.log(m.panel()));
