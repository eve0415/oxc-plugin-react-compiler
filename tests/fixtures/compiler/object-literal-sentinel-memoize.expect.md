## Code

```javascript
import { c as _c } from "react/compiler-runtime";
// @compilationMode(infer)
// Object literal with all-primitive values should be sentinel-memoized
// even when the result is indexed by a reactive prop.
// Babel memoizes {primary:'cls-a',...} with a sentinel guard.
// OXC leaves the object literal inline, recreating it every render.
import { useState } from 'react';
function useCustom(val, opts) {
  return val;
}
function Component(t0) {
  const $ = _c(14);
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
  const colorClass = {
    primary: "cls-a",
    secondary: "cls-b",
    tertiary: "cls-c"
  }[color];
  let t4;
  if ($[3] !== label) {
    t4 = <span>{label}</span>;
    $[3] = label;
    $[4] = t4;
  } else {
    t4 = $[4];
  }
  const t5 = `text ${colorClass}`;
  let t6;
  if ($[5] !== suffix) {
    t6 = suffix && <span>{suffix}</span>;
    $[5] = suffix;
    $[6] = t6;
  } else {
    t6 = $[6];
  }
  let t7;
  if ($[7] !== displayValue || $[8] !== t5 || $[9] !== t6) {
    t7 = <span className={t5}>{displayValue}{t6}</span>;
    $[7] = displayValue;
    $[8] = t5;
    $[9] = t6;
    $[10] = t7;
  } else {
    t7 = $[10];
  }
  let t8;
  if ($[11] !== t4 || $[12] !== t7) {
    t8 = <div>{t4}{t7}</div>;
    $[11] = t4;
    $[12] = t7;
    $[13] = t8;
  } else {
    t8 = $[13];
  }
  return t8;
}
export const FIXTURE_ENTRYPOINT = {
  fn: Component,
  params: [{}]
};
```
