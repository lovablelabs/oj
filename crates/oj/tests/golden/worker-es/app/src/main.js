import Counter from "./counter.js?worker";
import InlineCounter from "./counter.js?worker&inline";

const worker = new Counter();
worker.postMessage(1);
const inline = new InlineCounter();
inline.postMessage(2);
