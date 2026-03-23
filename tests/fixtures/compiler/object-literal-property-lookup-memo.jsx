// Babel memoizes static object literals used as property lookup maps
// with a sentinel check (_c slot). OXC computes them inline without
// caching, using 1 fewer cache slot.
// Pattern: { key: value }[dynamicProp] for enum-like mappings.
// From: eve0415/website stat-row.tsx, skill-card.tsx, language-bar.tsx
import { useState } from 'react';

function useCustomHook(value, opts) {
  return value;
}

function StatRow({ label, value, delay, animate, color = 'primary' }) {
  const displayValue = useCustomHook(value, {
    enabled: animate,
    delay,
    duration: 1200,
  });

  const colorClass = {
    primary: 'text-green',
    secondary: 'text-blue',
    tertiary: 'text-orange',
  }[color];

  return (
    <div className="stat-row">
      <span className="label">{label}</span>
      <span className={`value ${colorClass}`}>
        {displayValue}
      </span>
    </div>
  );
}

export const FIXTURE_ENTRYPOINT = {
  fn: StatRow,
  params: [{ label: 'Count', value: 42, delay: 0, animate: true, color: 'primary' }],
};
