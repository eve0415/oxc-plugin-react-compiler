## Input

```javascript
// @enablePreserveExistingMemoizationGuarantees
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
```

## Code

```javascript
import { c as _c } from "react/compiler-runtime";
// @enablePreserveExistingMemoizationGuarantees
import { useMemo } from "react";
function Component(t0) {
  const $ = _c(9);
  const {
    items,
    sortKey
  } = t0;
  let t1;
  if ($[0] !== items || $[1] !== sortKey) {
    let t2;
    if ($[3] !== sortKey) {
      t2 = (a, b) => {
        const aVal = a[sortKey];
        const bVal = b[sortKey];
        return aVal < bVal ? -1 : aVal > bVal ? 1 : 0;
      };
      $[3] = sortKey;
      $[4] = t2;
    } else {
      t2 = $[4];
    }
    t1 = [...items].sort(t2);
    $[0] = items;
    $[1] = sortKey;
    $[2] = t1;
  } else {
    t1 = $[2];
  }
  const sorted = t1;
  let t2;
  if ($[5] !== sorted) {
    t2 = sorted.map(_temp);
    $[5] = sorted;
    $[6] = t2;
  } else {
    t2 = $[6];
  }
  let t3;
  if ($[7] !== t2) {
    t3 = <ul>{t2}</ul>;
    $[7] = t2;
    $[8] = t3;
  } else {
    t3 = $[8];
  }
  return t3;
}
function _temp(item) {
  return <li key={item.id}>{item.name}</li>;
}
export const FIXTURE_ENTRYPOINT = {
  fn: Component,
  params: [{
    items: [{
      id: 1,
      name: "a"
    }],
    sortKey: "name"
  }]
};
```
