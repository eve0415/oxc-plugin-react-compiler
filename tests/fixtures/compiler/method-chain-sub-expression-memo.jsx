// @compilationMode(infer)
// Method chain on reactive value should be memoized as a sub-expression.
// Babel extracts Math.floor(...).toString(16).toUpperCase().padStart(2,"0")
// into a separately memoized intermediate, then interpolates it.
// OXC inlines the chain directly in the template literal.
import { useState, useEffect, useRef } from 'react';
function generateBar(pct, total) { return 'x'.repeat(Math.floor(pct * total / 100)); }
function Component({ percentage, index, animate, isLast }) {
  const [progress, setProgress] = useState(0);
  const [visible, setVisible] = useState(!animate);
  const frameRef = useRef(0);
  useEffect(() => {
    if (!animate) { setProgress(percentage); setVisible(true); return; }
    const start = performance.now();
    const step = (t) => {
      const elapsed = t - start;
      if (elapsed < 500) {
        setProgress(percentage * elapsed / 500);
        frameRef.current = requestAnimationFrame(step);
      } else {
        setProgress(percentage);
        setVisible(true);
      }
    };
    frameRef.current = requestAnimationFrame(step);
    return () => cancelAnimationFrame(frameRef.current);
  }, [animate, index, percentage]);
  const bar10 = generateBar(progress, 10);
  const bar15 = generateBar(progress, 15);
  const bar20 = generateBar(progress, 20);
  const hexValue = `0x${Math.floor(progress).toString(16).toUpperCase().padStart(2, "0")}`;
  const cls = `row ${visible ? 'show' : 'hide'}`;
  const transDelay = `${index * 50}ms`;
  return (
    <div className={cls} style={{ transitionDelay: transDelay }}>
      <span className='sm-hide'>{bar10}</span>
      <span className='md-only'>{bar15}</span>
      <span className='lg-only'>{bar20}</span>
      <span>{hexValue}</span>
      <span>{progress.toFixed(1)}%</span>
      {isLast && <div className='border' />}
    </div>
  );
}
export const FIXTURE_ENTRYPOINT = { fn: Component, params: [{}] };
