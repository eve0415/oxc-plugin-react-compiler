## Code

```javascript
import { c as _c } from "react/compiler-runtime";
import { useState } from "react";
function Component() {
  const $ = _c(2);
  const [progress] = useState(0);
  const t0 = Math.min(Math.floor(progress), 100);
  let t1;
  if ($[0] !== t0) {
    t1 = <div>{t0}%</div>;
    $[0] = t0;
    $[1] = t1;
  } else {
    t1 = $[1];
  }
  return t1;
}
export const FIXTURE_ENTRYPOINT = { fn: Component, params: [{}] };
```
