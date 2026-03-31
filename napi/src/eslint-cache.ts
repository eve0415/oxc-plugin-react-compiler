import { createRequire } from 'node:module';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { isDeepStrictEqual } from 'node:util';

import { isFilePartOfSources, withDetectedReanimatedSupport } from './compiler-options';
import type { NapiLintDiagnostic, OxcReactCompilerOptions } from './eslint-types';

const CACHE_SIZE = 20;
const cache = new Map<string, { sourceText: string; options: BindingLintOptions | undefined; diagnostics: NapiLintDiagnostic[] }>();
const insertionOrder: string[] = [];

type LintFn = (filename: string, source: string, options?: OxcReactCompilerOptions) => NapiLintDiagnostic[];

type BindingLintOptions = Omit<OxcReactCompilerOptions, 'enableReanimatedCheck' | 'sources'>;

let _lint: LintFn | undefined;
const getLint = (): LintFn => {
  if (_lint != null) return _lint;
  // Load the native binding from the dist directory.
  // createRequire is used for ESM→CJS interop with the .node binary loader.
  const __dirname = dirname(fileURLToPath(import.meta.url));
  const bindingPath = resolve(__dirname, '..', 'dist', 'index.js');
  const req = createRequire(bindingPath);
  const binding = req(bindingPath) as { lint: LintFn };
  _lint = binding.lint;
  return _lint;
};

export const getLintResults = (
  filename: string,
  sourceText: string,
  options?: OxcReactCompilerOptions,
): NapiLintDiagnostic[] => {
  if (!isFilePartOfSources(filename, options?.sources)) {
    return [];
  }

  const normalizedOptions = options == null ? undefined : withDetectedReanimatedSupport(options);
  const bindingOptions =
    normalizedOptions == null
      ? undefined
      : (({ enableReanimatedCheck: _enableReanimatedCheck, sources: _sources, ...rest }) => rest)(normalizedOptions);

  const entry = cache.get(filename);
  if (entry != null && entry.sourceText === sourceText && isDeepStrictEqual(entry.options, bindingOptions)) {
    return entry.diagnostics;
  }

  const lint = getLint();
  const diagnostics = lint(filename, sourceText, bindingOptions as BindingLintOptions | undefined);

  // Evict oldest entry if at capacity
  if (cache.size >= CACHE_SIZE && !cache.has(filename)) {
    const oldest = insertionOrder.shift();
    if (oldest != null) {
      cache.delete(oldest);
    }
  }

  cache.set(filename, { sourceText, options: bindingOptions, diagnostics });
  const idx = insertionOrder.indexOf(filename);
  if (idx >= 0) {
    insertionOrder.splice(idx, 1);
  }
  insertionOrder.push(filename);

  return diagnostics;
};
