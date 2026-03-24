import { rm } from 'node:fs/promises';
import { join } from 'node:path';

import { build } from 'vite-plus';
import { describe, expect, it } from 'vite-plus/test';

import { collectJsFiles, compareExactJsOutputs, logExactMismatchSummary } from './utils/build-compare.js';

const fixtureDir = join(import.meta.dirname, 'fixtures/minimal-app');

interface BuildResult {
  duration: number;
  chunkCount: number;
}

const buildWithConfig = async (configFile: string): Promise<BuildResult> => {
  const start = performance.now();
  await build({ configFile: join(fixtureDir, configFile) });
  const duration = performance.now() - start;
  // Count output JS files on disk instead of inspecting the opaque return type
  const files = await collectJsFiles(join(fixtureDir, configFile.includes('oxc') ? 'dist-oxc' : 'dist-babel'));
  return { duration, chunkCount: files.length };
};

describe('build comparison: OXC vs Babel', { timeout: 60_000 }, () => {
  it('both configs produce a successful build', async () => {
    await rm(join(fixtureDir, 'dist-oxc'), { recursive: true, force: true });
    await rm(join(fixtureDir, 'dist-babel'), { recursive: true, force: true });

    const [oxc, babel] = await Promise.all([buildWithConfig('vite.config.oxc.ts'), buildWithConfig('vite.config.babel.ts')]);

    console.log(
      `\n  Build timings:\n` +
        `    OXC:   ${oxc.duration.toFixed(0)}ms\n` +
        `    Babel: ${babel.duration.toFixed(0)}ms\n` +
        `    Speedup: ${(babel.duration / oxc.duration).toFixed(1)}x\n`,
    );

    expect(oxc.chunkCount).toBeGreaterThan(0);
    expect(babel.chunkCount).toBeGreaterThan(0);
  });

  it('JS output files are byte-identical after pairing by logical chunk name', async () => {
    const oxcDir = join(fixtureDir, 'dist-oxc');
    const babelDir = join(fixtureDir, 'dist-babel');

    const [oxcFiles, babelFiles] = await Promise.all([collectJsFiles(oxcDir), collectJsFiles(babelDir)]);

    expect(oxcFiles.length).toBeGreaterThan(0);
    expect(babelFiles.length).toBeGreaterThan(0);
    const mismatches = await compareExactJsOutputs(babelDir, oxcDir);
    logExactMismatchSummary('minimal-app', mismatches);
    expect(mismatches).toHaveLength(0);
  });
});
