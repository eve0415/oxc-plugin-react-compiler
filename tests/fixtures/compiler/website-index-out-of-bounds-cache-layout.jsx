// Reduced from /tmp/website/src/routes/-components/NotFound/Aftermath/ErrorVisualizations/IndexOutOfBounds/index-out-of-bounds.tsx
// Real-world drift: Babel and OXC disagree on cache layout around a sentinel
// Array.from grid nested among other memoized siblings. Babel keeps the
// Array.from result in its own cache slot; OXC currently folds it into the
// mapped JSX path and emits fewer cache reads.
import { useState } from 'react';

const toHexValue = index => `0x${index.toString(16).padStart(2, '0')}`;

function Component() {
  const arraySize = 10;
  const targetIndex = 404;
  const [cursorPosition] = useState(11);
  const [corruptedText] = useState('0xDEAD');

  return (
    <div>
      <div>
        <span>int</span>[] data = <span>new</span> <span>int</span>[{arraySize}];
      </div>

      <div className="grid">
        {Array.from({ length: arraySize }).map((_, i) => (
          <div
            key={i}
            className={
              cursorPosition === i
                ? 'active'
                : cursorPosition > i
                  ? 'past'
                  : 'future'
            }
          >
            <span>{toHexValue(i)}</span>
            <span>[{i}]</span>
          </div>
        ))}

        <div>
          <span>→</span>
        </div>

        <div
          className={cursorPosition >= arraySize ? 'error' : 'idle'}
          style={
            cursorPosition >= arraySize
              ? { backgroundImage: 'repeating-linear-gradient(red, red)' }
              : undefined
          }
        >
          <span>{cursorPosition >= arraySize ? corruptedText : '???'}</span>
          <span>[{targetIndex}]</span>
          {cursorPosition >= arraySize && <div>!</div>}
        </div>
      </div>
    </div>
  );
}

export const FIXTURE_ENTRYPOINT = {
  fn: Component,
  params: [{}],
};
