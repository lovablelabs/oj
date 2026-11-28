import lib from "some-cdn-lib";
import { thing } from "@scope/pkg/sub";
import remote from "https://esm.example.com/remote.js";

console.log(lib, thing, remote);
import("some-cdn-lib").then((m) => console.log(m.default));
