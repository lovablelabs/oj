import { createFileRoute } from "@tanstack/react-router";

import notes from "../content/notes.md?raw";
import data from "../content/data.json";
import logo from "../logo.svg?url";

const pages = import.meta.glob("../content/pages/*.ts", { eager: true, import: "title" });

export const Route = createFileRoute("/")({
  component: () => (
    <main>
      <img src={logo} alt="" />
      <h1>{data.title}</h1>
      <pre>{notes}</pre>
      <ul>
        {Object.entries(pages).map(([file, title]) => (
          <li key={file}>{String(title)}</li>
        ))}
      </ul>
    </main>
  ),
});
