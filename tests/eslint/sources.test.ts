import { rules } from '../../napi/src/eslint.js';
import { makeTestCaseError, normalizeIndent, testRule } from './shared-utils.js';

testRule('sources-option', rules.refs, {
  valid: [
    {
      name: 'skips files outside configured sources',
      filename: 'lib/Component.jsx',
      code: normalizeIndent`
        function Component() {
          const ref = useRef(null);
          return ref.current;
        }
      `,
      options: [{ sources: ['src/'] }],
    },
  ],
  invalid: [
    {
      name: 'lints files inside configured sources',
      filename: 'src/Component.jsx',
      code: normalizeIndent`
        function Component() {
          const ref = useRef(null);
          return ref.current;
        }
      `,
      options: [{ sources: ['src/'] }],
      errors: [makeTestCaseError('Cannot access ref value during render')],
    },
  ],
});
