## Input

```javascript
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
```

## Code

```javascript
import { c as _c } from "react/compiler-runtime";
// @enablePreserveExistingMemoizationGuarantees
// When Array.from() is sentinel-memoized and its result is used inside
// another reactive scope, Babel includes the Array.from result as a
// dependency of that scope. OXC omits it, producing 1 fewer cache slot.
// The sentinel result must be tracked as a dependency to ensure
// correctness if the sentinel scope is ever invalidated.
// From: eve0415/website index-out-of-bounds.tsx
import { useState } from 'react';
function Component(t0) {
  const $ = _c(4);
  const {
    enabled
  } = t0;
  const [cursor] = useState(0);
  if (!enabled) {
    return null;
  }
  let t1;
  if ($[0] === Symbol.for("react.memo_cache_sentinel")) {
    t1 = Array.from({
      length: 10
    });
    $[0] = t1;
  } else {
    t1 = $[0];
  }
  let t2;
  if ($[1] !== cursor || $[2] !== t1) {
    t2 = <div>{t1.map((_, i) => <div key={i} className={cursor === i ? "a" : "b"}>{i}</div>)}</div>;
    $[1] = cursor;
    $[2] = t1;
    $[3] = t2;
  } else {
    t2 = $[3];
  }
  return t2;
}
export const FIXTURE_ENTRYPOINT = {
  fn: Component,
  params: [{
    enabled: true
  }]
};
```
