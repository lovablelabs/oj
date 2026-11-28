import { createFileRoute } from "@tanstack/react-router";

import "./tracking.css";

export const Route = createFileRoute("/tracking")({
  component: () => <p className="tracking">events</p>,
});
