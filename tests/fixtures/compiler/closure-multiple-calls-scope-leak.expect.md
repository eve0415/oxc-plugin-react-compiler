## Code

```javascript
import { c as _c } from "react/compiler-runtime";
// A closure defined inside a scope block (with useEffect/useRef) and called
// multiple times should not leak extra memoization blocks.
// Babel inlines calls inside the scope block.
// OXC creates separate memo blocks for each call, inflating the cache.
import { useEffect, useRef, useState } from 'react';
function Component(t0) {
  const $ = _c(42);
  const {
    className: t1,
    animate: t2
  } = t0;
  const className = t1 === undefined ? "" : t1;
  const animate = t2 === undefined ? true : t2;
  const [isAnimating, setIsAnimating] = useState(animate);
  let t3;
  if ($[0] === Symbol.for("react.memo_cache_sentinel")) {
    t3 = [];
    $[0] = t3;
  } else {
    t3 = $[0];
  }
  const pathRefs = useRef(t3);
  let t4;
  let t5;
  if ($[1] !== animate) {
    t4 = () => {
      if (!animate) {
        return;
      }
      for (const p of pathRefs.current) {
        if (p) {
          const len = p.getTotalLength();
          p.style.strokeDasharray = String(len);
          p.style.strokeDashoffset = String(len);
        }
      }
      const timer = setTimeout(() => setIsAnimating(false), 2000);
      return () => clearTimeout(timer);
    };
    t5 = [animate];
    $[1] = animate;
    $[2] = t4;
    $[3] = t5;
  } else {
    t4 = $[2];
    t5 = $[3];
  }
  useEffect(t4, t5);
  let t10;
  let t11;
  let t12;
  let t13;
  let t14;
  let t15;
  let t6;
  let t7;
  let t8;
  let t9;
  if ($[4] !== animate || $[5] !== className || $[6] !== isAnimating) {
    const setRef = i => el => {
      pathRefs.current[i] = el;
    };
    let t16;
    if ($[17] !== isAnimating) {
      t16 = i_0 => ({
        opacity: isAnimating ? 0 : 1,
        transition: `stroke-dashoffset 0.8s ease-out ${i_0 * 0.1}s, fill 0.3s ease ${0.8 + i_0 * 0.1}s`
      });
      $[17] = isAnimating;
      $[18] = t16;
    } else {
      t16 = $[18];
    }
    const pathStyle = t16;
    const anim = animate ? "anim" : "";
    t7 = "0 0 100 100";
    t8 = `${className} wrap`;
    t9 = "img";
    const t17 = setRef(0);
    let t18;
    if ($[19] !== pathStyle) {
      t18 = pathStyle(0);
      $[19] = pathStyle;
      $[20] = t18;
    } else {
      t18 = $[20];
    }
    t10 = _jsx("path", {
      ref: t17,
      style: t18,
      d: "M10,10"
    });
    const t19 = setRef(1);
    let t20;
    if ($[21] !== pathStyle) {
      t20 = pathStyle(1);
      $[21] = pathStyle;
      $[22] = t20;
    } else {
      t20 = $[22];
    }
    t11 = _jsx("path", {
      ref: t19,
      style: t20,
      d: "M20,20"
    });
    const t21 = setRef(2);
    let t22;
    if ($[23] !== pathStyle) {
      t22 = pathStyle(2);
      $[23] = pathStyle;
      $[24] = t22;
    } else {
      t22 = $[24];
    }
    t12 = _jsx("path", {
      ref: t21,
      style: t22,
      d: "M30,30"
    });
    const t23 = setRef(3);
    let t24;
    if ($[25] !== pathStyle) {
      t24 = pathStyle(3);
      $[25] = pathStyle;
      $[26] = t24;
    } else {
      t24 = $[26];
    }
    t13 = _jsx("path", {
      ref: t23,
      style: t24,
      d: "M40,40"
    });
    const t25 = setRef(4);
    let t26;
    if ($[27] !== pathStyle) {
      t26 = pathStyle(4);
      $[27] = pathStyle;
      $[28] = t26;
    } else {
      t26 = $[28];
    }
    t14 = _jsx("path", {
      ref: t25,
      style: t26,
      d: "M50,50"
    });
    t6 = anim;
    t15 = _jsx("path", {
      ref: setRef(5),
      style: pathStyle(5),
      d: "M60,60"
    });
    $[4] = animate;
    $[5] = className;
    $[6] = isAnimating;
    $[7] = t10;
    $[8] = t11;
    $[9] = t12;
    $[10] = t13;
    $[11] = t14;
    $[12] = t15;
    $[13] = t6;
    $[14] = t7;
    $[15] = t8;
    $[16] = t9;
  } else {
    t10 = $[7];
    t11 = $[8];
    t12 = $[9];
    t13 = $[10];
    t14 = $[11];
    t15 = $[12];
    t6 = $[13];
    t7 = $[14];
    t8 = $[15];
    t9 = $[16];
  }
  let t16;
  if ($[29] !== t15 || $[30] !== t6) {
    t16 = _jsx("g", {
      className: t6,
      children: t15
    });
    $[29] = t15;
    $[30] = t6;
    $[31] = t16;
  } else {
    t16 = $[31];
  }
  let t17;
  if ($[32] !== t10 || $[33] !== t11 || $[34] !== t12 || $[35] !== t13 || $[36] !== t14 || $[37] !== t16 || $[38] !== t7 || $[39] !== t8 || $[40] !== t9) {
    t17 = _jsxs("svg", {
      viewBox: t7,
      className: t8,
      role: t9,
      children: [t10, t11, t12, t13, t14, t16]
    });
    $[32] = t10;
    $[33] = t11;
    $[34] = t12;
    $[35] = t13;
    $[36] = t14;
    $[37] = t16;
    $[38] = t7;
    $[39] = t8;
    $[40] = t9;
    $[41] = t17;
  } else {
    t17 = $[41];
  }
  return t17;
}
export const FIXTURE_ENTRYPOINT = {
  fn: Component,
  params: [{}]
};
```
