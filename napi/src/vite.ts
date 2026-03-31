import type { ReactCompilerCompilationMode, ReactCompilerPanicThreshold, ReactCompilerSources } from './compiler-options';
import type { Plugin } from 'vite';

import { isFilePartOfSources, withDetectedReanimatedSupport } from './compiler-options';

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
   * - `'syntax'` — only compile functions declared with Flow component/hook syntax
   * - `'annotation'` — only compile functions annotated with `"use memo"`
   * - `'all'` — compile every top-level function
   */
  compilationMode?: ReactCompilerCompilationMode;

  /**
   * Controls how the compiler handles internal errors (bailouts).
   *
   * - `'none'` — silently skip functions that fail to compile (default)
   * - `'all'` — throw on any compilation failure
   */
  panicThreshold?: ReactCompilerPanicThreshold;

  /**
   * The React version to target for generated memoization code.
   *
   * @defaultValue `'19'`
   */
  target?: string;

  /**
   * Optional environment configuration passed to the compiler.
   */
  environment?: Record<string, unknown>;

  /**
   * Optional gating configuration for compiler output.
   */
  gating?: { source: string; importSpecifierName: string };

  /**
   * Optional dynamic gating configuration for `use memo if(...)`.
   */
  dynamicGating?: { source: string };

  /**
   * Automatically enables React Native Reanimated type handling when the module is installed.
   *
   * @defaultValue `true`
   */
  enableReanimatedCheck?: boolean;

  /**
   * Whether to generate source maps for transformed output.
   *
   * @defaultValue `true`
   */
  sourceMap?: boolean;

  /**
   * Restricts which files are processed by the compiler.
   *
   * - When an array of strings, only files whose ID contains one of the
   *   strings are compiled (e.g. `['src/']`).
   * - When a function, it is called with each file ID and must return
   *   `true` to compile that file.
   * - When omitted, all files outside `node_modules` are compiled.
   */
  sources?: ReactCompilerSources;
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
  const codeFilter =
    options.compilationMode === 'annotation' ? /['"]use memo['"]/ : options.compilationMode === 'syntax' ? /\bcomponent\b|\bhook\b/ : /\b[A-Z]|\buse/;

  return {
    name: 'oxc-plugin-react-compiler',
    enforce: 'pre',

    applyToEnvironment(environment) {
      return environment.config.consumer === 'client';
    },

    async transform(code, id) {
      if (!jsExtRe.test(id)) return null;
      if (!isFilePartOfSources(id, options.sources)) {
        return null;
      }

      if (!codeFilter.test(code)) return null;

      const { transform } = await import('#binding');
      const normalizedOptions = withDetectedReanimatedSupport({
        compilationMode: options.compilationMode ?? 'infer',
        panicThreshold: options.panicThreshold ?? 'none',
        target: options.target ?? '19',
        sourceMap: options.sourceMap ?? true,
        environment: options.environment,
        gating: options.gating,
        dynamicGating: options.dynamicGating,
        enableReanimatedCheck: options.enableReanimatedCheck,
      });
      const { enableReanimatedCheck: _enableReanimatedCheck, ...bindingOptions } = normalizedOptions;
      const result = transform(id, code, bindingOptions);

      if (!result.transformed) return null;
      return { code: result.code, map: result.map ?? undefined };
    },
  };
};

export default reactCompilerOxc;
