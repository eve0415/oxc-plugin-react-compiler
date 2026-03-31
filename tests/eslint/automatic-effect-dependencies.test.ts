import { rules } from '../../napi/src/eslint.js';
import { normalizeIndent, testRule } from './shared-utils.js';

testRule('automatic-effect-dependencies', rules['automatic-effect-dependencies'], {
  valid: [
    {
      name: 'No AUTODEPS placeholder means no automatic effect dependency error',
      code: normalizeIndent`
        import useMyEffect from 'useMyEffect';
        function Component({a}) {
          useMyEffect(() => console.log(a.b), [a]);
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
    },
  ],
  invalid: [
    {
      name: 'Untransformed AUTODEPS call reports a pipeline error',
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
