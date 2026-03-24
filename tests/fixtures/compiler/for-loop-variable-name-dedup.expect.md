## Input

```javascript
// @enablePreserveExistingMemoizationGuarantees
// When multiple for-loops use the same variable name (e.g. `i`), Babel
// renames them to `i_0`, `i_1`, etc. to avoid conflicts across scopes.
// OXC reuses the same name `i` in each loop, which is incorrect when
// the loops are in the same function scope after compilation.
// From: eve0415/website code-radar.tsx
function Component({ data }) {
  const result = [];

  for (let i = 0; i < 5; i++) {
    result.push(i * 10);
  }

  for (let i = 0; i < 7; i++) {
    result.push(i * 20);
  }

  for (let i = 0; i < data.length; i++) {
    result.push(data[i]);
  }

  return <div>{result.length}</div>;
}

export const FIXTURE_ENTRYPOINT = {
  fn: Component,
  params: [{ data: [1, 2, 3] }],
};
```

## Code

```javascript
import { c as _c } from "react/compiler-runtime";
// @enablePreserveExistingMemoizationGuarantees
// When multiple for-loops use the same variable name (e.g. `i`), Babel
// renames them to `i_0`, `i_1`, etc. to avoid conflicts across scopes.
// OXC reuses the same name `i` in each loop, which is incorrect when
// the loops are in the same function scope after compilation.
// From: eve0415/website code-radar.tsx
function Component(t0) {
  const $ = _c(4);
  const {
    data
  } = t0;
  let result;
  if ($[0] !== data) {
    result = [];
    for (let i = 0; i < 5; i++) {
      result.push(i * 10);
    }
    for (let i_0 = 0; i_0 < 7; i_0++) {
      result.push(i_0 * 20);
    }
    for (let i_1 = 0; i_1 < data.length; i_1++) {
      result.push(data[i_1]);
    }
    $[0] = data;
    $[1] = result;
  } else {
    result = $[1];
  }
  let t1;
  if ($[2] !== result.length) {
    t1 = <div>{result.length}</div>;
    $[2] = result.length;
    $[3] = t1;
  } else {
    t1 = $[3];
  }
  return t1;
}
export const FIXTURE_ENTRYPOINT = {
  fn: Component,
  params: [{
    data: [1, 2, 3]
  }]
};
```
