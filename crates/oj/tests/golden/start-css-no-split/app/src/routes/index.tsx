import { createFileRoute } from "@tanstack/react-router";

import "./index.css";

export const Route = createFileRoute("/")({
  component: () => <main className="home">home</main>,
});
