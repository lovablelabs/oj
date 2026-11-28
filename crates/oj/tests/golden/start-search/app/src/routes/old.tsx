import { createFileRoute, redirect } from "@tanstack/react-router";

export const Route = createFileRoute("/old")({
  beforeLoad: () => {
    throw redirect({ to: "/", search: { q: "moved", page: 1 } });
  },
  component: () => null,
});
