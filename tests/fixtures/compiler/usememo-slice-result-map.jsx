// @compilationMode(infer)
// useMemo with .slice() in callback, then .map() on the result.
// Same root cause as filter — any array method returning new array from useMemo.
// Babel: _c(7) — compiles successfully
// OXC:   SKIP — bails during compilation
import { useMemo } from 'react';
function Component({ items, limit }) {
  const sliced = useMemo(
    () => items.slice(0, limit),
    [items, limit]
  );
  return (
    <div>
      {sliced.map(item => <span key={item}>{item}</span>)}
    </div>
  );
}
export const FIXTURE_ENTRYPOINT = { fn: Component, params: [{ items: ['a', 'b', 'c'], limit: 2 }] };
