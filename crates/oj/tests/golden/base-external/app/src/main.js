import logo from "./logo.svg";
import "./main.css";

console.log(import.meta.env.BASE_URL, logo);
import("./extra.js").then((m) => m.extra());
