## Code

```javascript
import { c as _c } from "react/compiler-runtime";
// @validatePreserveExistingMemoizationGuarantees
// useMemo with filter+sort chain should compile successfully.
// Babel: _c(16) — compiles, replaces useMemo with cache slots
// OXC:   SKIP — bails on validatePreservedManualMemoization, then skips emit
import { useMemo, useState, useCallback } from 'react';
function Component(t0) {
  const $ = _c(16);
  const {
    items,
    filter
  } = t0;
  const [sort, setSort] = useState("name");
  let t1;
  if ($[0] !== filter || $[1] !== items) {
    let t2;
    if ($[3] !== filter) {
      t2 = i => i.name.includes(filter);
      $[3] = filter;
      $[4] = t2;
    } else {
      t2 = $[4];
    }
    t1 = items.filter(t2);
    $[0] = filter;
    $[1] = items;
    $[2] = t1;
  } else {
    t1 = $[2];
  }
  const filtered = t1;
  let t2;
  if ($[5] !== filtered || $[6] !== sort) {
    let t3;
    if ($[8] !== sort) {
      t3 = (a, b) => a[sort] > b[sort] ? 1 : -1;
      $[8] = sort;
      $[9] = t3;
    } else {
      t3 = $[9];
    }
    t2 = [...filtered].sort(t3);
    $[5] = filtered;
    $[6] = sort;
    $[7] = t2;
  } else {
    t2 = $[7];
  }
  const sorted = t2;
  let t3;
  if ($[10] === Symbol.for("react.memo_cache_sentinel")) {
    t3 = col => setSort(col);
    $[10] = t3;
  } else {
    t3 = $[10];
  }
  const handleSort = t3;
  let t4;
  if ($[11] === Symbol.for("react.memo_cache_sentinel")) {
    t4 = _jsx("button", {
      onClick: () => handleSort("name"),
      children: "Sort"
    });
    $[11] = t4;
  } else {
    t4 = $[11];
  }
  let t5;
  if ($[12] !== sorted) {
    t5 = sorted.map(_temp);
    $[12] = sorted;
    $[13] = t5;
  } else {
    t5 = $[13];
  }
  let t6;
  if ($[14] !== t5) {
    t6 = _jsxs("div", {
      children: [t4, t5]
    });
    $[14] = t5;
    $[15] = t6;
  } else {
    t6 = $[15];
  }
  return t6;
}
function _temp(item) {
  return _jsx("div", {
    children: item.name
  }, item.id);
}
export const FIXTURE_ENTRYPOINT = {
  fn: Component,
  params: [{
    items: [{
      id: 1,
      name: 'a'
    }],
    filter: ''
  }]
};
```
