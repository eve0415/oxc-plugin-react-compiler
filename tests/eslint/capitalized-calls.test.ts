import { rules } from '../../napi/src/eslint.js';

import { makeTestCaseError, normalizeIndent, testRule } from './shared-utils.js';

testRule('capitalized-calls', rules['capitalized-calls'], {
  valid: [
    {
      name: 'Normal JSX component usage',
      code: normalizeIndent`
        function Component() {
          return <Foo />;
        }
      `,
    },
  ],
  invalid: [
    {
      name: 'Capitalized function call instead of JSX',
      code: normalizeIndent`
        function Component() {
          const x = Foo();
          return <div>{x}</div>;
        }
      `,
      options: [{ environment: { validateNoCapitalizedCalls: [] } }],
      errors: [makeTestCaseError('Capitalized functions are reserved for components')],
    },
    {
      name: 'Capitalized method call instead of JSX',
      code: normalizeIndent`
        function Component() {
          const x = MyModule.Child();
          return <div>{x}</div>;
        }
      `,
      options: [{ environment: { validateNoCapitalizedCalls: [] } }],
      errors: [makeTestCaseError('Capitalized functions are reserved for components')],
    },
    {
      name: 'Multiple capitalized-call violations in one function are deduplicated',
      code: normalizeIndent`
        function Component() {
          return <>
            {Child1()}
            {MyModule.Child2()}
          </>;
        }
      `,
      options: [{ environment: { validateNoCapitalizedCalls: [] } }],
      errors: [makeTestCaseError('Capitalized functions are reserved for components')],
    },
  ],
});
