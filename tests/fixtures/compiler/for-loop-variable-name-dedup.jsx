// @enablePreserveExistingMemoizationGuarantees
// When multiple for-loops use the same variable name (e.g. `i`), Babel
// renames them to `i_0`, `i_1`, etc. to avoid conflicts across scopes.
// OXC reuses the same name `i` in each loop, which is incorrect when
// the loops are in the same function scope after compilation.
// From: eve0415/website code-radar.tsx
function Component({ data }) {
  const result = [];

  for (let i = 0; i < 5; i++) {
    result.push(i * 10);
  }

  for (let i = 0; i < 7; i++) {
    result.push(i * 20);
  }

  for (let i = 0; i < data.length; i++) {
    result.push(data[i]);
  }

  return <div>{result.length}</div>;
}

export const FIXTURE_ENTRYPOINT = {
  fn: Component,
  params: [{ data: [1, 2, 3] }],
};
