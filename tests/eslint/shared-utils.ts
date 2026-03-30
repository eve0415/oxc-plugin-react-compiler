import type { Rule, RuleTester as RuleTesterType } from 'eslint';

import { describe, it } from 'vite-plus/test';
import { RuleTester } from 'eslint';
import tsParser from '@typescript-eslint/parser';

/**
 * Template tag that normalizes indentation to match the first non-empty line.
 * Ported from upstream's shared-utils.ts.
 */
export const normalizeIndent = (strings: TemplateStringsArray, ...values: unknown[]): string => {
  let result = '';
  for (let i = 0; i < strings.length; i++) {
    result += strings[i];
    if (i < values.length) {
      result += String(values[i]);
    }
  }
  // Find minimum indentation (excluding empty lines)
  const lines = result.split('\n');
  let minIndent = Infinity;
  for (const line of lines) {
    if (line.trim().length === 0) continue;
    const indent = line.match(/^(\s*)/)?.[1].length ?? 0;
    if (indent < minIndent) minIndent = indent;
  }
  if (minIndent === Infinity) minIndent = 0;
  return lines.map(line => (line.trim().length === 0 ? '' : line.slice(minIndent))).join('\n');
};

/**
 * Escape a string for use in a RegExp, then wrap it in a RegExp.
 * Port of upstream's makeTestCaseError from shared-utils.ts.
 */
export const makeTestCaseError = (reason: string): RuleTesterType.TestCaseError => ({
  message: new RegExp(reason.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')),
});

/**
 * Run ESLint RuleTester within a vitest describe/it block.
 */
export const testRule = (
  name: string,
  rule: Rule.RuleModule,
  tests: {
    valid: RuleTester.ValidTestCase[];
    invalid: RuleTester.InvalidTestCase[];
  },
): void => {
  const tester = new RuleTester({
    languageOptions: {
      ecmaVersion: 2024,
      sourceType: 'module',
      parserOptions: {
        ecmaFeatures: { jsx: true },
      },
    },
  });

  runTests(name, rule, tester, tests);
};

/**
 * Run ESLint RuleTester for TypeScript files within a vitest describe/it block.
 * Uses @typescript-eslint/parser with filename 'test.tsx'.
 */
export const testRuleTs = (
  name: string,
  rule: Rule.RuleModule,
  tests: {
    valid: RuleTester.ValidTestCase[];
    invalid: RuleTester.InvalidTestCase[];
  },
): void => {
  const tester = new RuleTester({
    languageOptions: {
      parser: tsParser,
      parserOptions: {
        ecmaFeatures: { jsx: true },
      },
    },
  });

  runTests(name, rule, tester, tests);
};

const runTests = (
  name: string,
  rule: Rule.RuleModule,
  tester: RuleTester,
  tests: {
    valid: RuleTester.ValidTestCase[];
    invalid: RuleTester.InvalidTestCase[];
  },
): void => {
  describe(name, () => {
    for (const testCase of tests.valid) {
      const testName = typeof testCase === 'string' ? testCase.slice(0, 50) : (testCase.name ?? 'valid case');
      it(`valid: ${testName}`, () => {
        tester.run(name, rule, { valid: [testCase], invalid: [] });
      });
    }
    for (const testCase of tests.invalid) {
      const testName = testCase.name ?? 'invalid case';
      it(`invalid: ${testName}`, () => {
        tester.run(name, rule, { valid: [], invalid: [testCase] });
      });
    }
  });
};
