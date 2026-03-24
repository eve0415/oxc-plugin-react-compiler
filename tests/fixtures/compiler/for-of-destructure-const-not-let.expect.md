## Input

```javascript
// @enablePreserveExistingMemoizationGuarantees
// When two for-of loops use the same destructured binding name (e.g. [i, item]),
// Babel preserves `const` and renames the second loop's variable to avoid conflicts.
// OXC may emit `let` instead of `const` for the destructuring pattern.
// From: eve0415/website skills-visualization.tsx
function Component({ staticItems, dynamicItems }) {
  const result = [];

  for (const [i, item] of staticItems.entries()) {
    result.push({ index: i, label: item.name, type: 'static' });
  }

  for (const [i, item] of dynamicItems.entries()) {
    result.push({ index: i, label: item.name, type: 'dynamic' });
  }

  return <div>{result.length}</div>;
}

export const FIXTURE_ENTRYPOINT = {
  fn: Component,
  params: [{ staticItems: [{ name: 'a' }], dynamicItems: [{ name: 'b' }] }],
};
```

## Code

```javascript
import { c as _c } from "react/compiler-runtime";
// @enablePreserveExistingMemoizationGuarantees
// When two for-of loops use the same destructured binding name (e.g. [i, item]),
// Babel preserves `const` and renames the second loop's variable to avoid conflicts.
// OXC may emit `let` instead of `const` for the destructuring pattern.
// From: eve0415/website skills-visualization.tsx
function Component(t0) {
  const $ = _c(5);
  const {
    staticItems,
    dynamicItems
  } = t0;
  let result;
  if ($[0] !== dynamicItems || $[1] !== staticItems) {
    result = [];
    for (const [i, item] of staticItems.entries()) {
      result.push({
        index: i,
        label: item.name,
        type: "static"
      });
    }
    for (const [i_0, item_0] of dynamicItems.entries()) {
      result.push({
        index: i_0,
        label: item_0.name,
        type: "dynamic"
      });
    }
    $[0] = dynamicItems;
    $[1] = staticItems;
    $[2] = result;
  } else {
    result = $[2];
  }
  let t1;
  if ($[3] !== result.length) {
    t1 = <div>{result.length}</div>;
    $[3] = result.length;
    $[4] = t1;
  } else {
    t1 = $[4];
  }
  return t1;
}
export const FIXTURE_ENTRYPOINT = {
  fn: Component,
  params: [{
    staticItems: [{
      name: 'a'
    }],
    dynamicItems: [{
      name: 'b'
    }]
  }]
};
```
