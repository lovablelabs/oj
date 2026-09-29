import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const R = parseInt(process.argv[2] ?? "24", 10);
const C = parseInt(process.argv[3] ?? "40", 10);
const FANOUT = 10;
const here = path.dirname(fileURLToPath(import.meta.url));
const donor = path.join(here, "apps", "app-1000"); // reuse its node_modules (react)

const kids = (i) => {
  const out = [];
  for (let c = i * FANOUT + 1; c <= i * FANOUT + FANOUT && c < C; c++) out.push(c);
  return out;
};

function genRouteTree(routeDir, prefix) {
  fs.mkdirSync(routeDir, { recursive: true });
  for (let i = 0; i < C; i++) {
    const k = kids(i);
    const imports = k.map((c) => `import { ${prefix}${c} } from "./${prefix}${c}";`).join("\n");
    const renders = k.map((c) => `<${prefix}${c} />`).join("");
    fs.writeFileSync(
      path.join(routeDir, `${prefix}${i}.tsx`),
      `import { useState } from "react";
${imports}

export function ${prefix}${i}() {
  const [n] = useState(${i});
  return (<div data-c="${prefix}${i}"><span>leaf-${prefix}-${i}-marker-A</span>{n}${renders}</div>);
}
`,
    );
  }
}

function build(variant) {
  const dir = path.join(here, "apps", `routes-${variant}`);
  fs.rmSync(path.join(dir, "src"), { recursive: true, force: true });
  const src = path.join(dir, "src");
  fs.mkdirSync(path.join(src, "routes"), { recursive: true });

  for (let r = 0; r < R; r++) {
    const prefix = `R${r}C`;
    genRouteTree(path.join(src, "routes", `route${r}`), prefix);
    fs.writeFileSync(
      path.join(src, "routes", `Route${r}.tsx`),
      `import { ${prefix}0 } from "./route${r}/${prefix}0";
export default function Route${r}() {
  return (<div${r === 0 ? ' data-done="yes"' : ""}><h1>route ${r}</h1><${prefix}0 /></div>);
}
`,
    );
  }

  const lazy = variant === "lazy";
  const routesMap = Array.from({ length: R }, (_, r) => `${r}: Route${r}`).join(", ");
  // Route 0 is the landing route: statically imported (eager) in BOTH variants,
  // so the landing paints without a load-on-mount waterfall. Routes 1..R-1 are
  // the difference: lazy import() boundaries vs static imports.
  const rest = Array.from({ length: R - 1 }, (_, i) => i + 1);
  const head =
    `import { useState } from "react";\n` +
    `import Route0 from "./routes/Route0";\n` +
    (lazy
      ? `import { lazy } from "react";\n` +
        rest.map((r) => `const Route${r} = lazy(() => import("./routes/Route${r}"));`).join("\n")
      : rest.map((r) => `import Route${r} from "./routes/Route${r}";`).join("\n"));
  fs.writeFileSync(
    path.join(src, "App.tsx"),
    `${head}

const routes: Record<number, any> = { ${routesMap} };

export function App() {
  const [r] = useState(0);
  const Route = routes[r]; // landing = route 0 (static in both variants)
  return (<main><Route /></main>);
}
`,
  );
  fs.writeFileSync(
    path.join(src, "main.tsx"),
    `import { createRoot } from "react-dom/client";
import { App } from "./App";

createRoot(document.getElementById("root")!).render(<App />);
`,
  );
  fs.writeFileSync(
    path.join(dir, "index.html"),
    `<!doctype html><html lang="en"><head><meta charset="utf-8"/><title>routes ${variant}</title></head><body><div id="root"></div><script type="module" src="/src/main.tsx"></script></body></html>
`,
  );
  fs.writeFileSync(
    path.join(dir, "package.json"),
    `{ "name": "routes-${variant}", "private": true, "type": "module" }\n`,
  );
  const nm = path.join(dir, "node_modules");
  fs.rmSync(nm, { recursive: true, force: true });
  fs.symlinkSync(path.join(donor, "node_modules"), nm, "dir");

  const total = R * (1 + C) + 2; // routes*(entry+tree) + App + main
  console.log(`routes-${variant}: ${R} routes x ${C} comps = ~${total} modules  (${dir})`);
}

build("lazy");
build("eager");
