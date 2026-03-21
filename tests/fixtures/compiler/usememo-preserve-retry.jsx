// @validatePreserveExistingMemoizationGuarantees
// useMemo/useCallback preserve: compiler should still compile on retry
// when existing memoization cannot be preserved.
// OXC bails: "Existing memoization could not be preserved" then skips emit.
import { useCallback, useEffect, useMemo, useState } from 'react';
function Component() {
  const [highlighted, setHighlighted] = useState(null);
  const [showPath, setShowPath] = useState(false);
  const grid = useMemo(() => [
    { address: '0x0000', value: '42', isPointer: false },
    { address: '0x0008', value: 'ref', isPointer: true },
  ], []);
  useEffect(() => {
    const timer = setTimeout(() => {
      setHighlighted(1);
      setTimeout(() => setShowPath(true), 500);
    }, 800);
    return () => clearTimeout(timer);
  }, []);
  const handleHover = useCallback((index) => setHighlighted(index), []);
  const handleLeave = useCallback(() => setHighlighted(1), []);
  return (
    <div>
      {grid.map((cell, i) => (
        <div key={cell.address} onMouseEnter={() => handleHover(i)} onMouseLeave={handleLeave}>
          {cell.value}
          {showPath && cell.isPointer && <span>null</span>}
        </div>
      ))}
    </div>
  );
}

export const FIXTURE_ENTRYPOINT = {
  fn: Component,
  params: [{}],
};
