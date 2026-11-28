import { createFileRoute } from "@tanstack/react-router";

export const Route = createFileRoute("/blog/$slug")({
  component: () => <article>{Route.useParams().slug}</article>,
});
