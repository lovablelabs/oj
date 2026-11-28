import { createRootRoute, HeadContent, Outlet, Scripts } from "@tanstack/react-router";
import { Link } from "@tanstack/react-router";
import "../styles.css";

export const Route = createRootRoute({
  head: () => ({ meta: [{ title: import.meta.env.VITE_TITLE }] }),
  component: Root,
});

function Root() {
  return (
    <html lang="en">
      <head>
        <HeadContent />
      </head>
      <body>
        <nav>
          <Link to="/">home</Link> <Link to="/items">items</Link>
        </nav>
        <Outlet />
        <Scripts />
      </body>
    </html>
  );
}
