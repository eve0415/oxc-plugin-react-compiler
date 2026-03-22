// @compilationMode(infer) @enablePreserveExistingMemoizationGuarantees
// Object literal with all-primitive values should be sentinel-memoized
// even when the result is indexed by a reactive prop.
// Babel memoizes {primary:'cls-a',...} with a sentinel guard.
// OXC leaves the object literal inline, recreating it every render.
import { useState } from 'react';
function useCustom(val, opts) { return val; }
function Component({ label, value, suffix = '', delay, animate, color = 'primary' }) {
  const displayValue = useCustom(value, {
    enabled: animate, delay, duration: 1200,
  });
  const colorClass = {
    primary: 'cls-a',
    secondary: 'cls-b',
    tertiary: 'cls-c',
  }[color];
  return (
    <div>
      <span>{label}</span>
      <span className={`text ${colorClass}`}>
        {displayValue}
        {suffix && <span>{suffix}</span>}
      </span>
    </div>
  );
}
export const FIXTURE_ENTRYPOINT = { fn: Component, params: [{}] };
