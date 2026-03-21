import { useMemo } from "react";

function Component({ items, sortKey }) {
  const sorted = useMemo(() => {
    return [...items].sort((a, b) => {
      const aVal = a[sortKey];
      const bVal = b[sortKey];
      return aVal < bVal ? -1 : aVal > bVal ? 1 : 0;
    });
  }, [items, sortKey]);
  return <ul>{sorted.map((item) => <li key={item.id}>{item.name}</li>)}</ul>;
}

export const FIXTURE_ENTRYPOINT = {
  fn: Component,
  params: [{ items: [{ id: 1, name: "a" }], sortKey: "name" }],
};
