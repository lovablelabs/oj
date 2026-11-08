import { createFileRoute } from "@tanstack/solid-router";

// A route whose LOADER throws before first render: TanStack Router catches
// it and renders the route errorComponent server-side (the reference answers
// 500; a handled loader throw may legitimately be 200, so the e2e accepts
// either). The e2e asserts that boundary message, not an empty response.
// (Render throws without an errorComponent are covered by /render-boom,
// which reaches the global default boundary instead.)
export const Route = createFileRoute("/boom")({
  head: () => ({
    meta: [{ title: "solid-start-fixture-boom" }],
  }),
  // A loader throw is caught before first render, so the framework renders the
  // route error boundary server-side (the reference answers 500; a handled
  // loader throw may legitimately be 200, so the e2e accepts either).
  loader: () => {
    throw new Error("solid-start-boom-thrown-marker");
  },
  component: Boom,
  errorComponent: BoomError,
});

function Boom() {
  return (
    <main>
      <h1>solid-start-boom-unreached-marker</h1>
    </main>
  );
}

function BoomError(props: { error: unknown }) {
  const message = props.error instanceof Error ? props.error.message : String(props.error);
  return (
    <main>
      <h1>solid-start-boom-error-marker</h1>
      <p>{message}</p>
    </main>
  );
}
