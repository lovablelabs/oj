// A plugin that spawns a long-lived child (the workerd shape: miniflare's
// runtime is a plain non-detached spawn) so the shutdown test can prove the
// Start dev server sweeps plugin children on SIGTERM. node_modules is a
// symlink into ../start-app (the CI install step covers both).
import { spawn } from "node:child_process";
import { writeFileSync } from "node:fs";
import { tanstackStart } from "@tanstack/react-start/plugin/vite";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

const childSpawner = () => ({
  name: "shutdown-child-spawner",
  configureServer() {
    const plain = spawn("sleep", ["300"], { stdio: "ignore" });
    writeFileSync(new URL("./plain.pid", import.meta.url), String(plain.pid));
    const detached = spawn("sleep", ["300"], { detached: true, stdio: "ignore" });
    writeFileSync(new URL("./detached.pid", import.meta.url), String(detached.pid));
    detached.unref();
  },
});

export default defineConfig({
  plugins: [tanstackStart(), react(), childSpawner()],
  logLevel: "warn",
});
