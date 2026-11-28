import "./zeta.css";
import { panel } from "./panel.js";
import "./alpha.css";

panel();
import("./late.js").then((m) => m.late());
