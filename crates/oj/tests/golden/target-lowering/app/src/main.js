import { Store } from "./store.js";

const store = new Store();
store.value ??= 3;
const label = store.config?.label ?? "none";
const { a, ...rest } = { a: 1, b: 2, c: 3 };
console.log(label, store.read(), rest, a, Store.count);
