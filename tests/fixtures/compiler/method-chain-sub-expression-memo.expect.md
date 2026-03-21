## Code

```javascript
import { c as _c } from "react/compiler-runtime";
// @compilationMode(infer)
// Method chain on reactive value should be memoized as a sub-expression.
// Babel extracts Math.floor(...).toString(16).toUpperCase().padStart(2,"0")
// into a separately memoized intermediate, then interpolates it.
// OXC inlines the chain directly in the template literal.
import { useState, useEffect, useRef } from 'react';
function generateBar(pct, total) {
  return 'x'.repeat(Math.floor(pct * total / 100));
}
function Component(t0) {
  const $ = _c(38);
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
  const hexValue = `0x${Math.floor(progress).toString(16).toUpperCase().padStart(2, "0")}`;
  const cls = `row ${visible ? "show" : "hide"}`;
  const transDelay = `${index * 50}ms`;
  let t6;
  if ($[13] !== transDelay) {
    t6 = {
      transitionDelay: transDelay
    };
    $[13] = transDelay;
    $[14] = t6;
  } else {
    t6 = $[14];
  }
  let t7;
  if ($[15] !== bar10) {
    t7 = <span className="sm-hide">{bar10}</span>;
    $[15] = bar10;
    $[16] = t7;
  } else {
    t7 = $[16];
  }
  let t8;
  if ($[17] !== bar15) {
    t8 = <span className="md-only">{bar15}</span>;
    $[17] = bar15;
    $[18] = t8;
  } else {
    t8 = $[18];
  }
  let t9;
  if ($[19] !== bar20) {
    t9 = <span className="lg-only">{bar20}</span>;
    $[19] = bar20;
    $[20] = t9;
  } else {
    t9 = $[20];
  }
  let t10;
  if ($[21] !== hexValue) {
    t10 = <span>{hexValue}</span>;
    $[21] = hexValue;
    $[22] = t10;
  } else {
    t10 = $[22];
  }
  let t11;
  if ($[23] !== progress) {
    t11 = progress.toFixed(1);
    $[23] = progress;
    $[24] = t11;
  } else {
    t11 = $[24];
  }
  let t12;
  if ($[25] !== t11) {
    t12 = <span>{t11}%</span>;
    $[25] = t11;
    $[26] = t12;
  } else {
    t12 = $[26];
  }
  let t13;
  if ($[27] !== isLast) {
    t13 = isLast && <div className="border" />;
    $[27] = isLast;
    $[28] = t13;
  } else {
    t13 = $[28];
  }
  let t14;
  if ($[29] !== cls || $[30] !== t10 || $[31] !== t12 || $[32] !== t13 || $[33] !== t6 || $[34] !== t7 || $[35] !== t8 || $[36] !== t9) {
    t14 = <div className={cls} style={t6}>{t7}{t8}{t9}{t10}{t12}{t13}</div>;
    $[29] = cls;
    $[30] = t10;
    $[31] = t12;
    $[32] = t13;
    $[33] = t6;
    $[34] = t7;
    $[35] = t8;
    $[36] = t9;
    $[37] = t14;
  } else {
    t14 = $[37];
  }
  return t14;
}
export const FIXTURE_ENTRYPOINT = {
  fn: Component,
  params: [{}]
};
```
