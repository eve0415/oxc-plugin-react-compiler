// @enablePreserveExistingMemoizationGuarantees
// useMemo callback with multiple return paths (early return) causes
// the compiler to bail out. Single-return useMemo callbacks work fine.
// After drop_manual_memoization converts useMemo(fn, deps) to fn(),
// inline_iifes uses Label+Break for multi-return IIFEs, producing a
// structure that fails validation.
// From: eve0415/website error-cascade.tsx (useMemo with if/return + filter)
import { useMemo } from 'react';

function Component({ enabled }) {
  const visible = useMemo(() => {
    if (!enabled) return [];
    return [1, 2, 3];
  }, [enabled]);
  return <div>{visible.length}</div>;
}

export const FIXTURE_ENTRYPOINT = {
  fn: Component,
  params: [{ enabled: true }],
};
