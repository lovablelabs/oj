import { createFileRoute } from "@tanstack/react-router";

declare global {
  // eslint-disable-next-line no-var
  var __APP_SSR__: boolean;
  // eslint-disable-next-line no-var
  var __PRICING__: boolean;
}

export const Route = createFileRoute("/")({
  loader: () => ({
    api: import.meta.env.VITE_API,
    mode: import.meta.env.MODE,
    ssr: import.meta.env.SSR,
    built: import.meta.env.VITE_BUILD_TIME,
    node: process.env.NODE_ENV,
  }),
  component: Index,
});

function Index() {
  const data = Route.useLoaderData();
  if (globalThis.__APP_SSR__) {
    return <p>server only</p>;
  }
  if (globalThis.__PRICING__) {
    import("../lib/pricing").then((m) => console.log(m.price()));
  }
  return <pre>{JSON.stringify(data)}</pre>;
}
