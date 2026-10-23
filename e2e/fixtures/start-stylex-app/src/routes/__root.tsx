import { createRootRoute, HeadContent, Outlet, Scripts } from "@tanstack/react-router";
import { useEffect } from "react";
import appCss from "../styles.css?url";

// The StyleX plugin's SSR-shell hookup: the dev sheet and its runtime in dev;
// in the build its rules join the app's own stylesheet.
const devLinks = import.meta.env.DEV ? [{ rel: "stylesheet", href: "/virtual:stylex.css" }] : [];

export const rootRoute = createRootRoute({
  head: () => ({
    meta: [{ title: "oj start stylex" }],
    links: [{ rel: "stylesheet", href: appCss }, ...devLinks],
  }),
  component: Root,
});

function Root() {
  useEffect(() => {
    if (import.meta.env.DEV) import("virtual:stylex:runtime");
  }, []);
  return (
    <html lang="en">
      <head>
        <HeadContent />
      </head>
      <body>
        <Outlet />
        <Scripts />
      </body>
    </html>
  );
}
