## Code

```javascript
import { c as _c } from "react/compiler-runtime";
function Component(t0) {
  const $ = _c(2);
  const { cond, a, b } = t0;
  let x;
  if (cond) {
    x = a * 2;
  } else {
    x = b + 1;
  }
  let t1;
  if ($[0] !== x) {
    t1 = <div>{x}</div>;
    $[0] = x;
    $[1] = t1;
  } else {
    t1 = $[1];
  }
  return t1;
}

export const FIXTURE_ENTRYPOINT = {
  fn: Component,
  params: [{
    cond: true,
    a: 5,
    b: 10
  }]
};
```
