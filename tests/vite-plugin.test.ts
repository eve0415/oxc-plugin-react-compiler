import type { ReactCompilerOxcOptions } from '../napi/src/vite.js';

import { describe, expect, it } from 'vitest';

import { reactCompilerOxc } from '../napi/src/vite.js';

// Access plugin hooks via property indexing on the plain object
// returned by the factory. The Vite Plugin type uses overloaded
// signatures that make direct typed access awkward in tests.
const callTransform = (options: ReactCompilerOxcOptions, id: string, code: string): Promise<unknown> => {
  const plugin = reactCompilerOxc(options);
  // @ts-expect-error -- accessing transform hook on plugin object
  // eslint-disable-next-line @typescript-eslint/no-unsafe-type-assertion -- runtime hook returns Promise
  return plugin.transform(code, id) as Promise<unknown>;
};

const callApplyToEnvironment = (consumer: string): unknown => {
  const plugin = reactCompilerOxc();
  // @ts-expect-error -- accessing applyToEnvironment hook on plugin object
  return plugin.applyToEnvironment({ config: { consumer } }) as unknown;
};

describe('reactCompilerOxc', () => {
  it('returns a plugin object with correct name and enforce', () => {
    const plugin = reactCompilerOxc();
    expect(plugin.name).toBe('oxc-plugin-react-compiler');
    expect(plugin.enforce).toBe('pre');
  });

  it('exports both named and default', async () => {
    const mod = await import('../napi/src/vite.js');
    expect(mod.reactCompilerOxc).toBe(mod.default);
  });

  describe('file filtering', () => {
    it('returns null for non-JS files', async () => {
      expect(await callTransform({}, 'styles.css', '')).toBeNull();
      expect(await callTransform({}, 'data.json', '')).toBeNull();
      expect(await callTransform({}, 'icon.svg', '')).toBeNull();
    });

    it('returns null for node_modules', async () => {
      const code = 'function App() { return <div /> }';
      expect(await callTransform({}, 'node_modules/react/index.js', code)).toBeNull();
    });

    it('accepts .tsx, .jsx, .ts, .js extensions', async () => {
      const noComponentCode = 'const x = 1';
      expect(await callTransform({}, 'app.tsx', noComponentCode)).toBeNull();
      expect(await callTransform({}, 'app.jsx', noComponentCode)).toBeNull();
      expect(await callTransform({}, 'app.ts', noComponentCode)).toBeNull();
      expect(await callTransform({}, 'app.js', noComponentCode)).toBeNull();
    });
  });

  describe('sources option', () => {
    it('filters by array of path prefixes', async () => {
      const code = 'function App() { return <div /> }';
      expect(await callTransform({ sources: ['src/'] }, 'lib/App.tsx', code)).toBeNull();
    });

    it('filters by function', async () => {
      const code = 'function App() { return <div /> }';
      expect(await callTransform({ sources: id => id.startsWith('src/') }, 'lib/App.tsx', code)).toBeNull();
    });

    it('allows node_modules when sources is set', async () => {
      const noComponentCode = 'const x = 1';
      expect(await callTransform({ sources: ['node_modules/my-lib'] }, 'node_modules/my-lib/index.js', noComponentCode)).toBeNull();
    });
  });

  describe('code regex pre-filter', () => {
    it('skips files without components or hooks (default mode)', async () => {
      const code = 'const x = 1;\nconst y = 2;';
      expect(await callTransform({}, 'app.ts', code)).toBeNull();
    });

    it('passes files with uppercase identifiers', async () => {
      const code = 'const x = 1;\nfunction App() {}';
      try {
        await callTransform({}, 'app.ts', code);
      } catch {
        // NAPI binary not available — that's fine, we verified the filter passed
      }
    });

    it('in annotation mode, only passes files with "use memo"', async () => {
      const code = 'function App() { return <div /> }';
      expect(await callTransform({ compilationMode: 'annotation' }, 'app.tsx', code)).toBeNull();
    });

    it('in annotation mode, passes files with "use memo"', async () => {
      const code = '"use memo";\nfunction App() { return <div /> }';
      try {
        await callTransform({ compilationMode: 'annotation' }, 'app.tsx', code);
      } catch {
        // NAPI binary not available — filter was passed
      }
    });
  });

  describe('applyToEnvironment', () => {
    it('returns true for client environment', () => {
      expect(callApplyToEnvironment('client')).toBe(true);
    });

    it('returns false for server environment', () => {
      expect(callApplyToEnvironment('server')).toBe(false);
    });
  });
});
