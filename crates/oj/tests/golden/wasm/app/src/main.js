import init from "./add.wasm?init";
import wasmUrl from "./add.wasm?url";

init().then((instance) => console.log(instance.exports.add(2, 3), wasmUrl));
