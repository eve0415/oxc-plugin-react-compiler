// @compilationMode(infer)
// useMemo with .filter() in callback, then .map() on the result in JSX.
// Babel: _c(9) — compiles, memoizes filtered result and mapped JSX
// OXC:   SKIP — bails during compilation (transformed=false)
import { useMemo } from 'react';
function Component({ items, flag }) {
  const filtered = useMemo(
    () => items.filter(x => x.active === flag),
    [items, flag]
  );
  return (
    <div>
      {filtered.map(item => (
        <div key={item.id}>{item.name}</div>
      ))}
    </div>
  );
}
export const FIXTURE_ENTRYPOINT = { fn: Component, params: [{ items: [{ id: 1, active: true, name: 'a' }], flag: true }] };
