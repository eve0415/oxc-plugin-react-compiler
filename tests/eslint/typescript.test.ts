import { rules } from '../../napi/src/eslint.js';

import { makeTestCaseError, normalizeIndent, testRuleTs } from './shared-utils.js';

testRuleTs('typescript-set-state-in-render', rules['set-state-in-render'], {
  valid: [
    {
      name: 'TypeScript generic component with valid hook usage',
      filename: 'test.tsx',
      code: normalizeIndent`
        function Component<T extends { name: string }>(props: T) {
          const [x, setX] = useState<string>('hello');
          const handleClick = () => setX('world');
          return <div onClick={handleClick}>{x}</div>;
        }
      `,
    },
    {
      name: 'TypeScript typed props',
      filename: 'test.tsx',
      code: normalizeIndent`
        function Component(props: { items: string[] }) {
          const [count, setCount] = useState<number>(0);
          return <button onClick={() => setCount(count + 1)}>{count}</button>;
        }
      `,
    },
    {
      name: 'Hooks used as normal typed values are allowed',
      filename: 'test.tsx',
      code: normalizeIndent`
        function Button(props) {
          const scrollview = React.useRef<ScrollView>(null);
          return <Button thing={scrollview} />;
        }
      `,
    },
  ],
  invalid: [
    {
      name: 'TypeScript generic component with setState in render',
      filename: 'test.tsx',
      code: normalizeIndent`
        function Component<T extends { name: string }>(props: T) {
          const [x, setX] = useState<string>('hello');
          setX('world');
          return <div>{x}</div>;
        }
      `,
      errors: [{ message: /setState/i }],
    },
    {
      name: 'TypeScript typed setState in render',
      filename: 'test.tsx',
      code: normalizeIndent`
        function Component(props: { items: string[] }) {
          const [count, setCount] = useState(0);
          setCount(count + 1);
          return <div>{count}</div>;
        }
      `,
      errors: [{ message: /setState/i }],
    },
  ],
});

testRuleTs('typescript-immutability', rules.immutability, {
  valid: [],
  invalid: [
    {
      name: 'Mutating useState value with TypeScript syntax',
      filename: 'test.tsx',
      code: normalizeIndent`
        import { useState } from 'react';
        function Component(props) {
          const x: \`foo\${1}\` = 'foo1';
          const [state, setState] = useState({a: 0});
          state.a = 1;
          return <div>{props.foo}{x}</div>;
        }
      `,
      errors: [makeTestCaseError("Modifying a value returned from 'useState()'")],
    },
  ],
});
