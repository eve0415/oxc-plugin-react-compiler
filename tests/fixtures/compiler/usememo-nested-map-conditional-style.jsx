// Nested .map() with conditional style objects inside a component
// that also uses useMemo. With enablePreserveExistingMemoizationGuarantees=false
// (Babel v1.0.0 default), OXC bails out while Babel compiles successfully.
// From: eve0415/website error-cascade.tsx
import { useMemo } from 'react';

const ITEMS = [
  { id: 1, message: 'Error 1', stack: ['line1', 'line2'], threshold: 0.1 },
  { id: 2, message: 'Error 2', stack: ['line1'], threshold: 0.5 },
];

function ErrorCascade({ progress, enabled }) {
  const reducedMotion = false;

  const visibleErrors = useMemo(() => {
    if (!enabled || progress < 0.05) return [];
    const effectiveProgress = progress ** 0.7;
    return ITEMS.filter(item => effectiveProgress >= item.threshold);
  }, [enabled, progress]);

  if (!enabled || visibleErrors.length === 0) return null;

  return (
    <div>
      {visibleErrors.map((error, errorIndex) => (
        <div key={error.id}
          style={reducedMotion ? { opacity: 1 } : { animation: 'fade 200ms', animationDelay: `${errorIndex * 30}ms`, opacity: 0 }}>
          <div>{error.message}</div>
          <div>
            {error.stack.map((line, lineIndex) => (
              <div key={lineIndex}
                style={reducedMotion ? { opacity: 1 } : { animation: 'fade 150ms', animationDelay: `${errorIndex * 30 + (lineIndex + 1) * 20}ms`, opacity: 0 }}>
                {line}
              </div>
            ))}
          </div>
        </div>
      ))}
    </div>
  );
}

export const FIXTURE_ENTRYPOINT = { fn: ErrorCascade, params: [{ progress: 0.5, enabled: true }] };
