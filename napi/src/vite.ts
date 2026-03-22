import type { TransformOptions } from '../index.js';
import type { Plugin } from 'vite-plus';

/**
 * Options for the OXC React Compiler Vite plugin.
 *
 * Controls which files are compiled and how the React Compiler
 * optimizes components and hooks.
 */
export interface ReactCompilerOxcOptions {
  /**
   * Controls which functions are compiled.
   *
   * - `'infer'` — compile functions that look like React components or hooks (default)
   * - `'annotation'` — only compile functions annotated with `"use memo"`
   * - `'all'` — compile every top-level function
   */
  compilationMode?: TransformOptions['compilationMode'];

  /**
   * Controls how the compiler handles internal errors (bailouts).
   *
   * - `'none'` — silently skip functions that fail to compile (default)
   * - `'all'` — throw on any compilation failure
   */
  panicThreshold?: TransformOptions['panicThreshold'];

  /**
   * The React version to target for generated memoization code.
   *
   * @defaultValue `'19'`
   */
  target?: TransformOptions['target'];

  /**
   * Optional environment configuration passed to the compiler.
   */
  environment?: Record<string, unknown>;

  /**
   * Restricts which files are processed by the compiler.
   *
   * - When an array of strings, only files whose ID contains one of the
   *   strings are compiled (e.g. `['src/']`).
   * - When a function, it is called with each file ID and must return
   *   `true` to compile that file.
   * - When omitted, all files outside `node_modules` are compiled.
   */
  sources?: string[] | ((id: string) => boolean);
}

const jsExtRe = /\.[jt]sx?$/;

/**
 * Vite plugin that runs the OXC-based React Compiler on source files.
 *
 * Runs with `enforce: 'pre'` so it processes source before
 * `@vitejs/plugin-react`. When that plugin has no Babel config,
 * it uses the fast OXC path for Fast Refresh — no Babel required.
 *
 * Supports the Vite 8 environment API via `applyToEnvironment`,
 * restricting compilation to client-side environments only.
 *
 * @example
 * ```ts
 * import { defineConfig } from 'vite'
 * import react from '@vitejs/plugin-react'
 * import { reactCompilerOxc } from 'oxc-plugin-react-compiler/vite'
 *
 * export default defineConfig({
 *   plugins: [reactCompilerOxc(), react()],
 * })
 * ```
 *
 * @param options - Plugin options controlling compilation behavior.
 * @returns A Vite plugin instance.
 */
export const reactCompilerOxc = (options: ReactCompilerOxcOptions = {}): Plugin => {
  const codeFilter = options.compilationMode === 'annotation' ? /['"]use memo['"]/ : /\b[A-Z]|\buse/;

  return {
    name: 'oxc-plugin-react-compiler',
    enforce: 'pre',

    applyToEnvironment(environment) {
      return environment.config.consumer === 'client';
    },

    async transform(code, id) {
      if (!jsExtRe.test(id)) return null;
      if (options.sources) {
        if (Array.isArray(options.sources)) {
          if (!options.sources.some(s => id.includes(s))) return null;
        } else if (typeof options.sources === 'function') {
          if (!options.sources(id)) return null;
        }
      } else if (id.includes('node_modules')) {
        return null;
      }

      if (!codeFilter.test(code)) return null;

      const { transform } = await import('../index.js');
      const result = transform(id, code, {
        compilationMode: options.compilationMode ?? 'infer',
        panicThreshold: options.panicThreshold ?? 'none',
        target: options.target ?? '19',
      });

      if (!result.transformed) return null;
      return { code: result.code, map: result.map ?? undefined };
    },
  };
};

export default reactCompilerOxc;
