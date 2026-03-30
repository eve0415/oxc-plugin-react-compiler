import { rules } from '../../napi/src/eslint.js';
import { makeTestCaseError, normalizeIndent, testRule } from './shared-utils.js';

// Impure function calls are categorized as Immutability in our compiler
testRule('immutability-impure-calls', rules['immutability'], {
  valid: [
    {
      name: 'Pure function calls are allowed',
      code: normalizeIndent`
        function Component() {
          const x = Math.abs(-1);
          return <div>{x}</div>;
        }
      `,
    },
  ],
  invalid: [
    {
      name: 'Date.now() is impure',
      code: normalizeIndent`
        function Component() {
          const x = Date.now();
          return <div>{x}</div>;
        }
      `,
      options: [{ environment: { validateNoImpureFunctionsInRender: true } }],
      errors: [makeTestCaseError('Cannot call impure function `Date.now` during render')],
    },
    {
      name: 'Math.random() is impure',
      code: normalizeIndent`
        function Component() {
          const x = Math.random();
          return <div>{x}</div>;
        }
      `,
      options: [{ environment: { validateNoImpureFunctionsInRender: true } }],
      errors: [makeTestCaseError('Cannot call impure function `Math.random` during render')],
    },
    {
      name: 'performance.now() is impure',
      code: normalizeIndent`
        function Component() {
          const x = performance.now();
          return <div>{x}</div>;
        }
      `,
      options: [{ environment: { validateNoImpureFunctionsInRender: true } }],
      errors: [makeTestCaseError('Cannot call impure function `performance.now` during render')],
    },
  ],
});
