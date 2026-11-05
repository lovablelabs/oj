import { createServerOnlyFn } from "@tanstack/react-start";
import { s } from "./m";
export function m() { const k = () => j(); const x = createServerOnlyFn(/*<*/() => k()/*>*/); const j = () => i(); const i = () => s; return x }
