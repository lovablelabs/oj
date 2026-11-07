import { createRoute } from "@tanstack/react-router";

import { rootRoute } from "./__root";

// An app page under `/__`, as a component-preview route is: Vite reserves no
// `/__` prefix, so the dev server must hand it to the app like any document.
export const dunderRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/__preview/$name",
  component: Dunder,
});

function Dunder() {
  const { name } = dunderRoute.useParams();
  return (
    <main>
      <h1>dunder-route-marker {name}</h1>
    </main>
  );
}
