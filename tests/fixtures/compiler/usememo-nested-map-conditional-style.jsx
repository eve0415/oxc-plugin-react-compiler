// useMemo + nested .map() causes bail when
// enablePreserveExistingMemoizationGuarantees=false (Babel v1.0.0 default).
// Without useMemo OR without nested map, it compiles fine.
// Babel compiles this successfully.
// From: eve0415/website error-cascade.tsx, division-by-zero.tsx, type-error.tsx,
// file-not-found.tsx, useBootAnimation.ts, code-radar.tsx (6 files affected)
import { useMemo } from 'react';

const ITEMS = [{ id: 1, lines: ['a', 'b'] }];

function Component({ enabled }) {
  const visible = useMemo(
    () => (enabled ? ITEMS : []),
    [enabled]
  );

  if (!visible.length) return null;

  return (
    <div>
      {visible.map(item => (
        <div key={item.id}>
          {item.lines.map((line, i) => (
            <span key={i}>{line}</span>
          ))}
        </div>
      ))}
    </div>
  );
}

export const FIXTURE_ENTRYPOINT = {
  fn: Component,
  params: [{ enabled: true }],
};
