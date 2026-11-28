import { createFileRoute } from "@tanstack/react-router";
import { useServerFn } from "@tanstack/react-start";

import { listItems, rename } from "../server/items";

export const Route = createFileRoute("/items")({
  loader: () => listItems({ data: { page: 1 } }),
  component: Items,
});

function Items() {
  const { items, seen } = Route.useLoaderData();
  const save = useServerFn(rename);
  return (
    <ul data-seen={seen}>
      {items.map((item, i) => (
        <li key={item} onClick={() => save({ data: { id: i, name: item } })}>
          {item}
        </li>
      ))}
    </ul>
  );
}
