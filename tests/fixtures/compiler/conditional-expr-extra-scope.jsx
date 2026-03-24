// OXC creates extra reactive scopes for ternary expressions that use
// method calls like .includes() on state arrays, when those expressions
// appear as JSX prop values. Babel leaves them inline as part of the
// JSX element scope; OXC splits each one into its own guard, inflating
// cache slots (9 in Babel vs 15 in OXC for this pattern).
import { useState, useMemo } from 'react';

function Component() {
  const [path, setPath] = useState([]);

  const nodes = useMemo(() => [
    { id: 'a', x: 10, y: 10 },
    { id: 'b', x: 50, y: 50 },
  ], []);

  return (
    <svg>
      <line
        x1="10%" y1="10%" x2="50%" y2="50%"
        stroke={path.includes("a") ? "blue" : "gray"}
        strokeWidth="2"
      />
      <line
        x1="50%" y1="50%" x2="90%" y2="90%"
        stroke={path.includes("b") ? "red" : "gray"}
        strokeWidth="2"
        strokeDasharray={path.includes("b") ? "5,5" : "0"}
      />
    </svg>
  );
}

export const FIXTURE_ENTRYPOINT = { fn: Component, params: [{}] };
