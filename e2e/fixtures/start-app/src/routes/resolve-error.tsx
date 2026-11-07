import { createRoute } from "@tanstack/react-router";

import { rootRoute } from "./__root";

// An import whose resolveId hook throws in the dev server render: Vite's
// import analysis fails the import with the hook's error. Imported on this
// route alone, so no other page depends on it.
export const resolveErrorRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/resolve-error",
  loader: async () => (await import("fixture-throws/x.css?url")).default as string,
  component: ResolveError,
});

function ResolveError() {
  return (
    <main>
      <p data-testid="resolve-error">{resolveErrorRoute.useLoaderData()}</p>
    </main>
  );
}
