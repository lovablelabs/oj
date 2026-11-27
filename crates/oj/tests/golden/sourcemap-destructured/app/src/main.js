import "./global.css";
import styles from "./card.module.css";
import { greet } from "./greet.js";
import dot from "./dot.svg";

const app = document.querySelector("#app");
app.className = styles.card;
app.textContent = greet("golden") + dot;

app.addEventListener("click", async () => {
  const { lazy } = await import("./lazy.js");
  lazy();
});
