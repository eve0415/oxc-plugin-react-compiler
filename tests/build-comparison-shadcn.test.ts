import { describe, it } from 'vitest';

// shadcn/ui smoke test — requires a separate fixture with shadcn components installed.
// Run with: RUN_SHADCN_TESTS=1 pnpm test tests/build-comparison-shadcn.test.ts
describe.skip('build comparison: shadcn/ui app', () => {
  it.todo('builds with OXC plugin');
  it.todo('builds with Babel plugin');
  it.todo('AST output matches between OXC and Babel');
});
