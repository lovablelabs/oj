import { shared } from "./shared.js";
import "./main.css";

console.log(shared("main"));
import("./lazy.js").then((m) => console.log(m.lazy()));
