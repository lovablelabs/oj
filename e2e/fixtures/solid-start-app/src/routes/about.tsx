import { createFileRoute } from "@tanstack/solid-router";

// A second route, to prove multi-route SSR + SPA navigation.
export const Route = createFileRoute("/about")({
  head: () => ({
    meta: [{ title: "solid-start-fixture-about" }],
  }),
  component: About,
});

function About() {
  return (
    <main>
      <h1>solid-start-about-marker</h1>
      <p>solid-start-about-copy-marker</p>
    </main>
  );
}
