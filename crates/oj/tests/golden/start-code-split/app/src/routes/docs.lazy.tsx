import { createLazyFileRoute } from "@tanstack/react-router";

import { Spinner } from "../components/spinner";

export const Route = createLazyFileRoute("/docs")({
  component: () => <p>docs: {Route.useLoaderData().pages}</p>,
  pendingComponent: Spinner,
});
