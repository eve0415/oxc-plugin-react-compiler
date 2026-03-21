// Calls to locally-defined functions after an early return should NOT be
// separately memoized by keying on the function reference.
// Babel calls getColor()/getLabel() directly in the JSX scope block.
// OXC wraps each call in its own memo block keyed on the function ref (+4 slots).
import { useState, useEffect } from 'react';
function Component({ items, animate, title }) {
  const [status, setStatus] = useState('idle');
  const [count, setCount] = useState(0);
  useEffect(() => {
    if (animate) { setStatus('running'); setCount(c => c + 1); }
  }, [animate]);
  if (!items) return null;
  const getColor = () => {
    switch (status) { case 'running': return 'green'; default: return 'gray'; }
  };
  const getLabel = () => {
    switch (status) { case 'running': return 'ACTIVE'; default: return 'IDLE'; }
  };
  return (
    <div>
      <h1>{title}</h1>
      <div>
        <span>{count} items</span>
        <span style={{ color: getColor() }}>{getLabel()}</span>
      </div>
      <ul>
        {items.map((item, i) => <li key={i}>{item}</li>)}
      </ul>
    </div>
  );
}
export const FIXTURE_ENTRYPOINT = { fn: Component, params: [{ items: ['a'], title: 't' }] };
