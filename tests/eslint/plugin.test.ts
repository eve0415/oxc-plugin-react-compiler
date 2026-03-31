import type { Rule } from 'eslint';

import { describe, it, expect } from 'vite-plus/test';
import { RuleTester } from 'eslint';

import { configs, meta, rules } from '../../napi/src/eslint.js';
import { makeTestCaseError, normalizeIndent, testRule } from './shared-utils.js';

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
    hasSuggestions: true,
    schema: [{ type: 'object', additionalProperties: true }],
  },
  create(context) {
    const pluginRules = configs.recommended.plugins?.['oxc-react-compiler']?.rules ?? {};
    for (const ruleModule of Object.values(pluginRules)) {
      ruleModule.create(context);
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

  it('invalid: multiple diagnostics within the same file are surfaced', () => {
    tester.run('aggregated', TestRecommendedRules, {
      valid: [],
      invalid: [
        {
          name: 'Multiple diagnostics within the same file',
          code: normalizeIndent`
            function useConditional1() {
              'use memo';
              return cond ?? useConditionalHook();
            }
            function useConditional2(props) {
              'use memo';
              return props.cond && useConditionalHook();
            }
          `,
          errors: [
            makeTestCaseError('Hooks must always be called in a consistent order'),
            makeTestCaseError('Hooks must always be called in a consistent order'),
          ],
        },
      ],
    });
  });

  it('invalid: multiple diagnostic kinds from the same function are surfaced', () => {
    tester.run('aggregated', TestRecommendedRules, {
      valid: [],
      invalid: [
        {
          name: 'Multiple diagnostic kinds from the same function',
          code: normalizeIndent`
            import Child from './Child';
            function Component() {
              const result = cond ?? useConditionalHook();
              return <>
                {Child(result)}
              </>;
            }
          `,
          errors: [makeTestCaseError('Hooks must always be called in a consistent order')],
        },
      ],
    });
  });

  it("invalid: 'use no forget' does not disable lint rules", () => {
    tester.run('aggregated', TestRecommendedRules, {
      valid: [],
      invalid: [
        {
          name: "'use no forget' does not disable lint rules",
          code: normalizeIndent`
            function Component() {
              'use no forget';
              return cond ?? useConditionalHook();
            }
          `,
          errors: [makeTestCaseError('Hooks must always be called in a consistent order')],
        },
      ],
    });
  });

  it('invalid: pipeline errors are reported', () => {
    tester.run('aggregated', TestRecommendedRules, {
      valid: [],
      invalid: [
        {
          name: 'Pipeline errors are reported',
          code: normalizeIndent`
            import useMyEffect from 'useMyEffect';
            import {AUTODEPS} from 'react';
            function Component({a}) {
              'use no memo';
              useMyEffect(() => console.log(a.b), AUTODEPS);
              return <div>Hello world</div>;
            }
          `,
          options: [
            {
              environment: {
                inferEffectDependencies: [
                  {
                    function: {
                      source: 'useMyEffect',
                      importSpecifierName: 'default',
                    },
                    autodepsIndex: 1,
                  },
                ],
              },
            },
          ],
          errors: [{ message: /Cannot infer dependencies of this effect/ }],
        },
      ],
    });
  });

  it('invalid: multiple non-fatal useMemo diagnostics are surfaced', () => {
    tester.run('aggregated', TestRecommendedRules, {
      valid: [],
      invalid: [
        {
          name: 'Multiple non-fatal useMemo diagnostics are surfaced',
          code: normalizeIndent`
            import {useMemo, useState} from 'react';

            function Component({item, cond}) {
              const [prevItem, setPrevItem] = useState(item);
              const [state, setState] = useState(0);

              useMemo(() => {
                if (cond) {
                  setPrevItem(item);
                  setState(0);
                }
              }, [cond, item, init]);

              return <Child x={state} />;
            }
          `,
          errors: [makeTestCaseError('useMemo() callbacks must return a value')],
        },
      ],
    });
  });

});

describe('options passthrough', () => {
  testRule('capitalized-calls-with-options', rules['capitalized-calls'], {
    valid: [
      {
        name: 'Capitalized call is fine when validation is off (default)',
        code: normalizeIndent`
          function Component() {
            const x = Foo();
            return <div>{x}</div>;
          }
        `,
        // No options → validateNoCapitalizedCalls is None → validation off
      },
    ],
    invalid: [
      {
        name: 'Capitalized call is flagged when validation is explicitly enabled',
        code: normalizeIndent`
          function Component() {
            const x = Foo();
            return <div>{x}</div>;
          }
        `,
        options: [{ environment: { validateNoCapitalizedCalls: [] } }],
        errors: [{ message: /Capitalized/ }],
      },
    ],
  });

  testRule('infer-effect-deps-passthrough', rules['set-state-in-render'], {
    valid: [
      {
        name: 'inferEffectDependencies config is accepted without error',
        code: normalizeIndent`
          function Component() {
            return <div>Hello</div>;
          }
        `,
        options: [
          {
            environment: {
              inferEffectDependencies: [
                {
                  function: { source: 'shared-runtime', importSpecifierName: 'useSpecialEffect' },
                  autodepsIndex: 1,
                },
              ],
            },
          },
        ],
      },
    ],
    invalid: [],
  });

  testRule('top-level-option-passthrough', rules['set-state-in-render'], {
    valid: [
      {
        name: 'panicThreshold, gating, and dynamicGating options are accepted',
        code: normalizeIndent`
          function Component() {
            return <div>Hello</div>;
          }
        `,
        options: [
          {
            panicThreshold: 'all',
            gating: {
              source: 'feature-flags',
              importSpecifierName: 'isForgetEnabled',
            },
            dynamicGating: {
              source: 'feature-flags',
            },
          },
        ],
      },
    ],
    invalid: [],
  });
});
