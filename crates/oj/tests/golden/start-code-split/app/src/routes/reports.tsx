import { createFileRoute } from "@tanstack/react-router";

import { Spinner } from "../components/spinner";
import { table } from "../lib/table";

export const Route = createFileRoute("/reports")({
  loader: async () => table(["a", "b"]),
  pendingComponent: Spinner,
  errorComponent: ({ error }) => <pre>{String(error)}</pre>,
  notFoundComponent: () => <p>no such report</p>,
  component: Reports,
});

function Reports() {
  const rows = Route.useLoaderData();
  return <pre>{rows}</pre>;
}
