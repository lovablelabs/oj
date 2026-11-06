import { s } from "./m";
export function f(k: number) {
  const h = s;
  outer: switch (k) { case 1: { const j = h; keep(/*<*/() => j/*>*/); break outer; } }
}
