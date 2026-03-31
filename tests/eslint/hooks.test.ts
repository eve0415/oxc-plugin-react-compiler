import { rules } from '../../napi/src/eslint.js';

import { makeTestCaseError, normalizeIndent, testRule } from './shared-utils.js';

testRule('hooks', rules['hooks'], {
  valid: [
    {
      name: 'Basic valid hook usage',
      code: normalizeIndent`
        function Component() {
          const [x, setX] = useState(0);
          useEffect(() => {}, []);
          return <div>{x}</div>;
        }
      `,
    },
    {
      name: 'Hook in top-level of component',
      code: normalizeIndent`
        function Component(props) {
          const ref = useRef(null);
          return <div ref={ref}>{props.text}</div>;
        }
      `,
    },
    {
      name: 'Flow suppression deduplicates hooks error',
      code: normalizeIndent`
        function Component(props) {
          if (props.cond) {
            // $FlowFixMe[react-rule-hook]
            useState(0);
          }
          return <div />;
        }
      `,
      options: [{ flowSuppressions: true }],
    },
    {
      name: '[Invariant] defined after use does not crash',
      code: normalizeIndent`
        function Component(props) {
          let y = function () {
            m(x);
          };

          let x = { a };
          m(x);
          return y;
        }
      `,
    },
    {
      name: "Classes don't throw",
      code: normalizeIndent`
        class Foo {
          #bar() {}
        }
      `,
    },
  ],
  invalid: [
    {
      name: 'Conditional hook call',
      code: normalizeIndent`
        function Component(props) {
          if (props.cond) {
            useState(0);
          }
          return <div />;
        }
      `,
      errors: [makeTestCaseError('Hooks must always be called in a consistent order')],
    },
    {
      name: 'Simple hook-function violation',
      code: normalizeIndent`
        function useConditional() {
          if (cond) {
            useConditionalHook();
          }
        }
      `,
      errors: [makeTestCaseError('Hooks must always be called in a consistent order')],
    },
    {
      name: 'Flow suppression with wrong code does not suppress',
      code: normalizeIndent`
        function Component(props) {
          if (props.cond) {
            // $FlowFixMe[react-rule-other]
            useState(0);
          }
          return <div />;
        }
      `,
      options: [{ flowSuppressions: true }],
      errors: [makeTestCaseError('Hooks must always be called in a consistent order')],
    },
    {
      name: 'Multiple conditional hook violations',
      code: normalizeIndent`
        function Component(props) {
          if (props.a) {
            useState(0);
          }
          if (props.b) {
            useEffect(() => {});
          }
          return <div />;
        }
      `,
      errors: [makeTestCaseError('Hooks must always be called in a consistent order'), makeTestCaseError('Hooks must always be called in a consistent order')],
    },
  ],
});
