// @compilationMode(infer)
// import.meta.env.DEV should be handled by the HIR builder.
// Babel: _c(9) — compiles successfully
// OXC:   SKIP — fails with "Handle MetaProperty expressions"
import { useState } from 'react';
function Component({ title }) {
  const [count, setCount] = useState(0);
  const isDev = import.meta.env.DEV;
  return (
    <div>
      <h1>{title}</h1>
      <span>{count}</span>
      {isDev && <span>dev mode</span>}
      <button onClick={() => setCount(c => c + 1)}>+</button>
    </div>
  );
}
export const FIXTURE_ENTRYPOINT = { fn: Component, params: [{ title: 'test' }] };
