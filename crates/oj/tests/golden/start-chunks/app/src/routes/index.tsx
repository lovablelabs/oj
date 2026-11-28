import { createFileRoute, Link } from "@tanstack/react-router";

import { format } from "@app/lib/format";

export const Route = createFileRoute("/")({
  component: () => (
    <nav>
      <Link to="/dashboard">{format("dashboard")}</Link>
      <Link to="/settings">{format("settings")}</Link>
    </nav>
  ),
});
