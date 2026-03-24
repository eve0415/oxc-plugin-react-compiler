import { readFile, readdir } from 'node:fs/promises';
import { basename, join, relative } from 'node:path';

import { compareAST, parseJS } from './ast-compare.js';

export interface JsOutputFile {
  key: string;
  path: string;
  source: string;
}

export interface JsMismatchDiagnostic {
  key: string;
  expectedPath: string;
  actualPath: string;
  expectedBytes: number;
  actualBytes: number;
  firstDiffLine?: number;
  expectedLine?: string;
  actualLine?: string;
  astDifferenceCount: number;
  astDifferencesPreview: string[];
}

const HASHED_JS_RE = /^(.*)-[A-Za-z0-9_-]{6,}(\.js)$/;

export const collectJsFiles = async (dir: string): Promise<string[]> => {
  const results: string[] = [];
  try {
    const entries = await readdir(dir, { withFileTypes: true, recursive: true });
    for (const entry of entries) {
      if (entry.isFile() && entry.name.endsWith('.js')) {
        results.push(join(entry.parentPath ?? dir, entry.name));
      }
    }
  } catch {
    // Directory may not exist yet.
  }
  return results.sort();
};

export const normalizeJsChunkKey = (pathFromRoot: string): string => {
  const normalized = pathFromRoot.replaceAll('\\', '/');
  return normalized.replace(HASHED_JS_RE, '$1$2');
};

export const readJsOutputs = async (dir: string): Promise<Map<string, JsOutputFile>> => {
  const outputs = new Map<string, JsOutputFile>();
  for (const filePath of await collectJsFiles(dir)) {
    const key = normalizeJsChunkKey(relative(dir, filePath));
    const source = await readFile(filePath, 'utf8');
    outputs.set(key, { key, path: filePath, source });
  }
  return outputs;
};

export const compareExactJsOutputs = async (
  expectedDir: string,
  actualDir: string,
): Promise<JsMismatchDiagnostic[]> => {
  const [expectedFiles, actualFiles] = await Promise.all([
    readJsOutputs(expectedDir),
    readJsOutputs(actualDir),
  ]);

  const allKeys = new Set([...expectedFiles.keys(), ...actualFiles.keys()]);
  const mismatches: JsMismatchDiagnostic[] = [];

  for (const key of [...allKeys].sort()) {
    const expected = expectedFiles.get(key);
    const actual = actualFiles.get(key);

    if (!expected || !actual) {
      mismatches.push({
        key,
        expectedPath: expected?.path ?? '<missing>',
        actualPath: actual?.path ?? '<missing>',
        expectedBytes: expected?.source.length ?? 0,
        actualBytes: actual?.source.length ?? 0,
        astDifferenceCount: -1,
        astDifferencesPreview: ['missing output file'],
      });
      continue;
    }

    if (expected.source === actual.source) {
      continue;
    }

    const expectedLines = expected.source.split('\n');
    const actualLines = actual.source.split('\n');
    const maxLines = Math.max(expectedLines.length, actualLines.length);
    let firstDiffLine: number | undefined;
    let expectedLine: string | undefined;
    let actualLine: string | undefined;

    for (let i = 0; i < maxLines; i++) {
      const expectedAtLine = expectedLines[i] ?? '';
      const actualAtLine = actualLines[i] ?? '';
      if (expectedAtLine !== actualAtLine) {
        firstDiffLine = i + 1;
        expectedLine = expectedAtLine;
        actualLine = actualAtLine;
        break;
      }
    }

    const astResult = compareAST(parseJS(actual.source), parseJS(expected.source));
    mismatches.push({
      key,
      expectedPath: expected.path,
      actualPath: actual.path,
      expectedBytes: expected.source.length,
      actualBytes: actual.source.length,
      firstDiffLine,
      expectedLine,
      actualLine,
      astDifferenceCount: astResult.differences.length,
      astDifferencesPreview: astResult.differences
        .slice(0, 5)
        .map(
          (diff) =>
            `${diff.path}: ${diff.kind} (${diff.expected ?? 'N/A'} -> ${diff.actual ?? 'N/A'})`,
        ),
    });
  }

  return mismatches;
};

export const logExactMismatchSummary = (
  label: string,
  mismatches: JsMismatchDiagnostic[],
): void => {
  if (mismatches.length === 0) {
    return;
  }

  console.log(`\n  ${label}: ${String(mismatches.length)} exact JS mismatch(es)`);
  for (const mismatch of mismatches.slice(0, 10)) {
    console.log(
      `    ${mismatch.key} — ${String(mismatch.expectedBytes)} bytes vs ${String(mismatch.actualBytes)} bytes`,
    );
    if (mismatch.firstDiffLine !== undefined) {
      console.log(
        `      line ${String(mismatch.firstDiffLine)} expected: ${mismatch.expectedLine ?? ''}`,
      );
      console.log(
        `      line ${String(mismatch.firstDiffLine)} actual:   ${mismatch.actualLine ?? ''}`,
      );
    }
    if (mismatch.astDifferenceCount >= 0) {
      console.log(`      AST differences: ${String(mismatch.astDifferenceCount)}`);
      for (const preview of mismatch.astDifferencesPreview) {
        console.log(`        ${preview}`);
      }
    } else {
      for (const preview of mismatch.astDifferencesPreview) {
        console.log(`      ${preview}`);
      }
    }
  }

  if (mismatches.length > 10) {
    console.log(`    ... and ${String(mismatches.length - 10)} more`);
  }
};

export const buildLabelFromPath = (filePath: string): string => basename(filePath);
