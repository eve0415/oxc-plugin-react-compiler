import { rules } from '../../napi/src/eslint.js';

import { normalizeIndent, testRule } from './shared-utils.js';

// Use real ESLint rule names (built-in) to avoid ESLint 10's "rule not found"
// errors for eslint-disable comments. Default rules (react-hooks/*) are tested
// at the Rust level in pipeline.rs integration tests.
const suppressionOpts = [{ eslintSuppressionRules: ['no-console'] }];

testRule('rule-suppression', rules['rule-suppression'], {
  valid: [
    {
      name: 'No eslint-disable comments produces no suppression diagnostic',
      code: normalizeIndent`
        function Component() {
          return <div>Hello</div>;
        }
      `,
      options: suppressionOpts,
    },
    {
      name: 'Empty eslintSuppressionRules disables suppression checking',
      code: normalizeIndent`
        function Component() {
          // eslint-disable-next-line no-console
          return <div>Hello</div>;
        }
      `,
      options: [{ eslintSuppressionRules: [] }],
    },
    {
      name: 'Unrelated eslint-disable rule does not trigger suppression',
      code: normalizeIndent`
        function Component() {
          // eslint-disable-next-line no-unused-vars
          const x = 1;
          return <div>{x}</div>;
        }
      `,
      options: suppressionOpts,
    },
  ],
  invalid: [
    {
      name: 'eslint-disable-next-line triggers suppression',
      code: normalizeIndent`
        function Component() {
          // eslint-disable-next-line no-console
          return <div>Hello</div>;
        }
      `,
      options: suppressionOpts,
      errors: [
        {
          message: /React Compiler has skipped optimizing this component/,
          suggestions: [
            {
              desc: 'Remove the ESLint suppression and address the React error',
              output: '\nfunction Component() {\n  \n  return <div>Hello</div>;\n}\n',
            },
          ],
        },
      ],
    },
    {
      name: 'eslint-disable/enable block wrapping function triggers suppression',
      code: normalizeIndent`
        /* eslint-disable no-console */
        function Component() {
          return <div>Hello</div>;
        }
        /* eslint-enable no-console */
      `,
      options: suppressionOpts,
      errors: [
        {
          message: /React Compiler has skipped optimizing this component/,
          suggestions: [
            {
              desc: 'Remove the ESLint suppression and address the React error',
              output: '\n\nfunction Component() {\n  return <div>Hello</div>;\n}\n/* eslint-enable no-console */\n',
            },
          ],
        },
      ],
    },
    {
      name: 'eslint-disable without enable affects rest of file',
      code: normalizeIndent`
        /* eslint-disable no-console */
        function Component() {
          return <div>Hello</div>;
        }
      `,
      options: suppressionOpts,
      errors: [
        {
          message: /React Compiler has skipped optimizing this component/,
          suggestions: [
            {
              desc: 'Remove the ESLint suppression and address the React error',
              output: '\n\nfunction Component() {\n  return <div>Hello</div>;\n}\n',
            },
          ],
        },
      ],
    },
  ],
});
