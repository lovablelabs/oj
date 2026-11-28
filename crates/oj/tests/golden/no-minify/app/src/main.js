import { shout } from "./shout.js";
import "./main.css";

// A comment the build keeps.
const greeting = shout("hello");
console.log(greeting);
import("./lazy.js").then((m) => console.log(m.lazy(greeting)));
