## Input

```javascript
// Reduced from /tmp/website/src/routes/-components/NotFound/Aftermath/ErrorVisualizations/IndexOutOfBounds/index-out-of-bounds.tsx
// Real-world drift: Babel and OXC disagree on cache layout around a sentinel
// Array.from grid nested among other memoized siblings. Babel keeps the
// Array.from result in its own cache slot; OXC currently folds it into the
// mapped JSX path and emits fewer cache reads.
import { useState } from 'react';

const toHexValue = index => `0x${index.toString(16).padStart(2, '0')}`;

function Component() {
  const arraySize = 10;
  const targetIndex = 404;
  const [cursorPosition] = useState(11);
  const [corruptedText] = useState('0xDEAD');

  return (
    <div>
      <div>
        <span>int</span>[] data = <span>new</span> <span>int</span>[{arraySize}];
      </div>

      <div className="grid">
        {Array.from({ length: arraySize }).map((_, i) => (
          <div
            key={i}
            className={
              cursorPosition === i
                ? 'active'
                : cursorPosition > i
                  ? 'past'
                  : 'future'
            }
          >
            <span>{toHexValue(i)}</span>
            <span>[{i}]</span>
          </div>
        ))}

        <div>
          <span>→</span>
        </div>

        <div
          className={cursorPosition >= arraySize ? 'error' : 'idle'}
          style={
            cursorPosition >= arraySize
              ? { backgroundImage: 'repeating-linear-gradient(red, red)' }
              : undefined
          }
        >
          <span>{cursorPosition >= arraySize ? corruptedText : '???'}</span>
          <span>[{targetIndex}]</span>
          {cursorPosition >= arraySize && <div>!</div>}
        </div>
      </div>
    </div>
  );
}

export const FIXTURE_ENTRYPOINT = {
  fn: Component,
  params: [{}],
};
```

## Code

```javascript
import { c as _c } from "react/compiler-runtime";
// Reduced from /tmp/website/src/routes/-components/NotFound/Aftermath/ErrorVisualizations/IndexOutOfBounds/index-out-of-bounds.tsx
// Real-world drift: Babel and OXC disagree on cache layout around a sentinel
// Array.from grid nested among other memoized siblings. Babel keeps the
// Array.from result in its own cache slot; OXC currently folds it into the
// mapped JSX path and emits fewer cache reads.
import { useState } from 'react';
const toHexValue = index => `0x${index.toString(16).padStart(2, '0')}`;
function Component() {
  const $ = _c(22);
  const [cursorPosition] = useState(11);
  const [corruptedText] = useState("0xDEAD");
  let t0;
  if ($[0] === Symbol.for("react.memo_cache_sentinel")) {
    t0 = <span>int</span>;
    $[0] = t0;
  } else {
    t0 = $[0];
  }
  let t1;
  if ($[1] === Symbol.for("react.memo_cache_sentinel")) {
    t1 = <span>new</span>;
    $[1] = t1;
  } else {
    t1 = $[1];
  }
  let t2;
  if ($[2] === Symbol.for("react.memo_cache_sentinel")) {
    t2 = <div>{t0}[] data = {t1} <span>int</span>[{10}];</div>;
    $[2] = t2;
  } else {
    t2 = $[2];
  }
  let t3;
  if ($[3] === Symbol.for("react.memo_cache_sentinel")) {
    t3 = Array.from({
      length: 10
    });
    $[3] = t3;
  } else {
    t3 = $[3];
  }
  let t4;
  if ($[4] !== cursorPosition) {
    t4 = t3.map((_, i) => <div key={i} className={cursorPosition === i ? "active" : cursorPosition > i ? "past" : "future"}><span>{toHexValue(i)}</span><span>[{i}]</span></div>);
    $[4] = cursorPosition;
    $[5] = t4;
  } else {
    t4 = $[5];
  }
  let t5;
  if ($[6] === Symbol.for("react.memo_cache_sentinel")) {
    t5 = <div><span>→</span></div>;
    $[6] = t5;
  } else {
    t5 = $[6];
  }
  const t6 = cursorPosition >= 10 ? "error" : "idle";
  let t7;
  if ($[7] !== cursorPosition) {
    t7 = cursorPosition >= 10 ? {
      backgroundImage: "repeating-linear-gradient(red, red)"
    } : undefined;
    $[7] = cursorPosition;
    $[8] = t7;
  } else {
    t7 = $[8];
  }
  const t8 = cursorPosition >= 10 ? corruptedText : "???";
  let t9;
  if ($[9] !== t8) {
    t9 = <span>{t8}</span>;
    $[9] = t8;
    $[10] = t9;
  } else {
    t9 = $[10];
  }
  let t10;
  if ($[11] === Symbol.for("react.memo_cache_sentinel")) {
    t10 = <span>[{404}]</span>;
    $[11] = t10;
  } else {
    t10 = $[11];
  }
  let t11;
  if ($[12] !== cursorPosition) {
    t11 = cursorPosition >= 10 && <div>!</div>;
    $[12] = cursorPosition;
    $[13] = t11;
  } else {
    t11 = $[13];
  }
  let t12;
  if ($[14] !== t11 || $[15] !== t6 || $[16] !== t7 || $[17] !== t9) {
    t12 = <div className={t6} style={t7}>{t9}{t10}{t11}</div>;
    $[14] = t11;
    $[15] = t6;
    $[16] = t7;
    $[17] = t9;
    $[18] = t12;
  } else {
    t12 = $[18];
  }
  let t13;
  if ($[19] !== t12 || $[20] !== t4) {
    t13 = <div>{t2}<div className="grid">{t4}{t5}{t12}</div></div>;
    $[19] = t12;
    $[20] = t4;
    $[21] = t13;
  } else {
    t13 = $[21];
  }
  return t13;
}
export const FIXTURE_ENTRYPOINT = {
  fn: Component,
  params: [{}]
};
```
