import { createServerOnlyFn } from "@tanstack/react-start";
import { s } from "./m";
export default function () { const h = s; return createServerOnlyFn(/*<*/() => h/*>*/) }
