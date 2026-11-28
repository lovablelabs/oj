import "./main.css";

document.body.addEventListener("click", () => import("./panel.js").then((m) => m.open()));
