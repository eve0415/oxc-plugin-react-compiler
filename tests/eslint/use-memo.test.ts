import { rules } from '../../napi/src/eslint.js';
import { makeTestCaseError, normalizeIndent, testRule } from './shared-utils.js';

testRule('use-memo', rules['use-memo'], {
  valid: [
    {
      name: 'useMemo callback returning a value is valid',
      code: normalizeIndent`
        import {useMemo} from 'react';
        function Component({item}) {
          const value = useMemo(() => item.id, [item]);
          return <div>{value}</div>;
        }
      `,
    },
  ],
  invalid: [
    {
      name: 'useMemo callback without a return is reported',
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
