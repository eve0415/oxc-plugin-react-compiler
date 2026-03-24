## Input

```javascript
// @compilationMode(infer) @enablePreserveExistingMemoizationGuarantees
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
```

## Code

```javascript
import { c as _c } from "react/compiler-runtime";
// @compilationMode(infer) @enablePreserveExistingMemoizationGuarantees
// useMemo with .filter() in callback, then .map() on the result in JSX.
// Babel: _c(9) — compiles, memoizes filtered result and mapped JSX
// OXC:   SKIP — bails during compilation (transformed=false)
import { useMemo } from 'react';
function Component(t0) {
  const $ = _c(9);
  const {
    items,
    flag
  } = t0;
  let t1;
  if ($[0] !== flag || $[1] !== items) {
    let t2;
    if ($[3] !== flag) {
      t2 = x => x.active === flag;
      $[3] = flag;
      $[4] = t2;
    } else {
      t2 = $[4];
    }
    t1 = items.filter(t2);
    $[0] = flag;
    $[1] = items;
    $[2] = t1;
  } else {
    t1 = $[2];
  }
  const filtered = t1;
  let t2;
  if ($[5] !== filtered) {
    t2 = filtered.map(_temp);
    $[5] = filtered;
    $[6] = t2;
  } else {
    t2 = $[6];
  }
  let t3;
  if ($[7] !== t2) {
    t3 = <div>{t2}</div>;
    $[7] = t2;
    $[8] = t3;
  } else {
    t3 = $[8];
  }
  return t3;
}
function _temp(item) {
  return <div key={item.id}>{item.name}</div>;
}
export const FIXTURE_ENTRYPOINT = {
  fn: Component,
  params: [{
    items: [{
      id: 1,
      active: true,
      name: 'a'
    }],
    flag: true
  }]
};
```
