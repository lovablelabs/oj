import { createRootRoute, HeadContent, Outlet, Scripts } from "@tanstack/react-router";

export const rootRoute = createRootRoute({
  head: () => ({ meta: [{ title: "gating fixture" }] }),
  component: () => (
    <html lang="en">
      <head>
        <HeadContent />
      </head>
      <body>
        <Outlet />
        <Scripts />
      </body>
    </html>
  ),
});
