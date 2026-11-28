import { createFileRoute, Outlet } from "@tanstack/react-router";

import "./_shell.css";

export const Route = createFileRoute("/_shell")({
  component: () => (
    <div className="shell">
      <aside>menu</aside>
      <Outlet />
    </div>
  ),
});
