import { createFileRoute } from "@tanstack/react-router";

import { format } from "@app/lib/format";
import { table } from "@app/lib/table";

export const Route = createFileRoute("/dashboard")({
  loader: async ({ location }) => {
    const lang = location.search ? "fr" : "en";
    const messages = lang === "fr" ? await import("@app/i18n/fr") : await import("@app/i18n/en");
    return { title: messages.default.dashboard };
  },
  component: () => <pre>{format(Route.useLoaderData().title) + table(["a"])}</pre>,
});
