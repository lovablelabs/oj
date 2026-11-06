import { createRoute } from "@tanstack/react-router";
import { useState } from "react";
import { Card } from "../Card";
import { rootRoute } from "./__root";

function Home() {
  const [active, setActive] = useState(false);
  return <Card active={active} onToggle={() => setActive((a) => !a)} />;
}

export const indexRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/",
  component: Home,
});
