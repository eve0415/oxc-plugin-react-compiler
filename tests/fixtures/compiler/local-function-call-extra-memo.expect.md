## Input

```javascript
// @compilationMode(infer)
// Calls to locally-defined functions after an early return should NOT be
// separately memoized by keying on the function reference.
// Babel calls getColor()/getLabel() directly in the JSX scope block.
// OXC wraps each call in its own memo block keyed on the function ref (+4 slots).
import { useState, useEffect } from 'react';
function Component({ items, animate, title }) {
  const [status, setStatus] = useState('idle');
  const [count, setCount] = useState(0);
  useEffect(() => {
    if (animate) { setStatus('running'); setCount(c => c + 1); }
  }, [animate]);
  if (!items) return null;
  const getColor = () => {
    switch (status) { case 'running': return 'green'; default: return 'gray'; }
  };
  const getLabel = () => {
    switch (status) { case 'running': return 'ACTIVE'; default: return 'IDLE'; }
  };
  return (
    <div>
      <h1>{title}</h1>
      <div>
        <span>{count} items</span>
        <span style={{ color: getColor() }}>{getLabel()}</span>
      </div>
      <ul>
        {items.map((item, i) => <li key={i}>{item}</li>)}
      </ul>
    </div>
  );
}
export const FIXTURE_ENTRYPOINT = { fn: Component, params: [{ items: ['a'], title: 't' }] };
```

## Code

```javascript
import { c as _c } from "react/compiler-runtime";
// @compilationMode(infer)
// Calls to locally-defined functions after an early return should NOT be
// separately memoized by keying on the function reference.
// Babel calls getColor()/getLabel() directly in the JSX scope block.
// OXC wraps each call in its own memo block keyed on the function ref (+4 slots).
import { useState, useEffect } from 'react';
function Component(t0) {
  const $ = _c(27);
  const {
    items,
    animate,
    title
  } = t0;
  const [status, setStatus] = useState("idle");
  const [count, setCount] = useState(0);
  let t1;
  let t2;
  if ($[0] !== animate) {
    t1 = () => {
      if (animate) {
        setStatus("running");
        setCount(_temp);
      }
    };
    t2 = [animate];
    $[0] = animate;
    $[1] = t1;
    $[2] = t2;
  } else {
    t1 = $[1];
    t2 = $[2];
  }
  useEffect(t1, t2);
  if (!items) {
    return null;
  }
  let t3;
  if ($[3] !== status) {
    t3 = () => {
      switch (status) {
        case "running":
          {
            return "green";
          }
        default:
          {
            return "gray";
          }
      }
    };
    $[3] = status;
    $[4] = t3;
  } else {
    t3 = $[4];
  }
  const getColor = t3;
  let t4;
  if ($[5] !== status) {
    t4 = () => {
      switch (status) {
        case "running":
          {
            return "ACTIVE";
          }
        default:
          {
            return "IDLE";
          }
      }
    };
    $[5] = status;
    $[6] = t4;
  } else {
    t4 = $[6];
  }
  const getLabel = t4;
  let t5;
  if ($[7] !== title) {
    t5 = <h1>{title}</h1>;
    $[7] = title;
    $[8] = t5;
  } else {
    t5 = $[8];
  }
  let t6;
  if ($[9] !== count) {
    t6 = <span>{count} items</span>;
    $[9] = count;
    $[10] = t6;
  } else {
    t6 = $[10];
  }
  const t7 = getColor();
  let t8;
  if ($[11] !== t7) {
    t8 = {
      color: t7
    };
    $[11] = t7;
    $[12] = t8;
  } else {
    t8 = $[12];
  }
  const t9 = getLabel();
  let t10;
  if ($[13] !== t8 || $[14] !== t9) {
    t10 = <span style={t8}>{t9}</span>;
    $[13] = t8;
    $[14] = t9;
    $[15] = t10;
  } else {
    t10 = $[15];
  }
  let t11;
  if ($[16] !== t10 || $[17] !== t6) {
    t11 = <div>{t6}{t10}</div>;
    $[16] = t10;
    $[17] = t6;
    $[18] = t11;
  } else {
    t11 = $[18];
  }
  let t12;
  if ($[19] !== items) {
    t12 = items.map(_temp2);
    $[19] = items;
    $[20] = t12;
  } else {
    t12 = $[20];
  }
  let t13;
  if ($[21] !== t12) {
    t13 = <ul>{t12}</ul>;
    $[21] = t12;
    $[22] = t13;
  } else {
    t13 = $[22];
  }
  let t14;
  if ($[23] !== t11 || $[24] !== t13 || $[25] !== t5) {
    t14 = <div>{t5}{t11}{t13}</div>;
    $[23] = t11;
    $[24] = t13;
    $[25] = t5;
    $[26] = t14;
  } else {
    t14 = $[26];
  }
  return t14;
}
function _temp2(item, i) {
  return <li key={i}>{item}</li>;
}
function _temp(c) {
  return c + 1;
}
export const FIXTURE_ENTRYPOINT = {
  fn: Component,
  params: [{
    items: ['a'],
    title: 't'
  }]
};
```
