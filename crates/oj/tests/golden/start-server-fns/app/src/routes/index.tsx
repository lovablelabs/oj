import { createFileRoute } from "@tanstack/react-router";
import { useState } from "react";

import { count, greet } from "@app/server/greet";
import { where } from "@app/server/env";

export const Route = createFileRoute("/")({
  loader: () => greet({ data: "golden" }),
  component: Index,
});

function Index() {
  const data = Route.useLoaderData();
  const [n, setN] = useState(0);
  return (
    <main>
      <h1>{data.text}</h1>
      <p>{where()}</p>
      <button onClick={() => count({ data: n }).then(setN)}>{n}</button>
    </main>
  );
}
