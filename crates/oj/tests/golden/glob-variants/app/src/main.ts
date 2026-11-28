// Every import.meta.glob form an app reaches for: a deep lazy glob over two
// extensions, a negative pattern, eager default imports, a named import, raw
// and url queries, and a glob keyed from the root.
const lazyPages = import.meta.glob("./pages/**/*.{ts,tsx}");
const notNested = import.meta.glob(["./pages/*.ts", "!./pages/skip.ts"]);
const eagerDefaults = import.meta.glob("./pages/*.ts", { eager: true, import: "default" });
const titles = import.meta.glob("./pages/**/*.ts", { eager: true, import: "title" });
const texts = import.meta.glob("./text/*.md", { query: "?raw", import: "default", eager: true });
const urls = import.meta.glob("./text/*.md", { query: "?url", import: "default" });
const fromRoot = import.meta.glob("/src/pages/nested/*.ts", { base: "/src/pages" });

console.log(
  Object.keys(lazyPages),
  Object.keys(notNested),
  eagerDefaults,
  titles,
  texts,
  Object.keys(urls),
  Object.keys(fromRoot),
);
for (const load of Object.values(lazyPages)) load().then((m) => console.log((m as { title: string }).title));
for (const load of Object.values(urls)) load().then((url) => console.log(url));
