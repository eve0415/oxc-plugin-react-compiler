## Input

```javascript
// OXC creates an extra reactive scope for a ternary expression passed
// as an argument to setState inside an if-branch. Babel leaves the
// expression inline; OXC wraps `mode === "all" ? processed : []` in
// its own guard, adding 3 extra cache slots (16 in Babel vs 19 in OXC).
import { useState, useMemo } from 'react';

function Component({ items, mode }) {
  const processed = useMemo(() => items.map(x => x.toUpperCase()), [items]);

  const [selected, setSelected] = useState(() => mode === "all" ? processed : []);
  const [complete, setComplete] = useState(() => mode === "all");

  const [prevMode, setPrevMode] = useState(mode);
  if (mode !== prevMode) {
    setPrevMode(mode);
    setSelected(mode === "all" ? processed : []);
    setComplete(mode === "all");
  }

  return (
    <div>
      <span>{complete ? "done" : "pending"}</span>
      <ul>{selected.map(s => <li key={s}>{s}</li>)}</ul>
    </div>
  );
}

export const FIXTURE_ENTRYPOINT = { fn: Component, params: [{ items: ["a", "b"], mode: "all" }] };
```

## Code

```javascript
import { c as _c } from "react/compiler-runtime";
// OXC creates an extra reactive scope for a ternary expression passed
// as an argument to setState inside an if-branch. Babel leaves the
// expression inline; OXC wraps `mode === "all" ? processed : []` in
// its own guard, adding 3 extra cache slots (16 in Babel vs 19 in OXC).
import { useState, useMemo } from 'react';
function Component(t0) {
  const $ = _c(16);
  const {
    items,
    mode
  } = t0;
  let t1;
  if ($[0] !== items) {
    t1 = items.map(_temp);
    $[0] = items;
    $[1] = t1;
  } else {
    t1 = $[1];
  }
  const processed = t1;
  let t2;
  if ($[2] !== mode || $[3] !== processed) {
    t2 = () => mode === "all" ? processed : [];
    $[2] = mode;
    $[3] = processed;
    $[4] = t2;
  } else {
    t2 = $[4];
  }
  const [selected, setSelected] = useState(t2);
  let t3;
  if ($[5] !== mode) {
    t3 = () => mode === "all";
    $[5] = mode;
    $[6] = t3;
  } else {
    t3 = $[6];
  }
  const [complete, setComplete] = useState(t3);
  const [prevMode, setPrevMode] = useState(mode);
  if (mode !== prevMode) {
    setPrevMode(mode);
    setSelected(mode === "all" ? processed : []);
    setComplete(mode === "all");
  }
  const t4 = complete ? "done" : "pending";
  let t5;
  if ($[7] !== t4) {
    t5 = <span>{t4}</span>;
    $[7] = t4;
    $[8] = t5;
  } else {
    t5 = $[8];
  }
  let t6;
  if ($[9] !== selected) {
    t6 = selected.map(_temp2);
    $[9] = selected;
    $[10] = t6;
  } else {
    t6 = $[10];
  }
  let t7;
  if ($[11] !== t6) {
    t7 = <ul>{t6}</ul>;
    $[11] = t6;
    $[12] = t7;
  } else {
    t7 = $[12];
  }
  let t8;
  if ($[13] !== t5 || $[14] !== t7) {
    t8 = <div>{t5}{t7}</div>;
    $[13] = t5;
    $[14] = t7;
    $[15] = t8;
  } else {
    t8 = $[15];
  }
  return t8;
}
function _temp2(s) {
  return <li key={s}>{s}</li>;
}
function _temp(x) {
  return x.toUpperCase();
}
export const FIXTURE_ENTRYPOINT = {
  fn: Component,
  params: [{
    items: ["a", "b"],
    mode: "all"
  }]
};
```
