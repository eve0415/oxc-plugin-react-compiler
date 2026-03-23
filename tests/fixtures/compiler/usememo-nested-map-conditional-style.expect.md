## Input

```javascript
// Nested .map() with conditional style objects inside a component
// that also uses useMemo. With enablePreserveExistingMemoizationGuarantees=false
// (Babel v1.0.0 default), OXC bails out while Babel compiles successfully.
// From: eve0415/website error-cascade.tsx
import { useMemo } from 'react';

const ITEMS = [
  { id: 1, message: 'Error 1', stack: ['line1', 'line2'], threshold: 0.1 },
  { id: 2, message: 'Error 2', stack: ['line1'], threshold: 0.5 },
];

function ErrorCascade({ progress, enabled }) {
  const reducedMotion = false;

  const visibleErrors = useMemo(() => {
    if (!enabled || progress < 0.05) return [];
    const effectiveProgress = progress ** 0.7;
    return ITEMS.filter(item => effectiveProgress >= item.threshold);
  }, [enabled, progress]);

  if (!enabled || visibleErrors.length === 0) return null;

  return (
    <div>
      {visibleErrors.map((error, errorIndex) => (
        <div key={error.id}
          style={reducedMotion ? { opacity: 1 } : { animation: 'fade 200ms', animationDelay: `${errorIndex * 30}ms`, opacity: 0 }}>
          <div>{error.message}</div>
          <div>
            {error.stack.map((line, lineIndex) => (
              <div key={lineIndex}
                style={reducedMotion ? { opacity: 1 } : { animation: 'fade 150ms', animationDelay: `${errorIndex * 30 + (lineIndex + 1) * 20}ms`, opacity: 0 }}>
                {line}
              </div>
            ))}
          </div>
        </div>
      ))}
    </div>
  );
}

export const FIXTURE_ENTRYPOINT = { fn: ErrorCascade, params: [{ progress: 0.5, enabled: true }] };
```

## Code

```javascript
import { c as _c } from "react/compiler-runtime";
// Nested .map() with conditional style objects inside a component
// that also uses useMemo. With enablePreserveExistingMemoizationGuarantees=false
// (Babel v1.0.0 default), OXC bails out while Babel compiles successfully.
// From: eve0415/website error-cascade.tsx
import { useMemo } from 'react';
const ITEMS = [{
  id: 1,
  message: 'Error 1',
  stack: ['line1', 'line2'],
  threshold: 0.1
}, {
  id: 2,
  message: 'Error 2',
  stack: ['line1'],
  threshold: 0.5
}];
function ErrorCascade(t0) {
  const $ = _c(7);
  const {
    progress,
    enabled
  } = t0;
  let t1;
  bb0: {
    if (!enabled || progress < 0.05) {
      let t2;
      if ($[0] === Symbol.for("react.memo_cache_sentinel")) {
        t2 = [];
        $[0] = t2;
      } else {
        t2 = $[0];
      }
      t1 = t2;
      break bb0;
    }
    const effectiveProgress = progress ** 0.7;
    let t2;
    if ($[1] !== effectiveProgress) {
      t2 = ITEMS.filter(item => effectiveProgress >= item.threshold);
      $[1] = effectiveProgress;
      $[2] = t2;
    } else {
      t2 = $[2];
    }
    t1 = t2;
  }
  const visibleErrors = t1;
  if (!enabled || visibleErrors.length === 0) {
    return null;
  }
  let t2;
  if ($[3] !== visibleErrors) {
    t2 = visibleErrors.map(_temp);
    $[3] = visibleErrors;
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
function _temp(error, errorIndex) {
  return <div key={error.id} style={false ? {
    opacity: 1
  } : {
    animation: "fade 200ms",
    animationDelay: `${errorIndex * 30}ms`,
    opacity: 0
  }}><div>{error.message}</div><div>{error.stack.map((line, lineIndex) => <div key={lineIndex} style={false ? {
        opacity: 1
      } : {
        animation: "fade 150ms",
        animationDelay: `${errorIndex * 30 + (lineIndex + 1) * 20}ms`,
        opacity: 0
      }}>{line}</div>)}</div></div>;
}
export const FIXTURE_ENTRYPOINT = {
  fn: ErrorCascade,
  params: [{
    progress: 0.5,
    enabled: true
  }]
};
```
