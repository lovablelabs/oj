const fileUrl = new URL("./big.txt", import.meta.url).href;
console.log(fileUrl);

const direct = new Worker(new URL("./direct.js", import.meta.url), { type: "module" });
direct.postMessage(3);
