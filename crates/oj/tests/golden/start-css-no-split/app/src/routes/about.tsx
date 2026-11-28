import { createFileRoute } from "@tanstack/react-router";

import "./about.css";

export const Route = createFileRoute("/about")({
  component: () => <main className="about">about</main>,
});
