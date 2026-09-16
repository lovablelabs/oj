import { createRootRoute, HeadContent, Outlet, Scripts } from "@tanstack/react-router";

import "../../styles/app.css";
import { Brand } from "../components/site";

export const rootRoute = createRootRoute({
  head: () => ({
    meta: [
      { charSet: "utf-8" },
      { name: "viewport", content: "width=device-width, initial-scale=1" },
      { title: "oj: Rust-native builds for React" },
      {
        name: "description",
        content:
          "oj is a Rust-native build tool for React apps: a fast dev server, SSR and TanStack Start, Tailwind, and one-command Cloudflare deploys.",
      },
      { name: "theme-color", content: "#191918" },
    ],
    links: [
      { rel: "icon", type: "image/png", href: "/favicon-32x32.png" },
      // font-display: block, so preloading keeps the invisible-text window short.
      {
        rel: "preload",
        as: "font",
        type: "font/woff2",
        href: "/fonts/CameraPlainVariable-c48bd243.woff2",
        crossOrigin: "anonymous",
      },
    ],
  }),
  component: RootComponent,
});

function RootComponent() {
  return (
    <html lang="en">
      <head>
        <HeadContent />
      </head>
      <body>
        <Brand />
        <main className="main">
          <Outlet />
        </main>
        <Scripts />
      </body>
    </html>
  );
}
