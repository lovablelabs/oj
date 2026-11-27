import { channel, marker } from "virtual:build-info";
import note from "./release.note";
import { shared } from "./shared.js";
import "./main.css";

console.log(channel, marker, note, shared(), __BUILD_TAG__);
import("./tracking.js").then((m) => console.log(m.track("open")));
import("./analytics-panel.js").then((m) => console.log(m.panel()));
