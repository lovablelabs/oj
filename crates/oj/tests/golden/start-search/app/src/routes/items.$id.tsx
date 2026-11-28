import { createFileRoute, notFound } from "@tanstack/react-router";

export const Route = createFileRoute("/items/$id")({
  params: {
    parse: (raw) => ({ id: Number(raw.id) }),
    stringify: ({ id }) => ({ id: String(id) }),
  },
  loader: ({ params }) => {
    if (params.id > 10) throw notFound();
    return { name: `item ${params.id}` };
  },
  notFoundComponent: () => <p>no such item</p>,
  component: Item,
});

function Item() {
  const { name } = Route.useLoaderData();
  const { id } = Route.useParams();
  return (
    <article data-id={id}>
      <h1>{name}</h1>
    </article>
  );
}
