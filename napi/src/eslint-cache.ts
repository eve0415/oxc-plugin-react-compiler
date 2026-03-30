import type { NapiLintDiagnostic } from './eslint-types';

const CACHE_SIZE = 20;
const cache = new Map<string, { sourceText: string; diagnostics: NapiLintDiagnostic[] }>();
const insertionOrder: string[] = [];

export const getLintResults = (filename: string, sourceText: string): NapiLintDiagnostic[] => {
  const entry = cache.get(filename);
  if (entry != null && entry.sourceText === sourceText) {
    return entry.diagnostics;
  }

  // Dynamic import to avoid loading native binding until needed.
  // eslint-disable-next-line @typescript-eslint/no-require-imports
  const { lint } = require('#binding') as { lint: (filename: string, source: string) => NapiLintDiagnostic[] };
  const diagnostics = lint(filename, sourceText);

  // Evict oldest entry if at capacity
  if (cache.size >= CACHE_SIZE && !cache.has(filename)) {
    const oldest = insertionOrder.shift();
    if (oldest != null) {
      cache.delete(oldest);
    }
  }

  cache.set(filename, { sourceText, diagnostics });
  const idx = insertionOrder.indexOf(filename);
  if (idx >= 0) {
    insertionOrder.splice(idx, 1);
  }
  insertionOrder.push(filename);

  return diagnostics;
};
