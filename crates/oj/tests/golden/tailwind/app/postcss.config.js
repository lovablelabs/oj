import tailwindcss from "@tailwindcss/postcss";

// `:where(A > B)` → `:where(A) > :where(B)`, a postcss plugin of the app's
// own after Tailwind's, as an app tuning its selectors for the browser's
// ancestor filter writes one.
const WHERE_CHILD = /^:where\((.+?)\s*>\s*(:not\(:last-child\))\)$/;

const splitWhereChildSelectors = {
  postcssPlugin: "split-where-child-selectors",
  OnceExit(root) {
    root.walkRules((rule) => {
      const selectors = rule.selectors.map((selector) => {
        const match = WHERE_CHILD.exec(selector);
        return match ? `:where(${match[1]}) > :where(${match[2]})` : selector;
      });
      if (selectors.some((selector, i) => selector !== rule.selectors[i])) rule.selectors = selectors;
    });
  },
};

export default {
  plugins: [tailwindcss, splitWhereChildSelectors],
};
