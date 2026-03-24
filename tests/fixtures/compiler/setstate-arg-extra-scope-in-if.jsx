// OXC creates an extra reactive scope for a ternary expression passed
// as an argument to setState inside an if-branch. Babel leaves the
// expression inline; OXC wraps `mode === "all" ? processed : []` in
// its own guard, adding 3 extra cache slots (16 in Babel vs 19 in OXC).
import { useState, useMemo } from 'react';

function Component({ items, mode }) {
  const processed = useMemo(() => items.map(x => x.toUpperCase()), [items]);

  const [selected, setSelected] = useState(() => mode === "all" ? processed : []);
  const [complete, setComplete] = useState(() => mode === "all");

  const [prevMode, setPrevMode] = useState(mode);
  if (mode !== prevMode) {
    setPrevMode(mode);
    setSelected(mode === "all" ? processed : []);
    setComplete(mode === "all");
  }

  return (
    <div>
      <span>{complete ? "done" : "pending"}</span>
      <ul>{selected.map(s => <li key={s}>{s}</li>)}</ul>
    </div>
  );
}

export const FIXTURE_ENTRYPOINT = { fn: Component, params: [{ items: ["a", "b"], mode: "all" }] };
