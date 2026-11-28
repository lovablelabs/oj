import "./main.css";
import { shared } from "./shared.js";

console.log(shared);
document.body.addEventListener("click", async () => {
  const { page } = await import("./page.js");
  page();
});
