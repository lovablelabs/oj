import { createRootRoute, HeadContent, Link, Outlet, Scripts } from "@tanstack/solid-router";
import { Suspense, type JSX } from "solid-js";
import { HydrationScript } from "solid-js/web";

// Conventional file-route root (mirrors the homepage root shape): the document
// shell lives in shellComponent with the solid-js HydrationScript in <head>
// (without it the client hydrate() has no SSR context and throws), head
// content renders in <body>, and a nav exercises router Link SPA navigation.
export const Route = createRootRoute({
  head: () => ({
    meta: [{ title: "solid-start-fixture-root" }],
  }),
  component: RootOutlet,
  shellComponent: RootDocument,
});

function RootOutlet() {
  return (
    <>
      <nav>
        <Link to="/" data-testid="nav-home">
          home
        </Link>{" "}
        <Link to="/about" data-testid="nav-about">
          about
        </Link>
      </nav>
      <Outlet />
    </>
  );
}

function RootDocument(props: { children: JSX.Element }) {
  return (
    <html lang="en">
      <head>
        <HydrationScript />
      </head>
      <body>
        <HeadContent />
        <Suspense>{props.children}</Suspense>
        <Scripts />
      </body>
    </html>
  );
}
