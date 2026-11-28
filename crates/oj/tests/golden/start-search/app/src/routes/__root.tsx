import { createRootRoute, HeadContent, Outlet, Scripts } from "@tanstack/react-router";
import { Link } from "@tanstack/react-router";
import "../styles.css";

export const Route = createRootRoute({
  head: () => ({
    meta: [{ title: import.meta.env.VITE_TITLE }],
    links: [{ rel: "icon", href: "/robots.txt" }],
    scripts: [{ src: "/analytics.js", defer: true }],
  }),
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
          <Link to="/" search={{ q: "pens" }}>
            search pens
          </Link>
          <Link to="/items/$id" params={{ id: 3 }}>
            item 3
          </Link>
          <Link to="/old">old</Link>
        </nav>
        <Outlet />
        <Scripts />
      </body>
    </html>
  );
}
