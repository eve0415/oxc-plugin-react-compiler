## Input

```javascript
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
```

## Code

```javascript
import { c as _c } from "react/compiler-runtime";
// OXC fails to create reactive scopes for Array.find() and conditional
// expressions between hook calls (useMemo / useEffect). Babel wraps
// STAGES.find() and the ternary using it in reactive scopes; OXC emits
// them as plain recomputation every render, losing 7 cache slots.
import { useEffect, useMemo, useState } from 'react';
const STAGES = [{
  name: 'init',
  startAt: 0,
  duration: 1000
}, {
  name: 'load',
  startAt: 1000,
  duration: 2000
}, {
  name: 'done',
  startAt: 3000,
  duration: 500
}];
const BASE_DURATION = 7000;
function useAnimation(enabled, elapsed, scaleFactor) {
  const $ = _c(17);
  const [tick, setTick] = useState(0);
  let t0;
  if ($[0] !== scaleFactor) {
    t0 = STAGES.map(s => ({
      ...s,
      scaled: s.duration * scaleFactor
    }));
    $[0] = scaleFactor;
    $[1] = t0;
  } else {
    t0 = $[1];
  }
  const items = t0;
  const scaledDuration = BASE_DURATION * scaleFactor;
  const adjustedElapsed = enabled ? elapsed : 0;
  let t1;
  if ($[2] !== adjustedElapsed || $[3] !== scaleFactor) {
    t1 = STAGES.find(stage => {
      const start = stage.startAt * scaleFactor;
      const end = (stage.startAt + stage.duration) * scaleFactor;
      return adjustedElapsed >= start && adjustedElapsed < end;
    });
    $[2] = adjustedElapsed;
    $[3] = scaleFactor;
    $[4] = t1;
  } else {
    t1 = $[4];
  }
  const currentStage = t1;
  let t2;
  if ($[5] !== adjustedElapsed || $[6] !== currentStage || $[7] !== scaleFactor) {
    t2 = currentStage ? {
      stage: currentStage,
      value: Math.min(1, (adjustedElapsed - currentStage.startAt * scaleFactor) / (currentStage.duration * scaleFactor))
    } : {
      stage: STAGES.at(-1) ?? null,
      value: 1
    };
    $[5] = adjustedElapsed;
    $[6] = currentStage;
    $[7] = scaleFactor;
    $[8] = t2;
  } else {
    t2 = $[8];
  }
  const progress = t2;
  const overall = Math.min(100, adjustedElapsed / scaledDuration * 100);
  let t3;
  let t4;
  if ($[9] !== enabled) {
    t3 = () => {
      if (!enabled) {
        return;
      }
      const id = setInterval(() => setTick(_temp), 500);
      return () => clearInterval(id);
    };
    t4 = [enabled];
    $[9] = enabled;
    $[10] = t3;
    $[11] = t4;
  } else {
    t3 = $[10];
    t4 = $[11];
  }
  useEffect(t3, t4);
  let t5;
  if ($[12] !== items || $[13] !== overall || $[14] !== progress || $[15] !== tick) {
    t5 = {
      items,
      progress,
      overall,
      tick
    };
    $[12] = items;
    $[13] = overall;
    $[14] = progress;
    $[15] = tick;
    $[16] = t5;
  } else {
    t5 = $[16];
  }
  return t5;
}
function _temp(t) {
  return t + 1;
}
export const FIXTURE_ENTRYPOINT = {
  fn: useAnimation,
  params: [true, 1500, 1.0]
};
```
