import { createRoute } from "@tanstack/react-router";

import { rootRoute } from "./__root";
import { Playground } from "../components/Playground";

function Home() {
  return (
    <div id="top">
      <section id="playground" className="play-bleed">
        <Playground />
      </section>
    </div>
  );
}

export const indexRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/",
  component: Home,
});
