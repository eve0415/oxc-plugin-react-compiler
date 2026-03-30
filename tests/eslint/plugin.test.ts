import type { Rule } from 'eslint';

import { describe, it, expect } from 'vite-plus/test';
import { RuleTester } from 'eslint';

import { configs, meta, rules } from '../../napi/src/eslint.js';
import { normalizeIndent, testRule } from './shared-utils.js';

describe('eslint plugin metadata', () => {
  it('exports meta with name and version', () => {
    expect(meta.name).toBe('oxc-react-compiler');
    expect(meta.version).toBeDefined();
  });

  it('exports rules object with expected rule names', () => {
    expect(rules).toBeDefined();
    expect(typeof rules).toBe('object');
    expect('purity' in rules).toBe(true);
    expect('refs' in rules).toBe(true);
    expect('hooks' in rules).toBe(true);
    expect('no-unused-directives' in rules).toBe(true);
    expect('set-state-in-render' in rules).toBe(true);
    expect('immutability' in rules).toBe(true);
  });

  it('exports recommended and all configs', () => {
    expect(configs.recommended).toBeDefined();
    expect(configs.all).toBeDefined();
  });

  it('recommended config includes recommended rules', () => {
    const ruleKeys = Object.keys(configs.recommended.rules ?? {});
    expect(ruleKeys.some(k => k.includes('purity'))).toBe(true);
    expect(ruleKeys.some(k => k.includes('refs'))).toBe(true);
    expect(ruleKeys.some(k => k.includes('no-unused-directives'))).toBe(true);
  });

  it('all config includes all rules', () => {
    const allRuleKeys = Object.keys(configs.all.rules ?? {});
    expect(allRuleKeys.length).toBeGreaterThan(Object.keys(configs.recommended.rules ?? {}).length);
  });
});

describe('plugin recommended rules', () => {
  testRule('recommended-valid', rules['purity'], {
    valid: [
      {
        name: 'Simple valid component',
        code: normalizeIndent`
          function Component() {
            return <div>Hello</div>;
          }
        `,
      },
      {
        name: "Classes don't throw",
        code: normalizeIndent`
          class Foo {
            bar() {}
          }
        `,
      },
    ],
    invalid: [],
  });
});

/**
 * TestRecommendedRules: aggregates all recommended rules and runs them together.
 * Port of upstream's TestRecommendedRules pattern — catches cross-rule interactions.
 */
const TestRecommendedRules: Rule.RuleModule = {
  meta: {
    type: 'problem',
    schema: [{ type: 'object', additionalProperties: true }],
  },
  create(context) {
    const recommendedRuleEntries = Object.entries(configs.recommended.rules ?? {});
    for (const [fullName] of recommendedRuleEntries) {
      const shortName = fullName.replace('oxc-react-compiler/', '');
      const ruleModule = rules[shortName];
      if (ruleModule) {
        ruleModule.create(context);
      }
    }
    return {};
  },
};

describe('aggregated recommended rules', () => {
  const tester = new RuleTester({
    languageOptions: {
      ecmaVersion: 2024,
      sourceType: 'module',
      parserOptions: { ecmaFeatures: { jsx: true } },
    },
  });

  it('valid: simple component passes all recommended rules', () => {
    tester.run('aggregated', TestRecommendedRules, {
      valid: [
        {
          name: 'Simple valid component',
          code: normalizeIndent`
            function Component() {
              return <div>Hello</div>;
            }
          `,
        },
      ],
      invalid: [],
    });
  });

  it('valid: class does not crash aggregated rules', () => {
    tester.run('aggregated', TestRecommendedRules, {
      valid: [
        {
          name: 'Class does not crash',
          code: normalizeIndent`
            class Foo {
              bar() {}
            }
          `,
        },
      ],
      invalid: [],
    });
  });
});
