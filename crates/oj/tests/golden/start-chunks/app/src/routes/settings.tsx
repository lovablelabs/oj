import { createFileRoute } from "@tanstack/react-router";

import { format } from "@app/lib/format";
import { table } from "@app/lib/table";

export const Route = createFileRoute("/settings")({
  component: () => <pre>{format("settings") + table(["x", "y"])}</pre>,
});
