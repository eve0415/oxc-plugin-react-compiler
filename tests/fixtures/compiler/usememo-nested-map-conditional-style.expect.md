## Input

```javascript
// useMemo + nested .map() causes bail when
// enablePreserveExistingMemoizationGuarantees=false (Babel v1.0.0 default).
// Without useMemo OR without nested map, it compiles fine.
// Babel compiles this successfully.
// From: eve0415/website error-cascade.tsx, division-by-zero.tsx, type-error.tsx,
// file-not-found.tsx, useBootAnimation.ts, code-radar.tsx (6 files affected)
import { useMemo } from 'react';

const ITEMS = [{ id: 1, lines: ['a', 'b'] }];

function Component({ enabled }) {
  const visible = useMemo(
    () => (enabled ? ITEMS : []),
    [enabled]
  );

  if (!visible.length) return null;

  return (
    <div>
      {visible.map(item => (
        <div key={item.id}>
          {item.lines.map((line, i) => (
            <span key={i}>{line}</span>
          ))}
        </div>
      ))}
    </div>
  );
}

export const FIXTURE_ENTRYPOINT = {
  fn: Component,
  params: [{ enabled: true }],
};
```

## Code

```javascript
import { c as _c } from "react/compiler-runtime";
// useMemo + nested .map() causes bail when
// enablePreserveExistingMemoizationGuarantees=false (Babel v1.0.0 default).
// Without useMemo OR without nested map, it compiles fine.
// Babel compiles this successfully.
// From: eve0415/website error-cascade.tsx, division-by-zero.tsx, type-error.tsx,
// file-not-found.tsx, useBootAnimation.ts, code-radar.tsx (6 files affected)
import { useMemo } from 'react';
const ITEMS = [{
  id: 1,
  lines: ['a', 'b']
}];
function Component(t0) {
  const $ = _c(6);
  const {
    enabled
  } = t0;
  let t1;
  if ($[0] !== enabled) {
    t1 = enabled ? ITEMS : [];
    $[0] = enabled;
    $[1] = t1;
  } else {
    t1 = $[1];
  }
  const visible = t1;
  if (!visible.length) {
    return null;
  }
  let t2;
  if ($[2] !== visible) {
    t2 = visible.map(_temp2);
    $[2] = visible;
    $[3] = t2;
  } else {
    t2 = $[3];
  }
  let t3;
  if ($[4] !== t2) {
    t3 = <div>{t2}</div>;
    $[4] = t2;
    $[5] = t3;
  } else {
    t3 = $[5];
  }
  return t3;
}
function _temp2(item) {
  return <div key={item.id}>{item.lines.map(_temp)}</div>;
}
function _temp(line, i) {
  return <span key={i}>{line}</span>;
}
export const FIXTURE_ENTRYPOINT = {
  fn: Component,
  params: [{
    enabled: true
  }]
};
```
