## Input

```javascript
// @enablePreserveExistingMemoizationGuarantees
// useMemo callback with multiple return paths (early return) causes
// the compiler to bail out. Single-return useMemo callbacks work fine.
// After drop_manual_memoization converts useMemo(fn, deps) to fn(),
// inline_iifes uses Label+Break for multi-return IIFEs, producing a
// structure that fails validation.
// From: eve0415/website error-cascade.tsx (useMemo with if/return + filter)
import { useMemo } from 'react';

function Component({ enabled }) {
  const visible = useMemo(() => {
    if (!enabled) return [];
    return [1, 2, 3];
  }, [enabled]);
  return <div>{visible.length}</div>;
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
// useMemo callback with multiple return paths (early return) causes
// the compiler to bail out. Single-return useMemo callbacks work fine.
// After drop_manual_memoization converts useMemo(fn, deps) to fn(),
// inline_iifes uses Label+Break for multi-return IIFEs, producing a
// structure that fails validation.
// From: eve0415/website error-cascade.tsx (useMemo with if/return + filter)
import { useMemo } from 'react';
function Component(t0) {
  const $ = _c(4);
  const {
    enabled
  } = t0;
  let t1;
  bb0: {
    if (!enabled) {
      let t2;
      if ($[0] === Symbol.for("react.memo_cache_sentinel")) {
        t2 = [];
        $[0] = t2;
      } else {
        t2 = $[0];
      }
      t1 = t2;
      break bb0;
    }
    let t2;
    if ($[1] === Symbol.for("react.memo_cache_sentinel")) {
      t2 = [1, 2, 3];
      $[1] = t2;
    } else {
      t2 = $[1];
    }
    t1 = t2;
  }
  const visible = t1;
  let t2;
  if ($[2] !== visible.length) {
    t2 = <div>{visible.length}</div>;
    $[2] = visible.length;
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
