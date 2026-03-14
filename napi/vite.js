import { transform } from './index.js';

/**
 * Vite v8 plugin for React Compiler (OXC-based).
 *
 * Runs with `enforce: 'pre'` so it processes source files before
 * `@vitejs/plugin-react`. When `@vitejs/plugin-react` has no babel
 * config, it uses the fast OXC path for Fast Refresh.
 *
 * @param {object} options
 * @param {'infer' | 'annotation' | 'all'} [options.compilationMode='infer']
 * @param {'none' | 'all'} [options.panicThreshold='none']
 * @param {string} [options.target='19']
 * @param {object} [options.environment={}]
 * @param {string[] | ((id: string) => boolean)} [options.sources]
 */
export default function reactCompilerOxc(options = {}) {
  return {
    name: 'oxc-plugin-react-compiler',
    enforce: 'pre',
    transform(code, id) {
      if (!/\.[jt]sx?$/.test(id)) return null;
      if (options.sources) {
        if (Array.isArray(options.sources)) {
          if (!options.sources.some(s => id.includes(s))) return null;
        } else if (typeof options.sources === 'function') {
          if (!options.sources(id)) return null;
        }
      } else if (id.includes('node_modules')) {
        return null;
      }
      const result = transform(id, code, {
        compilationMode: options.compilationMode ?? 'infer',
        panicThreshold: options.panicThreshold ?? 'none',
        target: options.target ?? '19',
      });
      if (!result.transformed) return null;
      return { code: result.code, map: result.map };
    },
  };
}
