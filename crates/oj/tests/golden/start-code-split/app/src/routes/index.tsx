import { createFileRoute, Link } from "@tanstack/react-router";

export const Route = createFileRoute("/")({
  component: () => (
    <nav>
      <Link to="/reports">reports</Link>
      <Link to="/docs">docs</Link>
    </nav>
  ),
});
