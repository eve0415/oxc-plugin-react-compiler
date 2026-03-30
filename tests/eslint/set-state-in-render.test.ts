import { rules } from '../../napi/src/eslint.js';
import { normalizeIndent, testRule } from './shared-utils.js';

testRule('set-state-in-render', rules['set-state-in-render'], {
  valid: [
    {
      name: 'setState in callback is ok',
      code: normalizeIndent`
        function Component() {
          const [x, setX] = useState(0);
          const handleClick = () => setX(1);
          return <button onClick={handleClick}>{x}</button>;
        }
      `,
    },
  ],
  invalid: [
    {
      name: 'Unconditional setState in render is invalid',
      code: normalizeIndent`
        function Component() {
          const [x, setX] = useState(0);
          setX(1);
          return <div>{x}</div>;
        }
      `,
      errors: [
        {
          message: /setState/i,
        },
      ],
    },
  ],
});
