// A closure defined inside a scope block (with useEffect/useRef) and called
// multiple times should not leak extra memoization blocks.
// Babel inlines calls inside the scope block.
// OXC creates separate memo blocks for each call, inflating the cache.
import { useEffect, useRef, useState } from 'react';
function Component({ className = '', animate = true }) {
  const [isAnimating, setIsAnimating] = useState(animate);
  const pathRefs = useRef([]);
  useEffect(() => {
    if (!animate) return;
    for (const p of pathRefs.current) {
      if (p) { const len = p.getTotalLength(); p.style.strokeDasharray = String(len); p.style.strokeDashoffset = String(len); }
    }
    const timer = setTimeout(() => setIsAnimating(false), 2000);
    return () => clearTimeout(timer);
  }, [animate]);
  const setRef = (i) => (el) => { pathRefs.current[i] = el; };
  const pathStyle = (i) => ({
    opacity: isAnimating ? 0 : 1,
    transition: `stroke-dashoffset 0.8s ease-out ${i * 0.1}s, fill 0.3s ease ${0.8 + i * 0.1}s`,
  });
  const anim = animate ? 'anim' : '';
  return (
    <svg viewBox='0 0 100 100' className={`${className} wrap`} role='img'>
      <path ref={setRef(0)} style={pathStyle(0)} d='M10,10' />
      <path ref={setRef(1)} style={pathStyle(1)} d='M20,20' />
      <path ref={setRef(2)} style={pathStyle(2)} d='M30,30' />
      <path ref={setRef(3)} style={pathStyle(3)} d='M40,40' />
      <path ref={setRef(4)} style={pathStyle(4)} d='M50,50' />
      <g className={anim}>
        <path ref={setRef(5)} style={pathStyle(5)} d='M60,60' />
      </g>
    </svg>
  );
}
export const FIXTURE_ENTRYPOINT = { fn: Component, params: [{}] };
