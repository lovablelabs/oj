// node bench/generate.mjs 1000
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const N = parseInt(process.argv[2] ?? "1000", 10);
const FANOUT = 10;
const here = path.dirname(fileURLToPath(import.meta.url));
const dir = path.join(here, "apps", `app-${N}`);
const src = path.join(dir, "src", "components");

fs.rmSync(path.join(dir, "src"), { recursive: true, force: true });
fs.mkdirSync(src, { recursive: true });

const children = (i) => {
  const out = [];
  for (let c = i * FANOUT + 1; c <= i * FANOUT + FANOUT && c < N; c++) out.push(c);
  return out;
};

for (let i = 0; i < N; i++) {
  const kids = children(i);
  const imports = kids.map((c) => `import { Comp${c} } from "./Comp${c}";`).join("\n");
  const renders = kids.map((c) => `<Comp${c} />`).join("");
  fs.writeFileSync(
    path.join(src, `Comp${i}.tsx`),
    `import { useState } from "react";
${imports}

export function Comp${i}() {
  const [n] = useState(${i});
  return (
    <div data-comp="${i}">
      <span>leaf-${i}-marker-A</span>{n}${renders}
    </div>
  );
}
`,
  );
}

fs.writeFileSync(
  path.join(dir, "src", "App.tsx"),
  `import { Comp0 } from "./components/Comp0";

export function App() {
  return (
    <main data-done="yes">
      <h1>bench app ${N}</h1>
      <Comp0 />
    </main>
  );
}
`,
);

fs.writeFileSync(
  path.join(dir, "src", "main.tsx"),
  `import { createRoot } from "react-dom/client";
import { App } from "./App";

createRoot(document.getElementById("root")!).render(<App />);
`,
);

fs.writeFileSync(
  path.join(dir, "index.html"),
  `<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8" />
    <title>bench ${N}</title>
  </head>
  <body>
    <div id="root"></div>
    <script type="module" src="/src/main.tsx"></script>
  </body>
</html>
`,
);

if (!fs.existsSync(path.join(dir, "package.json"))) {
  fs.writeFileSync(
    path.join(dir, "package.json"),
    JSON.stringify(
      {
        name: `bench-app-${N}`,
        private: true,
        dependencies: { react: "^19.1.0", "react-dom": "^19.1.0" },
        devDependencies: { vite: "^8", "@vitejs/plugin-react": "^6" },
      },
      null,
      2,
    ),
  );
}

fs.writeFileSync(
  path.join(dir, "vite.config.mjs"),
  `import react from "@vitejs/plugin-react";
export default { plugins: [react()], server: { port: 5200, strictPort: true }, logLevel: "warn" };
`,
);

fs.writeFileSync(
  path.join(dir, "vite.bundled.config.mjs"),
  `import react from "@vitejs/plugin-react";
export default {
  plugins: [react()],
  server: { port: 5200, strictPort: true },
  logLevel: "warn",
  experimental: { bundledDev: true },
};
`,
);

// tsconfig so editors don't complain. neither tool needs it.
fs.writeFileSync(
  path.join(dir, "tsconfig.json"),
  JSON.stringify({ compilerOptions: { jsx: "react-jsx", strict: true } }, null, 2),
);

console.log(`generated ${N} components in ${dir}`);
console.log(`next: (cd ${path.relative(process.cwd(), dir)} && npm install)`);
