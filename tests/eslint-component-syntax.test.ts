import { describe, expect, it } from 'vite-plus/test';

import { lint } from '../napi/dist/index.js';

describe('eslint component syntax parity', () => {
  it('does not report diagnostics for the upstream component-syntax happy path', () => {
    const source = `
      export default component HelloWorld(
        text: string = 'Hello!',
        onClick: () => void,
      ) {
        return <div onClick={onClick}>{text}</div>;
      }
    `;

    const diagnostics = lint('test.jsx', source);
    expect(diagnostics).toEqual([]);
  });
});
