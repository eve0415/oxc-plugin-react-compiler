## Code

```javascript
import { c as _c } from "react/compiler-runtime";
// @compilationMode(infer)
// useMemo with .slice() in callback, then .map() on the result.
// Same root cause as filter — any array method returning new array from useMemo.
// Babel: _c(7) — compiles successfully
// OXC:   SKIP — bails during compilation
import { useMemo } from 'react';
function Component(t0) {
  const $ = _c(7);
  const {
    items,
    limit
  } = t0;
  let t1;
  if ($[0] !== items || $[1] !== limit) {
    t1 = items.slice(0, limit);
    $[0] = items;
    $[1] = limit;
    $[2] = t1;
  } else {
    t1 = $[2];
  }
  const sliced = t1;
  let t2;
  if ($[3] !== sliced) {
    t2 = sliced.map(_temp);
    $[3] = sliced;
    $[4] = t2;
  } else {
    t2 = $[4];
  }
  let t3;
  if ($[5] !== t2) {
    t3 = <div>{t2}</div>;
    $[5] = t2;
    $[6] = t3;
  } else {
    t3 = $[6];
  }
  return t3;
}
function _temp(item) {
  return <span key={item}>{item}</span>;
}
export const FIXTURE_ENTRYPOINT = {
  fn: Component,
  params: [{
    items: ['a', 'b', 'c'],
    limit: 2
  }]
};
```
