import { createFileRoute } from "@tanstack/react-router";
import { useEffect, useState } from "react";

import { big, pages, pick, Store } from "../lib/store";

const store = new Store({ theme: { name: "" } });

export const Route = createFileRoute("/")({
  component: Home,
});

function Home() {
  const [count, setCount] = useState(0);
  useEffect(() => {
    (async () => {
      for await (const page of pages(3)) setCount((c) => c + page);
    })();
  }, []);
  const name = pick(store.state, "default");
  return (
    <main className="pane">
      <p>{name}</p>
      <p>{count * big}</p>
      <button onClick={() => store.patch({ theme: { name: "dark" } })}>dark</button>
    </main>
  );
}
