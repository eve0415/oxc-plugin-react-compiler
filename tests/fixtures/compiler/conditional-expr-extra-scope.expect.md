## Input

```javascript
// OXC creates extra reactive scopes for ternary expressions that use
// method calls like .includes() on state arrays, when those expressions
// appear as JSX prop values. Babel leaves them inline as part of the
// JSX element scope; OXC splits each one into its own guard, inflating
// cache slots (9 in Babel vs 15 in OXC for this pattern).
import { useState, useMemo } from 'react';

function Component() {
  const [path, setPath] = useState([]);

  const nodes = useMemo(() => [
    { id: 'a', x: 10, y: 10 },
    { id: 'b', x: 50, y: 50 },
  ], []);

  return (
    <svg>
      <line
        x1="10%" y1="10%" x2="50%" y2="50%"
        stroke={path.includes("a") ? "blue" : "gray"}
        strokeWidth="2"
      />
      <line
        x1="50%" y1="50%" x2="90%" y2="90%"
        stroke={path.includes("b") ? "red" : "gray"}
        strokeWidth="2"
        strokeDasharray={path.includes("b") ? "5,5" : "0"}
      />
    </svg>
  );
}

export const FIXTURE_ENTRYPOINT = { fn: Component, params: [{}] };
```

## Code

```javascript
import { c as _c } from "react/compiler-runtime";
// OXC creates extra reactive scopes for ternary expressions that use
// method calls like .includes() on state arrays, when those expressions
// appear as JSX prop values. Babel leaves them inline as part of the
// JSX element scope; OXC splits each one into its own guard, inflating
// cache slots (9 in Babel vs 15 in OXC for this pattern).
import { useState, useMemo } from 'react';
function Component() {
  const $ = _c(9);
  let t0;
  if ($[0] === Symbol.for("react.memo_cache_sentinel")) {
    t0 = [];
    $[0] = t0;
  } else {
    t0 = $[0];
  }
  const [path] = useState(t0);
  const t1 = path.includes("a") ? "blue" : "gray";
  let t2;
  if ($[1] !== t1) {
    t2 = <line x1="10%" y1="10%" x2="50%" y2="50%" stroke={t1} strokeWidth="2" />;
    $[1] = t1;
    $[2] = t2;
  } else {
    t2 = $[2];
  }
  const t3 = path.includes("b") ? "red" : "gray";
  const t4 = path.includes("b") ? "5,5" : "0";
  let t5;
  if ($[3] !== t3 || $[4] !== t4) {
    t5 = <line x1="50%" y1="50%" x2="90%" y2="90%" stroke={t3} strokeWidth="2" strokeDasharray={t4} />;
    $[3] = t3;
    $[4] = t4;
    $[5] = t5;
  } else {
    t5 = $[5];
  }
  let t6;
  if ($[6] !== t2 || $[7] !== t5) {
    t6 = <svg>{t2}{t5}</svg>;
    $[6] = t2;
    $[7] = t5;
    $[8] = t6;
  } else {
    t6 = $[8];
  }
  return t6;
}
export const FIXTURE_ENTRYPOINT = {
  fn: Component,
  params: [{}]
};
```
