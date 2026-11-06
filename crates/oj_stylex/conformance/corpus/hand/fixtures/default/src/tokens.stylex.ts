import * as stylex from "@stylexjs/stylex";

export const breakpoints = stylex.defineConsts({
  md: "@media (min-width: 768px)",
  lg: "@media (min-width: 1024px)",
});

export const spacing = stylex.defineConsts({ gutter: "16px" });
