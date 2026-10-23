import * as stylex from "@stylexjs/stylex";

export const tvars = stylex.defineVars({
  accent: "rebeccapurple",
  gap: "8px",
  radius: { default: "4px", "@media (min-width: 600px)": "8px" },
});
