import { createRoute } from "@tanstack/react-router";
import { rootRoute } from "./__root";
import { note } from "../note";

export const indexRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/",
  component: () => <main data-done="yes">gating fixture {note}</main>,
});
