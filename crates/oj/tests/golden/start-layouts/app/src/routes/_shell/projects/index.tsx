import { createFileRoute, Link } from "@tanstack/react-router";

export const Route = createFileRoute("/_shell/projects/")({
  component: () => (
    <ul>
      <li>
        <Link to="/projects/$projectId" params={{ projectId: "p1" }}>
          p1
        </Link>
      </li>
    </ul>
  ),
});
