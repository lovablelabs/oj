import { createFileRoute } from "@tanstack/react-router";

import { now, secret, side, viewport } from "../lib/env";

export const Route = createFileRoute("/")({
  loader: () => ({ side: side(), secret: secret() }),
  component: Home,
});

function Home() {
  const { side: where, secret: s } = Route.useLoaderData();
  return (
    <main>
      <p>{where}</p>
      <p>{s}</p>
      <button onClick={() => console.log(viewport(), now())}>measure</button>
    </main>
  );
}
