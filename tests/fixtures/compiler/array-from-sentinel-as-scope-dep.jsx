// @enablePreserveExistingMemoizationGuarantees
// When Array.from() is sentinel-memoized and its result is used inside
// another reactive scope, Babel includes the Array.from result as a
// dependency of that scope. OXC omits it, producing 1 fewer cache slot.
// The sentinel result must be tracked as a dependency to ensure
// correctness if the sentinel scope is ever invalidated.
// From: eve0415/website index-out-of-bounds.tsx
import { useState } from 'react';

function Component({ enabled }) {
  const arraySize = 10;
  const [cursor, setCursor] = useState(0);

  if (!enabled) return null;

  return (
    <div>
      {Array.from({ length: arraySize }).map((_, i) => (
        <div key={i} className={cursor === i ? 'a' : 'b'}>{i}</div>
      ))}
    </div>
  );
}

export const FIXTURE_ENTRYPOINT = {
  fn: Component,
  params: [{ enabled: true }],
};
