## Code

```javascript
import { c as _c } from "react/compiler-runtime";
// Method chain on reactive value should be memoized as a sub-expression.
// Babel extracts Math.floor(...).toString(16).toUpperCase().padStart(2,"0")
// into a separately memoized intermediate, then interpolates it.
// OXC inlines the chain directly in the template literal.
import { useState, useEffect, useRef } from 'react';
function generateBar(pct, total) {
  return 'x'.repeat(Math.floor(pct * total / 100));
}
function Component(t0) {
  const $ = _c(40);
  const {
    percentage,
    index,
    animate,
    isLast
  } = t0;
  const [progress, setProgress] = useState(0);
  const [visible, setVisible] = useState(!animate);
  const frameRef = useRef(0);
  let t1;
  if ($[0] !== animate || $[1] !== percentage) {
    t1 = () => {
      if (!animate) {
        setProgress(percentage);
        setVisible(true);
        return;
      }
      const start = performance.now();
      const step = t => {
        const elapsed = t - start;
        if (elapsed < 500) {
          setProgress(percentage * elapsed / 500);
          frameRef.current = requestAnimationFrame(step);
        } else {
          setProgress(percentage);
          setVisible(true);
        }
      };
      frameRef.current = requestAnimationFrame(step);
      return () => cancelAnimationFrame(frameRef.current);
    };
    $[0] = animate;
    $[1] = percentage;
    $[2] = t1;
  } else {
    t1 = $[2];
  }
  let t2;
  if ($[3] !== animate || $[4] !== index || $[5] !== percentage) {
    t2 = [animate, index, percentage];
    $[3] = animate;
    $[4] = index;
    $[5] = percentage;
    $[6] = t2;
  } else {
    t2 = $[6];
  }
  useEffect(t1, t2);
  let t3;
  if ($[7] !== progress) {
    t3 = generateBar(progress, 10);
    $[7] = progress;
    $[8] = t3;
  } else {
    t3 = $[8];
  }
  const bar10 = t3;
  let t4;
  if ($[9] !== progress) {
    t4 = generateBar(progress, 15);
    $[9] = progress;
    $[10] = t4;
  } else {
    t4 = $[10];
  }
  const bar15 = t4;
  let t5;
  if ($[11] !== progress) {
    t5 = generateBar(progress, 20);
    $[11] = progress;
    $[12] = t5;
  } else {
    t5 = $[12];
  }
  const bar20 = t5;
  let t6;
  if ($[13] !== progress) {
    t6 = Math.floor(progress).toString(16).toUpperCase().padStart(2, "0");
    $[13] = progress;
    $[14] = t6;
  } else {
    t6 = $[14];
  }
  const hexValue = `0x${t6}`;
  const cls = `row ${visible ? "show" : "hide"}`;
  const transDelay = `${index * 50}ms`;
  let t7;
  if ($[15] !== transDelay) {
    t7 = {
      transitionDelay: transDelay
    };
    $[15] = transDelay;
    $[16] = t7;
  } else {
    t7 = $[16];
  }
  let t8;
  if ($[17] !== bar10) {
    t8 = _jsx("span", {
      className: "sm-hide",
      children: bar10
    });
    $[17] = bar10;
    $[18] = t8;
  } else {
    t8 = $[18];
  }
  let t9;
  if ($[19] !== bar15) {
    t9 = _jsx("span", {
      className: "md-only",
      children: bar15
    });
    $[19] = bar15;
    $[20] = t9;
  } else {
    t9 = $[20];
  }
  let t10;
  if ($[21] !== bar20) {
    t10 = _jsx("span", {
      className: "lg-only",
      children: bar20
    });
    $[21] = bar20;
    $[22] = t10;
  } else {
    t10 = $[22];
  }
  let t11;
  if ($[23] !== hexValue) {
    t11 = _jsx("span", {
      children: hexValue
    });
    $[23] = hexValue;
    $[24] = t11;
  } else {
    t11 = $[24];
  }
  let t12;
  if ($[25] !== progress) {
    t12 = progress.toFixed(1);
    $[25] = progress;
    $[26] = t12;
  } else {
    t12 = $[26];
  }
  let t13;
  if ($[27] !== t12) {
    t13 = _jsxs("span", {
      children: [t12, "%"]
    });
    $[27] = t12;
    $[28] = t13;
  } else {
    t13 = $[28];
  }
  let t14;
  if ($[29] !== isLast) {
    t14 = isLast && _jsx("div", {
      className: "border"
    });
    $[29] = isLast;
    $[30] = t14;
  } else {
    t14 = $[30];
  }
  let t15;
  if ($[31] !== cls || $[32] !== t10 || $[33] !== t11 || $[34] !== t13 || $[35] !== t14 || $[36] !== t7 || $[37] !== t8 || $[38] !== t9) {
    t15 = _jsxs("div", {
      className: cls,
      style: t7,
      children: [t8, t9, t10, t11, t13, t14]
    });
    $[31] = cls;
    $[32] = t10;
    $[33] = t11;
    $[34] = t13;
    $[35] = t14;
    $[36] = t7;
    $[37] = t8;
    $[38] = t9;
    $[39] = t15;
  } else {
    t15 = $[39];
  }
  return t15;
}
export const FIXTURE_ENTRYPOINT = {
  fn: Component,
  params: [{}]
};
```
