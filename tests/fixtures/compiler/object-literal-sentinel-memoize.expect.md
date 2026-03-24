## Input

```javascript
// @compilationMode(infer) @enablePreserveExistingMemoizationGuarantees
// Object literal with all-primitive values should be sentinel-memoized
// even when the result is indexed by a reactive prop.
// Babel memoizes {primary:'cls-a',...} with a sentinel guard.
// OXC leaves the object literal inline, recreating it every render.
import { useState } from 'react';
function useCustom(val, opts) { return val; }
function Component({ label, value, suffix = '', delay, animate, color = 'primary' }) {
  const displayValue = useCustom(value, {
    enabled: animate, delay, duration: 1200,
  });
  const colorClass = {
    primary: 'cls-a',
    secondary: 'cls-b',
    tertiary: 'cls-c',
  }[color];
  return (
    <div>
      <span>{label}</span>
      <span className={`text ${colorClass}`}>
        {displayValue}
        {suffix && <span>{suffix}</span>}
      </span>
    </div>
  );
}
export const FIXTURE_ENTRYPOINT = { fn: Component, params: [{}] };
```

## Code

```javascript
import { c as _c } from "react/compiler-runtime";
// @compilationMode(infer) @enablePreserveExistingMemoizationGuarantees
// Object literal with all-primitive values should be sentinel-memoized
// even when the result is indexed by a reactive prop.
// Babel memoizes {primary:'cls-a',...} with a sentinel guard.
// OXC leaves the object literal inline, recreating it every render.
import { useState } from 'react';
function useCustom(val, opts) {
  return val;
}
function Component(t0) {
  const $ = _c(15);
  const {
    label,
    value,
    suffix: t1,
    delay,
    animate,
    color: t2
  } = t0;
  const suffix = t1 === undefined ? "" : t1;
  const color = t2 === undefined ? "primary" : t2;
  let t3;
  if ($[0] !== animate || $[1] !== delay) {
    t3 = {
      enabled: animate,
      delay,
      duration: 1200
    };
    $[0] = animate;
    $[1] = delay;
    $[2] = t3;
  } else {
    t3 = $[2];
  }
  const displayValue = useCustom(value, t3);
  let t4;
  if ($[3] === Symbol.for("react.memo_cache_sentinel")) {
    t4 = {
      primary: "cls-a",
      secondary: "cls-b",
      tertiary: "cls-c"
    };
    $[3] = t4;
  } else {
    t4 = $[3];
  }
  const colorClass = t4[color];
  let t5;
  if ($[4] !== label) {
    t5 = <span>{label}</span>;
    $[4] = label;
    $[5] = t5;
  } else {
    t5 = $[5];
  }
  const t6 = `text ${colorClass}`;
  let t7;
  if ($[6] !== suffix) {
    t7 = suffix && <span>{suffix}</span>;
    $[6] = suffix;
    $[7] = t7;
  } else {
    t7 = $[7];
  }
  let t8;
  if ($[8] !== displayValue || $[9] !== t6 || $[10] !== t7) {
    t8 = <span className={t6}>{displayValue}{t7}</span>;
    $[8] = displayValue;
    $[9] = t6;
    $[10] = t7;
    $[11] = t8;
  } else {
    t8 = $[11];
  }
  let t9;
  if ($[12] !== t5 || $[13] !== t8) {
    t9 = <div>{t5}{t8}</div>;
    $[12] = t5;
    $[13] = t8;
    $[14] = t9;
  } else {
    t9 = $[14];
  }
  return t9;
}
export const FIXTURE_ENTRYPOINT = {
  fn: Component,
  params: [{}]
};
```
