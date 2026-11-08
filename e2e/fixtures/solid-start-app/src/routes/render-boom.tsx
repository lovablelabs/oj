import { createFileRoute } from "@tanstack/solid-router";

// A route whose COMPONENT actually throws during render (not a loader
// throw like /boom): there is deliberately NO custom errorComponent, so the
// throw reaches the global default boundary. That boundary renders a
// legitimate fragment ("Something went wrong!" plus, in dev, the thrown
// message) with no <body> wrapper on both Vite and oj — the e2e asserts
// that genuine fallback markup, not a healthy-body wrapper.
export const Route = createFileRoute("/render-boom")({
  head: () => ({
    meta: [{ title: "solid-start-fixture-render-boom" }],
  }),
  component: RenderBoom,
});

function RenderBoom() {
  throw new Error("solid-start-render-boom-thrown-marker");
}
