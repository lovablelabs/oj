import { createFileRoute } from "@tanstack/react-router";
import { lazy, Suspense, useState } from "react";

import { points } from "../lib/points";

const Chart = lazy(() => import("../components/chart"));
const themes = import.meta.glob("../themes/*.css");

export const Route = createFileRoute("/")({
  loader: () => ({ points: points(8) }),
  pendingComponent: () => <p>loading</p>,
  errorComponent: ({ error }) => <pre>{String(error)}</pre>,
  component: Home,
});

function Home() {
  const data = Route.useLoaderData();
  const [greeting, setGreeting] = useState("");
  const theme = async (name: string) => {
    await import("../themes/dark.css");
    await themes[`../themes/${name}.css`]?.();
  };
  const speak = async (lang: string) => {
    const m = await import(`../locales/${lang}.ts`);
    setGreeting(m.default.hello);
  };
  return (
    <main>
      <Suspense fallback={<p>chart</p>}>
        <Chart points={data.points} />
      </Suspense>
      <button onClick={() => theme("light")}>light</button>
      <button onClick={() => speak("fr")}>{greeting}</button>
    </main>
  );
}
