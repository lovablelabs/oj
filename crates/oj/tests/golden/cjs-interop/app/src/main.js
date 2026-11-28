import legacy, { badge, LABEL } from "legacy-cjs";
import deep from "legacy-cjs/deep";
import * as ns from "legacy-cjs";

console.log(badge("a"), LABEL, legacy.badge("b"), deep("c"), Object.keys(ns).sort());
import("legacy-cjs").then((m) => console.log(m.default.LABEL, m.badge("lazy")));
