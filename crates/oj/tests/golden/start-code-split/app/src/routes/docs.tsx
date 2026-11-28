import { createFileRoute } from "@tanstack/react-router";

export const Route = createFileRoute("/docs")({
  loader: () => ({ pages: 3 }),
});
