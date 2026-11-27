import { add } from "./math.js";

let total = 0;
self.onmessage = (event) => {
  total = add(total, event.data);
  self.postMessage(total);
};
