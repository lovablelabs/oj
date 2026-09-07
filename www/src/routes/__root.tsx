import { createRootRoute, HeadContent, Outlet, Scripts } from "@tanstack/react-router";

import "../../styles/app.css";
import { Nav, Footer } from "../components/site";

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
      { name: "theme-color", content: "#ffffff" },
    ],
    links: [
      { rel: "icon", type: "image/png", href: "/favicon-32x32.png" },
      { rel: "preconnect", href: "https://fonts.googleapis.com" },
      { rel: "preconnect", href: "https://fonts.gstatic.com", crossOrigin: "anonymous" },
      {
        rel: "stylesheet",
        href: "https://fonts.googleapis.com/css2?family=EB+Garamond:ital,wght@0,400;0,600;1,400;1,600&display=swap",
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
        <Nav />
        <main className="main">
          <Outlet />
        </main>
        <Footer />
        <Scripts />
      </body>
    </html>
  );
}
