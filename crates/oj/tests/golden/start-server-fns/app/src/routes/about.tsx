import { createFileRoute } from "@tanstack/react-router";

import { secret } from "@app/server/env";

export const Route = createFileRoute("/about")({
  loader: () => ({ hidden: typeof secret }),
  component: () => <p>{Route.useLoaderData().hidden}</p>,
});
