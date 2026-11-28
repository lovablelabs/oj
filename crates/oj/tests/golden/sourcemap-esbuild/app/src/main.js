import { format } from "./format.js";
import "./main.css";

console.log(format("ready"));
document.body.addEventListener("click", () => {
  import("./panel.js").then((m) => console.log(m.panel(format("open"))));
});
