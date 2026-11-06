import { createRouter } from "@tanstack/react-router";
import { routeTree } from "./routeTree";

export function getRouter() {
  return createRouter({ routeTree });
}
