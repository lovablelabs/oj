import { createFileRoute } from "@tanstack/react-router";

import { label } from "@app/lib/label";

export const Route = createFileRoute("/_shell/projects/$projectId")({
  loader: ({ params }) => ({ title: label(params.projectId) }),
  component: () => <h1>{Route.useLoaderData().title}</h1>,
});
