import { tanstackStart } from "@tanstack/solid-start/plugin/vite";
import solid from "vite-plugin-solid";
import { defineConfig } from "vite";

// Mirrors the homepage's plugin stack (tanstackStart + solid with SSR), minus
// unrelated integrations (tailwind, rsbuild). Routes are conventional
// file-routes (createFileRoute/createRootRoute with `Route` exports) so the
// TanStack generator and src/routeTree.gen.ts stay in sync with the router.
export default defineConfig({
  publicDir: "public",
  plugins: [tanstackStart(), solid({ ssr: true })],
});
