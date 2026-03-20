## Code

```javascript
import { c as _c } from "react/compiler-runtime";
// Side-effectful expression passed to useRef() should be sentinel-memoized.
// Babel: _c(9) — wraps the useRef argument in a sentinel guard
// OXC:   _c(8) — passes the expression directly to useRef
import { useCallback, useEffect, useRef, useState } from 'react';
const STORAGE_KEY = 'debug-mode';
function useDebugMode() {
  const $ = _c(9);
  let t0;
  if ($[0] === Symbol.for("react.memo_cache_sentinel")) {
    t0 = {
      enabled: false,
      index: 0
    };
    $[0] = t0;
  } else {
    t0 = $[0];
  }
  const [state, setState] = useState(t0);
  let t1;
  if ($[1] === Symbol.for("react.memo_cache_sentinel")) {
    t1 = globalThis.window !== undefined && localStorage.getItem(STORAGE_KEY) === "true";
    $[1] = t1;
  } else {
    t1 = $[1];
  }
  const needsSyncRef = useRef(t1);
  let t2;
  if ($[2] === Symbol.for("react.memo_cache_sentinel")) {
    t2 = () => {
      setState(_temp);
      localStorage.setItem(STORAGE_KEY, "true");
      needsSyncRef.current = false;
    };
    $[2] = t2;
  } else {
    t2 = $[2];
  }
  const enable = t2;
  let t3;
  if ($[3] === Symbol.for("react.memo_cache_sentinel")) {
    t3 = () => {
      setState(_temp2);
      localStorage.removeItem(STORAGE_KEY);
    };
    $[3] = t3;
  } else {
    t3 = $[3];
  }
  const disable = t3;
  let t4;
  if ($[4] === Symbol.for("react.memo_cache_sentinel")) {
    t4 = () => {
      setState(_temp3);
    };
    $[4] = t4;
  } else {
    t4 = $[4];
  }
  const toggle = t4;
  let t5;
  let t6;
  if ($[5] === Symbol.for("react.memo_cache_sentinel")) {
    t5 = () => {
      if (needsSyncRef.current) {
        setState(_temp4);
        needsSyncRef.current = false;
      }
    };
    t6 = [];
    $[5] = t5;
    $[6] = t6;
  } else {
    t5 = $[5];
    t6 = $[6];
  }
  useEffect(t5, t6);
  let t7;
  if ($[7] !== state) {
    t7 = {
      state,
      enable,
      disable,
      toggle
    };
    $[7] = state;
    $[8] = t7;
  } else {
    t7 = $[8];
  }
  return t7;
}
function _temp4(prev_2) {
  return {
    ...prev_2,
    enabled: true
  };
}
function _temp3(prev_1) {
  const next = !prev_1.enabled;
  if (next) {
    localStorage.setItem(STORAGE_KEY, "true");
  } else {
    localStorage.removeItem(STORAGE_KEY);
  }
  return {
    ...prev_1,
    enabled: next
  };
}
function _temp2(prev_0) {
  return {
    ...prev_0,
    enabled: false
  };
}
function _temp(prev) {
  return {
    ...prev,
    enabled: true
  };
}
export const FIXTURE_ENTRYPOINT = {
  fn: useDebugMode,
  params: []
};
```
