import { createFileRoute } from "@tanstack/react-router";

import "./about.scss";

export const Route = createFileRoute("/about")({
  component: () => (
    <section className="about">
      <h1 className="about__title mt-2">about</h1>
    </section>
  ),
});
