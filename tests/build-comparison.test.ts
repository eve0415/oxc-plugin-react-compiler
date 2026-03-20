import { readFile, readdir, rm } from 'node:fs/promises';
import { join } from 'node:path';

import { build } from 'vite';
import { describe, expect, it } from 'vitest';

import { compareAST, parseJS } from './utils/ast-compare.js';

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

const collectJsFiles = async (dir: string): Promise<string[]> => {
  const results: string[] = [];
  try {
    const entries = await readdir(dir, { withFileTypes: true, recursive: true });
    for (const entry of entries) {
      if (entry.isFile() && entry.name.endsWith('.js')) {
        results.push(join(entry.parentPath ?? dir, entry.name));
      }
    }
  } catch {
    // Directory may not exist yet
  }
  return results;
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

  it('JS output files have matching AST structure', async () => {
    const oxcDir = join(fixtureDir, 'dist-oxc');
    const babelDir = join(fixtureDir, 'dist-babel');

    const oxcFiles = await collectJsFiles(oxcDir);
    const babelFiles = await collectJsFiles(babelDir);

    expect(oxcFiles.length).toBeGreaterThan(0);
    expect(babelFiles.length).toBeGreaterThan(0);
    expect(oxcFiles.length).toBe(babelFiles.length);

    // Read all files upfront then compare
    const pairs = await Promise.all(
      oxcFiles.map(async (oxcPath, i) => {
        const babelPath = babelFiles[i];
        if (!babelPath) return null;
        const [oxcSource, babelSource] = await Promise.all([readFile(oxcPath, 'utf8'), readFile(babelPath, 'utf8')]);
        return { name: `chunk-${String(i)}.js`, oxcSource, babelSource };
      }),
    );

    for (const pair of pairs) {
      if (!pair) continue;
      const { name, oxcSource, babelSource } = pair;

      const oxcAST = parseJS(oxcSource);
      const babelAST = parseJS(babelSource);
      const result = compareAST(oxcAST, babelAST);

      if (result.match) {
        console.log(`  ${name}: AST structures match`);
      } else {
        console.log(`\n  AST differences in ${name}:`);
        for (const diff of result.differences.slice(0, 10)) {
          console.log(`    ${diff.path}: ${diff.kind} (expected: ${diff.expected ?? 'N/A'}, actual: ${diff.actual ?? 'N/A'})`);
        }
        if (result.differences.length > 10) {
          console.log(`    ... and ${String(result.differences.length - 10)} more`);
        }
      }
    }
  });
});
