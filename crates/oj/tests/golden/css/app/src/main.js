import "./global.css";
import styles from "./card.module.css";

const app = document.querySelector("#app");
app.className = styles.card;
app.textContent = styles.cardTitle;

app.addEventListener("click", async () => {
  await import("./extra.js");
});
