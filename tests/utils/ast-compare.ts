import * as acorn from 'acorn';

export interface Difference {
  path: string;
  kind: 'type_mismatch' | 'value_mismatch' | 'missing_node' | 'extra_node';
  expected?: string;
  actual?: string;
}

export interface CompareResult {
  match: boolean;
  differences: Difference[];
}

/** Keys to ignore when comparing AST nodes. */
const IGNORED_KEYS = new Set([
  'start',
  'end',
  'loc',
  'range',
  'raw',
  'extra',
  'leadingComments',
  'trailingComments',
  'innerComments',
  'comments',
  'sourceType',
]);

/**
 * Parse JavaScript source into an AST using acorn.
 */
export const parseJS = (source: string): acorn.Program =>
  acorn.parse(source, {
    ecmaVersion: 'latest',
    sourceType: 'module',
  });

/**
 * Compare two AST nodes structurally, ignoring positions and comments.
 *
 * @returns A {@link CompareResult} with `match: true` if structurally identical.
 */
export const compareAST = (a: unknown, b: unknown): CompareResult => {
  const differences: Difference[] = [];
  walk(a, b, '$', differences);
  return { match: differences.length === 0, differences };
};

const walk = (a: unknown, b: unknown, path: string, diffs: Difference[]): void => {
  if (a === b) return;

  if (a === null && b === null) return;

  if (a === null || b === null) {
    diffs.push({
      path,
      kind: a === null ? 'missing_node' : 'extra_node',
      expected: String(a),
      actual: String(b),
    });
    return;
  }

  if (typeof a !== typeof b) {
    diffs.push({
      path,
      kind: 'type_mismatch',
      expected: typeof a,
      actual: typeof b,
    });
    return;
  }

  if (typeof a !== 'object') {
    if (a !== b) {
      diffs.push({
        path,
        kind: 'value_mismatch',
        /* eslint-disable @typescript-eslint/no-base-to-string -- guarded by typeof !== 'object' above */
        expected: String(a),
        actual: String(b),
        /* eslint-enable @typescript-eslint/no-base-to-string */
      });
    }
    return;
  }

  if (Array.isArray(a)) {
    if (!Array.isArray(b)) {
      diffs.push({ path, kind: 'type_mismatch', expected: 'array', actual: 'object' });
      return;
    }
    const len = Math.max(a.length, b.length);
    for (let i = 0; i < len; i++) {
      if (i >= a.length) {
        diffs.push({ path: `${path}[${String(i)}]`, kind: 'extra_node', actual: nodeType(b[i]) });
      } else if (i >= b.length) {
        diffs.push({ path: `${path}[${String(i)}]`, kind: 'missing_node', expected: nodeType(a[i]) });
      } else {
        walk(a[i], b[i], `${path}[${String(i)}]`, diffs);
      }
    }
    return;
  }

  // Both a and b are non-null objects (guards above ensure this).
  // The walker fundamentally operates on unknown-shaped AST nodes,
  // so we need property access via Object.keys/entries.
  /* eslint-disable @typescript-eslint/no-unsafe-type-assertion */
  const aRecord = a as Record<string, unknown>;
  const bRecord = b as Record<string, unknown>;
  /* eslint-enable @typescript-eslint/no-unsafe-type-assertion */
  const keys = new Set([...Object.keys(aRecord), ...Object.keys(bRecord)]);

  for (const key of keys) {
    if (IGNORED_KEYS.has(key)) continue;
    walk(aRecord[key], bRecord[key], `${path}.${key}`, diffs);
  }
};

const nodeType = (node: unknown): string => {
  if (node !== null && typeof node === 'object' && 'type' in node) {
    const typed = node as object & { type: unknown };
    return String(typed.type);
  }
  return String(node);
};
