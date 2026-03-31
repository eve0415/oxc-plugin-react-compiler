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
    {
      name: 'Directive is not reported as unused when another compiler error exists',
      code: normalizeIndent`
        function Component() {
          'use no forget';
          return cond ?? useConditionalHook();
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
      errors: [
        {
          message: "Unused 'use no forget' directive",
          suggestions: [
            {
              desc: 'Remove the directive',
              output: '\nfunction Component() {\n  \n  return <div>Hello</div>;\n}\n',
            },
          ],
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
      errors: [
        {
          message: "Unused 'use no memo' directive",
          suggestions: [
            {
              desc: 'Remove the directive',
              output: '\nfunction Component() {\n  \n  return <div>Hello</div>;\n}\n',
            },
          ],
        },
      ],
    },
    {
      name: "Unused 'use no forget' is reported for non-component functions too",
      code: normalizeIndent`
        function notacomponent() {
          'use no forget';
          return 1 + 1;
        }
      `,
      errors: [
        {
          message: "Unused 'use no forget' directive",
          suggestions: [
            {
              desc: 'Remove the directive',
              output: '\nfunction notacomponent() {\n  \n  return 1 + 1;\n}\n',
            },
          ],
        },
      ],
    },
  ],
});
