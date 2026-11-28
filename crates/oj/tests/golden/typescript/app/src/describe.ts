import type { Greeting } from "./types";

export enum Level {
  Quiet = "quiet",
  Loud = "loud",
}

export function describe(g: Greeting): string {
  const upper = g.level === Level.Loud ? g.text.toUpperCase() : g.text;
  return `${upper} (${g.level})`;
}
