## Input

```javascript
// Babel memoizes static object literals used as property lookup maps
// with a sentinel check (_c slot). OXC computes them inline without
// caching, using 1 fewer cache slot.
// Pattern: { key: value }[dynamicProp] for enum-like mappings.
// From: eve0415/website stat-row.tsx, skill-card.tsx, language-bar.tsx
import { useState } from 'react';

function useCustomHook(value, opts) {
  return value;
}

function StatRow({ label, value, delay, animate, color = 'primary' }) {
  const displayValue = useCustomHook(value, {
    enabled: animate,
    delay,
    duration: 1200,
  });

  const colorClass = {
    primary: 'text-green',
    secondary: 'text-blue',
    tertiary: 'text-orange',
  }[color];

  return (
    <div className="stat-row">
      <span className="label">{label}</span>
      <span className={`value ${colorClass}`}>
        {displayValue}
      </span>
    </div>
  );
}

export const FIXTURE_ENTRYPOINT = {
  fn: StatRow,
  params: [{ label: 'Count', value: 42, delay: 0, animate: true, color: 'primary' }],
};
```

## Code

```javascript
import { c as _c } from "react/compiler-runtime";
// Babel memoizes static object literals used as property lookup maps
// with a sentinel check (_c slot). OXC computes them inline without
// caching, using 1 fewer cache slot.
// Pattern: { key: value }[dynamicProp] for enum-like mappings.
// From: eve0415/website stat-row.tsx, skill-card.tsx, language-bar.tsx
import { useState } from 'react';
function useCustomHook(value, opts) {
  return value;
}
function StatRow(t0) {
  const $ = _c(12);
  const {
    label,
    value,
    delay,
    animate,
    color: t1
  } = t0;
  const color = t1 === undefined ? "primary" : t1;
  let t2;
  if ($[0] !== animate || $[1] !== delay) {
    t2 = {
      enabled: animate,
      delay,
      duration: 1200
    };
    $[0] = animate;
    $[1] = delay;
    $[2] = t2;
  } else {
    t2 = $[2];
  }
  const displayValue = useCustomHook(value, t2);
  let t3;
  if ($[3] === Symbol.for("react.memo_cache_sentinel")) {
    t3 = {
      primary: "text-green",
      secondary: "text-blue",
      tertiary: "text-orange"
    };
    $[3] = t3;
  } else {
    t3 = $[3];
  }
  const colorClass = t3[color];
  let t4;
  if ($[4] !== label) {
    t4 = <span className="label">{label}</span>;
    $[4] = label;
    $[5] = t4;
  } else {
    t4 = $[5];
  }
  const t5 = `value ${colorClass}`;
  let t6;
  if ($[6] !== displayValue || $[7] !== t5) {
    t6 = <span className={t5}>{displayValue}</span>;
    $[6] = displayValue;
    $[7] = t5;
    $[8] = t6;
  } else {
    t6 = $[8];
  }
  let t7;
  if ($[9] !== t4 || $[10] !== t6) {
    t7 = <div className="stat-row">{t4}{t6}</div>;
    $[9] = t4;
    $[10] = t6;
    $[11] = t7;
  } else {
    t7 = $[11];
  }
  return t7;
}
export const FIXTURE_ENTRYPOINT = {
  fn: StatRow,
  params: [{
    label: 'Count',
    value: 42,
    delay: 0,
    animate: true,
    color: 'primary'
  }]
};
```
