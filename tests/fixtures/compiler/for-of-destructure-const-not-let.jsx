// @enablePreserveExistingMemoizationGuarantees
// When two for-of loops use the same destructured binding name (e.g. [i, item]),
// Babel preserves `const` and renames the second loop's variable to avoid conflicts.
// OXC may emit `let` instead of `const` for the destructuring pattern.
// From: eve0415/website skills-visualization.tsx
function Component({ staticItems, dynamicItems }) {
  const result = [];

  for (const [i, item] of staticItems.entries()) {
    result.push({ index: i, label: item.name, type: 'static' });
  }

  for (const [i, item] of dynamicItems.entries()) {
    result.push({ index: i, label: item.name, type: 'dynamic' });
  }

  return <div>{result.length}</div>;
}

export const FIXTURE_ENTRYPOINT = {
  fn: Component,
  params: [{ staticItems: [{ name: 'a' }], dynamicItems: [{ name: 'b' }] }],
};
