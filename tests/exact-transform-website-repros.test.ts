import type { ParserOptions } from '@babel/core';

import { readFile } from 'node:fs/promises';
import { extname, join } from 'node:path';

import { parseSync, transformSync } from '@babel/core';
import BabelPluginReactCompiler from 'babel-plugin-react-compiler';
import { describe, expect, it } from 'vite-plus/test';

import { transform as oxcTransform } from '../napi/dist/index.js';

import { compareAST } from './utils/ast-compare.js';

const fixtureDir = join(import.meta.dirname, 'fixtures/compiler');

const parserPluginsFor = (filePath: string): ParserOptions['plugins'] => {
  const ext = extname(filePath);
  if (ext === '.ts' || ext === '.tsx') return ['jsx', 'typescript'];
  return ['jsx'];
};

const transformFixture = async (filename: string) => {
  const fullPath = join(fixtureDir, filename);
  const source = await readFile(fullPath, 'utf8');
  const parserPlugins = parserPluginsFor(fullPath);

  const babelCode =
    transformSync(source, {
      filename: fullPath,
      parserOpts: { plugins: parserPlugins },
      plugins: [[BabelPluginReactCompiler, {}]],
      sourceType: 'module',
    })?.code ?? '';

  const oxcCode = oxcTransform(fullPath, source, {
    compilationMode: 'infer',
    panicThreshold: 'none',
    target: '19',
  }).code;

  const babelAst = parseSync(babelCode, {
    filename: fullPath,
    parserOpts: { plugins: parserPlugins },
    sourceType: 'module',
  });
  const oxcAst = parseSync(oxcCode, {
    filename: fullPath,
    parserOpts: { plugins: parserPlugins },
    sourceType: 'module',
  });

  return {
    babelCode,
    oxcCode,
    ast: compareAST(babelAst, oxcAst),
  };
};

const logMismatch = (label: string, babelCode: string, oxcCode: string, astDiffs: string[]) => {
  const babelLines = babelCode.split('\n');
  const oxcLines = oxcCode.split('\n');
  const max = Math.max(babelLines.length, oxcLines.length);

  let firstDiff = 0;
  for (let index = 0; index < max; index += 1) {
    if ((babelLines[index] ?? '') !== (oxcLines[index] ?? '')) {
      firstDiff = index + 1;
      break;
    }
  }

  console.log(`\n  ${label}`);
  if (firstDiff > 0) {
    console.log(`    first diff line: ${String(firstDiff)}`);
    console.log(`    babel: ${babelLines[firstDiff - 1] ?? ''}`);
    console.log(`    oxc:   ${oxcLines[firstDiff - 1] ?? ''}`);
  }
  for (const diff of astDiffs.slice(0, 5)) {
    console.log(`    ast: ${diff}`);
  }
};

describe('exact transform parity: website reductions', () => {
  it('link reduction should remain AST-identical to Babel', async () => {
    const result = await transformFixture('website-repro-link.tsx');
    logMismatch(
      'link.tsx',
      result.babelCode,
      result.oxcCode,
      result.ast.differences.map(diff => `${diff.path}: ${diff.kind} (${diff.expected ?? 'N/A'} -> ${diff.actual ?? 'N/A'})`),
    );
    expect(result.ast.match).toBe(true);
  });

  it('projects reduction should match Babel exactly', async () => {
    const result = await transformFixture('website-repro-projects.tsx');
    logMismatch('projects.tsx', result.babelCode, result.oxcCode, []);
    expect(result.babelCode).toBe(result.oxcCode);
  });

  it('skills reduction should remain AST-identical to Babel', async () => {
    const result = await transformFixture('website-repro-skills.tsx');
    logMismatch(
      'skills.tsx',
      result.babelCode,
      result.oxcCode,
      result.ast.differences.map(diff => `${diff.path}: ${diff.kind} (${diff.expected ?? 'N/A'} -> ${diff.actual ?? 'N/A'})`),
    );
    expect(result.ast.match).toBe(true);
  });

  it('sys reduction should match Babel exactly', async () => {
    const result = await transformFixture('website-repro-sys.tsx');
    logMismatch(
      'sys.tsx',
      result.babelCode,
      result.oxcCode,
      result.ast.differences.map(diff => `${diff.path}: ${diff.kind} (${diff.expected ?? 'N/A'} -> ${diff.actual ?? 'N/A'})`),
    );
    expect(result.babelCode).toBe(result.oxcCode);
  });
});
