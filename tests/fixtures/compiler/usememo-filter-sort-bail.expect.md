## Code

```javascript
// @compilationMode(infer)
// @validatePreserveExistingMemoizationGuarantees
// useMemo with filter+sort chain should compile successfully.
// Babel: _c(16) — compiles, replaces useMemo with cache slots
// OXC:   SKIP — bails on validatePreservedManualMemoization, then skips emit
import { useMemo, useState, useCallback } from 'react';
function Component({
  items,
  filter
}) {
  const [sort, setSort] = useState('name');
  const filtered = useMemo(() => items.filter(i => i.name.includes(filter)), [items, filter]);
  const sorted = useMemo(() => [...filtered].sort((a, b) => a[sort] > b[sort] ? 1 : -1), [filtered, sort]);
  const handleSort = useCallback(col => setSort(col), []);
  return <div>
      <button onClick={() => handleSort('name')}>Sort</button>
      {sorted.map(item => <div key={item.id}>{item.name}</div>)}
    </div>;
}
export const FIXTURE_ENTRYPOINT = {
  fn: Component,
  params: [{
    items: [{
      id: 1,
      name: 'a'
    }],
    filter: ''
  }]
};
```
