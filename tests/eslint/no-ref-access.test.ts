import { rules } from '../../napi/src/eslint.js';

import { normalizeIndent, testRule } from './shared-utils.js';

testRule('refs', rules['refs'], {
  valid: [
    {
      name: 'Ref access in effect is ok',
      code: normalizeIndent`
        function Component() {
          const ref = useRef(null);
          useEffect(() => {
            console.log(ref.current);
          });
          return <div ref={ref} />;
        }
      `,
    },
    {
      name: 'Ref without current access is ok',
      code: normalizeIndent`
        function Component() {
          const ref = useRef(null);
          return <div ref={ref} />;
        }
      `,
    },
  ],
  invalid: [
    {
      name: 'Direct ref.current access in render is invalid',
      code: normalizeIndent`
        function Component() {
          const ref = useRef(null);
          const value = ref.current;
          return <div>{value}</div>;
        }
      `,
      errors: [
        {
          message: /ref/i,
        },
      ],
    },
  ],
});
