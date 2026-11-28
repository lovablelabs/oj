import { createFileRoute } from "@tanstack/react-router";
import { z } from "zod";

const searchSchema = z.object({
  q: z.string().optional(),
  page: z.number().int().min(1).catch(1),
});

export const Route = createFileRoute("/")({
  validateSearch: searchSchema,
  loaderDeps: ({ search }) => ({ q: search.q, page: search.page }),
  loader: ({ deps }) => ({ hits: deps.q ? [`${deps.q} ${deps.page}`] : [] }),
  staticData: { section: "catalog" },
  component: Search,
});

function Search() {
  const { q, page } = Route.useSearch();
  const { hits } = Route.useLoaderData();
  const navigate = Route.useNavigate();
  return (
    <main>
      <input
        defaultValue={q ?? ""}
        onChange={(e) => navigate({ search: (prev) => ({ ...prev, q: e.target.value }) })}
      />
      <p>page {page}</p>
      <ul>
        {hits.map((hit) => (
          <li key={hit}>{hit}</li>
        ))}
      </ul>
    </main>
  );
}
