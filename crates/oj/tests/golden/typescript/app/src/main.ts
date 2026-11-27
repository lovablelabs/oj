import type { Greeting } from "./types";
import { Level, describe } from "./describe";
import { render } from "./view";

const greeting: Greeting = { text: "hello", level: Level.Loud };
const el = document.querySelector<HTMLElement>("#app")!;
el.textContent = describe(greeting);
el.append(render(greeting.text, 2));

export const enumValue = Level.Quiet satisfies Level;
