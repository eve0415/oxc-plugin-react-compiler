## Code

```javascript
import { c as _c } from "react/compiler-runtime";
// import.meta.env.DEV should be handled by the HIR builder.
// Babel: _c(9) — compiles successfully
// OXC:   SKIP — fails with "Handle MetaProperty expressions"
import { useState } from 'react';
function Component(t0) {
  const $ = _c(9);
  const {
    title
  } = t0;
  const [count, setCount] = useState(0);
  const isDev = import.meta.env.DEV;
  let t1;
  if ($[0] !== title) {
    t1 = _jsx("h1", {
      children: title
    });
    $[0] = title;
    $[1] = t1;
  } else {
    t1 = $[1];
  }
  let t2;
  if ($[2] !== count) {
    t2 = _jsx("span", {
      children: count
    });
    $[2] = count;
    $[3] = t2;
  } else {
    t2 = $[3];
  }
  let t3;
  let t4;
  if ($[4] === Symbol.for("react.memo_cache_sentinel")) {
    t3 = isDev && _jsx("span", {
      children: "dev mode"
    });
    t4 = _jsx("button", {
      onClick: () => setCount(_temp),
      children: "+"
    });
    $[4] = t3;
    $[5] = t4;
  } else {
    t3 = $[4];
    t4 = $[5];
  }
  let t5;
  if ($[6] !== t1 || $[7] !== t2) {
    t5 = _jsxs("div", {
      children: [t1, t2, t3, t4]
    });
    $[6] = t1;
    $[7] = t2;
    $[8] = t5;
  } else {
    t5 = $[8];
  }
  return t5;
}
function _temp(c) {
  return c + 1;
}
export const FIXTURE_ENTRYPOINT = {
  fn: Component,
  params: [{
    title: 'test'
  }]
};
```
