import data from "./data.json";
import { name, tags } from "./data.json";
import raw from "./data.json?raw";

console.log(data.version, name, tags.length, raw.length, data.nested.ok);
