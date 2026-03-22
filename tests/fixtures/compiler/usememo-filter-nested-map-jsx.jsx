// @compilationMode(infer)
// useMemo with .filter() followed by nested .map() calls in JSX.
// Babel: _c(9) — compiles with nested memoization and hoisted _temp functions
// OXC:   SKIP — bails during compilation
import { useMemo } from 'react';
function Component({ items, flag }) {
  const filtered = useMemo(
    () => items.filter(x => x.active === flag),
    [items, flag]
  );
  return (
    <div>
      {filtered.map(item => (
        <div key={item.id}>
          {item.tags.map((tag, i) => (
            <span key={i}>{tag}</span>
          ))}
        </div>
      ))}
    </div>
  );
}
export const FIXTURE_ENTRYPOINT = { fn: Component, params: [{ items: [{ id: 1, active: true, tags: ['x'] }], flag: true }] };
