import { createFileRoute, Link } from "@tanstack/react-router";

declare const __FLAG__: string;

export const Route = createFileRoute("/")({
  component: () => (
    <nav data-flag={__FLAG__}>
      <Link to="/tracking">tracking</Link>
    </nav>
  ),
});
