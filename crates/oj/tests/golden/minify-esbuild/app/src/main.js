import { greet } from "./greet.js";

document.querySelector("#app").textContent = greet("golden");

document.querySelector("#app").addEventListener("click", async () => {
  const { lazy } = await import("./lazy.js");
  lazy();
});
