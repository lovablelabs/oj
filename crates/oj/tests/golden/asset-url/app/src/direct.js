import { add } from "./math.js";

self.onmessage = (event) => {
  self.postMessage(add(event.data, 100));
};
