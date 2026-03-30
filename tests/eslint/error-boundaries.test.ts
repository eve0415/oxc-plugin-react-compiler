import { rules } from '../../napi/src/eslint.js';
import { makeTestCaseError, normalizeIndent, testRule } from './shared-utils.js';

testRule('error-boundaries', rules['error-boundaries'], {
  valid: [],
  invalid: [
    {
      name: 'JSX in try blocks are warned against',
      code: normalizeIndent`
        function Component(props) {
          let el;
          try {
            el = <Child />;
          } catch {
            return null;
          }
          return el;
        }
      `,
      errors: [makeTestCaseError('Avoid constructing JSX within try/catch')],
    },
  ],
});
