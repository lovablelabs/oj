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
          "oj is a Rust-native build tool for React apps. This playground is oj itself compiled to WebAssembly, building a live-editable site in your browser tab.",
      },
      { name: "theme-color", content: "#191918" },
    ],
    links: [
      { rel: "icon", type: "image/png", href: "/favicon-32x32.png" },
      { rel: "preconnect", href: "https://fonts.googleapis.com" },
      { rel: "preconnect", href: "https://fonts.gstatic.com", crossOrigin: "anonymous" },
      {
        rel: "stylesheet",
        href: "https://fonts.googleapis.com/css2?family=Inter:wght@100..900&display=swap",
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
