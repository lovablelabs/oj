// The project the playground opens with: a small multi-file React site that
// exercises the pipeline (TSX, a plain stylesheet via <link>, CSS Modules).
export const DEMO_FILES: Record<string, string> = {
  "/index.html": `<!doctype html>
<html>
  <head>
    <meta charset="utf-8" />
    <title>the juice stand</title>
    <link rel="stylesheet" href="/src/global.css" />
  </head>
  <body>
    <div id="root"></div>
    <script type="module" src="/src/main.tsx"></script>
  </body>
</html>
`,
  "/src/main.tsx": `import { createRoot } from "react-dom/client";
import App from "./App";

createRoot(document.getElementById("root")!).render(<App />);
`,
  "/src/App.tsx": `import Counter from "./Counter";

const JUICES = [
  { name: "Orange", price: "$4" },
  { name: "Grapefruit", price: "$5" },
  { name: "Blood orange", price: "$6" },
];

export default function App() {
  return (
    <main>
      <h1>the juice stand</h1>
      <p>
        This little site is compiled by <strong>oj</strong> running as
        WebAssembly in your browser. Edit a file and watch it rebuild.
      </p>
      <ul>
        {JUICES.map((j) => (
          <li key={j.name}>
            <span>{j.name}</span>
            <em>{j.price}</em>
          </li>
        ))}
      </ul>
      <Counter />
    </main>
  );
}
`,
  "/src/Counter.tsx": `import { useState } from "react";
import styles from "./Counter.module.css";

export default function Counter() {
  const [glasses, setGlasses] = useState(0);
  return (
    <div className={styles.stand}>
      <button className={styles.squeeze} onClick={() => setGlasses(glasses + 1)}>
        squeeze an orange
      </button>
      <p className={styles.tally}>
        {glasses === 0 ? "no juice yet" : \`\${glasses} glass\${glasses === 1 ? "" : "es"} poured\`}
      </p>
    </div>
  );
}
`,
  "/src/Counter.module.css": `.stand {
  margin-top: 2rem;
  text-align: center;
}

.squeeze {
  font: inherit;
  padding: 0.6rem 1.4rem;
  border: 2px solid #e8590c;
  border-radius: 999px;
  background: #ff922b;
  color: #fff;
  cursor: pointer;
  transition: transform 120ms ease;
}

.squeeze:active {
  transform: scale(0.94);
}

.tally {
  margin-top: 0.8rem;
  color: #868e96;
}
`,
  "/src/global.css": `body {
  margin: 0;
  font-family: ui-sans-serif, system-ui, sans-serif;
  background: #fff8f0;
  color: #212529;
}

main {
  max-width: 26rem;
  margin: 3rem auto;
  padding: 0 1rem;
}

h1 {
  font-size: 1.8rem;
  letter-spacing: -0.02em;
}

ul {
  list-style: none;
  padding: 0;
}

li {
  display: flex;
  justify-content: space-between;
  padding: 0.5rem 0;
  border-bottom: 1px dashed #ffd8a8;
}

li em {
  font-style: normal;
  color: #e8590c;
}
`,
};

export const INITIAL_FILE = "/src/App.tsx";
