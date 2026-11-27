import { createFileRoute, Link } from "@tanstack/react-router";
import { createServerFn } from "@tanstack/react-start";

import { shout } from "@app/lib/format";
import logo from "../logo.svg?url";

declare const __SIDE__: string;

const getGreeting = createServerFn({ method: "GET" }).handler(async () => ({
  text: shout(`hello from ${__SIDE__}`),
}));

export const Route = createFileRoute("/")({
  loader: () => getGreeting(),
  component: Index,
});

function Index() {
  const data = Route.useLoaderData();
  return (
    <main>
      <img src={logo} alt="" width={24} height={24} />
      <h1>{data.text}</h1>
      <Link to="/about">about</Link>
    </main>
  );
}
