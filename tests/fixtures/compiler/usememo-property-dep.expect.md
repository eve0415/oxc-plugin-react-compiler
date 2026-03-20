## Code

```javascript
import { c as _c } from "react/compiler-runtime";
import { useMemo } from "react";
function Component(t0) {
  const $ = _c(7);
  const { items, sortKey } = t0;
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
    const sorted = [...items].sort(t2);
    t1 = sorted.map(_temp);
    $[0] = items;
    $[1] = sortKey;
    $[2] = t1;
  } else {
    t1 = $[2];
  }
  let t2;
  if ($[5] !== t1) {
    t2 = <ul>{t1}</ul>;
    $[5] = t1;
    $[6] = t2;
  } else {
    t2 = $[6];
  }
  return t2;
}
function _temp(item) {
  return <li key={item.id}>{item.name}</li>;
}
export const FIXTURE_ENTRYPOINT = { fn: Component, params: [{ items: [{ id: 1, name: "a" }], sortKey: "name" }] };
```
