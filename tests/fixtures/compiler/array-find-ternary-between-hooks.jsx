// OXC fails to create reactive scopes for Array.find() and conditional
// expressions between hook calls (useMemo / useEffect). Babel wraps
// STAGES.find() and the ternary using it in reactive scopes; OXC emits
// them as plain recomputation every render, losing 7 cache slots.
import { useEffect, useMemo, useState } from 'react';

const STAGES = [
  { name: 'init', startAt: 0, duration: 1000 },
  { name: 'load', startAt: 1000, duration: 2000 },
  { name: 'done', startAt: 3000, duration: 500 },
];

const BASE_DURATION = 7000;

function useAnimation(enabled, elapsed, scaleFactor) {
  const [tick, setTick] = useState(0);

  const items = useMemo(
    () => STAGES.map(s => ({ ...s, scaled: s.duration * scaleFactor })),
    [scaleFactor]
  );

  // Plain imperative code between useMemo and useEffect.
  // Babel wraps these in reactive scopes; OXC does not.
  const scaledDuration = BASE_DURATION * scaleFactor;
  const adjustedElapsed = enabled ? elapsed : 0;

  const currentStage = STAGES.find(stage => {
    const start = stage.startAt * scaleFactor;
    const end = (stage.startAt + stage.duration) * scaleFactor;
    return adjustedElapsed >= start && adjustedElapsed < end;
  });

  const progress = currentStage
    ? {
        stage: currentStage,
        value: Math.min(
          1,
          (adjustedElapsed - currentStage.startAt * scaleFactor)
            / (currentStage.duration * scaleFactor)
        ),
      }
    : { stage: STAGES.at(-1) ?? null, value: 1 };

  const overall = Math.min(100, (adjustedElapsed / scaledDuration) * 100);

  useEffect(() => {
    if (!enabled) return;
    const id = setInterval(() => setTick(t => t + 1), 500);
    return () => clearInterval(id);
  }, [enabled]);

  return { items, progress, overall, tick };
}

export const FIXTURE_ENTRYPOINT = {
  fn: useAnimation,
  params: [true, 1500, 1.0],
};
