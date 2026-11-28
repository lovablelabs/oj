import { createFileRoute, Link } from "@tanstack/react-router";

export const Route = createFileRoute("/")({
  component: () => (
    <nav>
      <Link to="/projects">projects</Link>
      <Link to="/blog/$slug" params={{ slug: "hello" }}>
        blog
      </Link>
    </nav>
  ),
});
