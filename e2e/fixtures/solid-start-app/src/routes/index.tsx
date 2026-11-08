import { createFileRoute } from "@tanstack/solid-router";
import { createSignal, onMount } from "solid-js";
import { SolidStartHero } from "../components/SolidStartHero";

// Asset convention: ?raw inlines the text (mirrors the React fixture).
import notes from "../content/notes.txt?raw";

export const Route = createFileRoute("/")({
  // TanStack merges head down the route tree: the deepest route's title wins
  // over the root title. The e2e pins exactly that (route title present, root
  // title absent).
  head: () => ({
    meta: [{ title: "solid-start-fixture-home" }],
  }),
  component: Index,
});

function Index() {
  // A client-only hydration probe for the browser gate: the span is rendered
  // only after mount (absent from the SSR HTML, so its appearance proves the
  // client runtime came alive), and the counter's onClick proves event handlers
  // attached post-hydration (same convention as the React fixture).
  const [mounted, setMounted] = createSignal(false);
  const [count, setCount] = createSignal(0);
  onMount(() => setMounted(true));
  return (
    <main>
      <h1>solid-start-home-marker</h1>
      <SolidStartHero />
      {mounted() ? <span data-testid="client-mounted">client-mounted-ok</span> : null}
      <button data-testid="counter" onClick={() => setCount((c) => c + 1)}>
        count: {count()}
      </button>
      <p data-testid="raw">{notes.trim()}</p>
    </main>
  );
}
