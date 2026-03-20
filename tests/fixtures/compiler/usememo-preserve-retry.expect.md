## Code

```javascript
import { c as _c } from "react/compiler-runtime";
import { useCallback, useEffect, useMemo, useState } from "react";
function Component() {
  const $ = _c(7);
  const [, setHighlighted] = useState(null);
  const [showPath, setShowPath] = useState(false);
  let t0;
  if ($[0] === Symbol.for("react.memo_cache_sentinel")) {
    t0 = [{ address: "0x0000", value: "42", isPointer: false }, { address: "0x0008", value: "ref", isPointer: true }];
    $[0] = t0;
  } else {
    t0 = $[0];
  }
  const grid = t0;
  let t1;
  let t2;
  if ($[1] === Symbol.for("react.memo_cache_sentinel")) {
    t1 = () => {
      const timer = setTimeout(() => {
        setHighlighted(1);
        setTimeout(() => setShowPath(true), 500);
      }, 800);
      return () => clearTimeout(timer);
    };
    t2 = [];
    $[1] = t1;
    $[2] = t2;
  } else {
    t1 = $[1];
    t2 = $[2];
  }
  useEffect(t1, t2);
  let t3;
  if ($[3] === Symbol.for("react.memo_cache_sentinel")) {
    t3 = index => setHighlighted(index);
    $[3] = t3;
  } else {
    t3 = $[3];
  }
  const handleHover = t3;
  let t4;
  if ($[4] === Symbol.for("react.memo_cache_sentinel")) {
    t4 = () => setHighlighted(1);
    $[4] = t4;
  } else {
    t4 = $[4];
  }
  const handleLeave = t4;
  let t5;
  if ($[5] !== showPath) {
    t5 = <div>{grid.map((cell, i) => <div key={cell.address} onMouseEnter={() => handleHover(i)} onMouseLeave={handleLeave}>{cell.value}{showPath && cell.isPointer && <span>null</span>}</div>)}</div>;
    $[5] = showPath;
    $[6] = t5;
  } else {
    t5 = $[6];
  }
  return t5;
}
export const FIXTURE_ENTRYPOINT = { fn: Component, params: [{}] };
```
