// Side-effectful expression passed to useRef() should be sentinel-memoized.
// Babel: _c(9) — wraps the useRef argument in a sentinel guard
// OXC:   _c(8) — passes the expression directly to useRef
import { useCallback, useEffect, useRef, useState } from 'react';
const STORAGE_KEY = 'debug-mode';
function useDebugMode() {
  const [state, setState] = useState({ enabled: false, index: 0 });
  const needsSyncRef = useRef(
    globalThis.window !== undefined && localStorage.getItem(STORAGE_KEY) === 'true'
  );
  const enable = useCallback(() => {
    setState(prev => ({ ...prev, enabled: true }));
    localStorage.setItem(STORAGE_KEY, 'true');
    needsSyncRef.current = false;
  }, []);
  const disable = useCallback(() => {
    setState(prev => ({ ...prev, enabled: false }));
    localStorage.removeItem(STORAGE_KEY);
  }, []);
  const toggle = useCallback(() => {
    setState(prev => {
      const next = !prev.enabled;
      if (next) localStorage.setItem(STORAGE_KEY, 'true');
      else localStorage.removeItem(STORAGE_KEY);
      return { ...prev, enabled: next };
    });
  }, []);
  useEffect(() => {
    if (needsSyncRef.current) {
      setState(prev => ({ ...prev, enabled: true }));
      needsSyncRef.current = false;
    }
  }, []);
  return { state, enable, disable, toggle };
}
export const FIXTURE_ENTRYPOINT = { fn: useDebugMode, params: [] };
