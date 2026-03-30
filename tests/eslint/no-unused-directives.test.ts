import { rules } from '../../napi/src/eslint.js';
import { normalizeIndent, testRule } from './shared-utils.js';

testRule('no-unused-directives', rules['no-unused-directives'], {
  valid: [
    {
      name: 'No directives, no errors',
      code: normalizeIndent`
        function Component() {
          return <div>Hello</div>;
        }
      `,
    },
  ],
  invalid: [
    {
      name: "Unused 'use no forget' is reported for clean component",
      code: normalizeIndent`
        function Component() {
          'use no forget';
          return <div>Hello</div>;
        }
      `,
      // The fix removes the directive expression statement; leading whitespace remains
      output: '\nfunction Component() {\n  \n  return <div>Hello</div>;\n}\n',
      errors: [
        {
          message: "Unused 'use no forget' directive",
        },
      ],
    },
    {
      name: "Unused 'use no memo' is reported for clean component",
      code: normalizeIndent`
        function Component() {
          'use no memo';
          return <div>Hello</div>;
        }
      `,
      output: '\nfunction Component() {\n  \n  return <div>Hello</div>;\n}\n',
      errors: [
        {
          message: "Unused 'use no memo' directive",
        },
      ],
    },
  ],
});
