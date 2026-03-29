/**
 * Generate .expect.md files for custom fixtures by running the upstream
 * babel-plugin-react-compiler on each fixture input.
 *
 * Usage:
 *   node scripts/generate-babel-expectations.ts [--filter pattern]
 *
 * This reads each .jsx/.tsx file in tests/fixtures/compiler/,
 * parses pragmas from the first line, runs babel-plugin-react-compiler
 * with matching options, and writes the output to .expect.md files.
 */

import type { ParserOptions } from '@babel/core';

import fs from 'node:fs';
import path from 'node:path';

import { transformSync } from '@babel/core';

const FIXTURES_DIR = path.resolve('tests/fixtures/compiler');

interface PragmaResult {
  env: Record<string, boolean>;
  pluginOpts: Record<string, string>;
  isFlow: boolean;
}

// Parse pragma comments from the first line of a fixture file.
// Returns an object with environment config options matching the upstream plugin.
const parsePragmas = (source: string): PragmaResult => {
  const firstLine = source.split('\n')[0] ?? '';
  const env: Record<string, boolean> = {};
  const pluginOpts: Record<string, string> = {};

  // compilationMode
  const modeMatch = firstLine.match(/@compilationMode\(["']?(\w+)["']?\)/);
  if (modeMatch?.[1] !== undefined) {
    const [, modeValue] = modeMatch;
    pluginOpts['compilationMode'] = modeValue;
  }

  // panicThreshold
  const panicMatch = firstLine.match(/@panicThreshold\(["']?(\w+)["']?\)/);
  if (panicMatch?.[1] !== undefined) {
    const [, panicValue] = panicMatch;
    pluginOpts['panicThreshold'] = panicValue;
  }

  // Boolean environment flags
  const boolFlags = [
    'enablePreserveExistingMemoizationGuarantees',
    'enablePreserveExistingManualUseMemo',
    'validatePreserveExistingMemoizationGuarantees',
    'enableTransitivelyFreezeFunctionExpressions',
    'enableAssumeHooksFollowRulesOfReact',
    'enableOptionalDependencies',
    'enableTreatFunctionDepsAsConditional',
    'enableTreatRefLikeIdentifiersAsRefs',
    'enableTreatSetIdentifiersAsStateSetters',
    'enableUseTypeAnnotations',
    'enableJsxOutlining',
    'enableInstructionReordering',
    'enableMemoizationComments',
    'enableNameAnonymousFunctions',
    'enableEmitInstrumentForget',
    'enableEmitHookGuards',
    'enableFire',
    'enableAllowSetStateFromRefsInEffects',
    'disableMemoizationForDebugging',
    'enableNewMutationAliasingModel',
    'enablePropagateDepsInHIR',
    'enableReactiveScopesInHIR',
    'enableChangeDetectionForDebugging',
    'validateRefAccessDuringRender',
    'validateNoSetStateInRender',
    'validateNoSetStateInEffects',
    'validateNoDerivedComputationsInEffects',
    'validateNoJsxInTryStatements',
  ];

  for (const flag of boolFlags) {
    if (firstLine.includes(`@${flag}`)) {
      env[flag] = true;
    }
  }

  // @flow detection
  const isFlow = firstLine.includes('@flow');

  return { env, pluginOpts, isFlow };
};

interface BabelResult {
  error: string | null;
  code: string | null;
  transformed: boolean;
}

const runBabel = (filepath: string, source: string): BabelResult => {
  const { env, pluginOpts, isFlow } = parsePragmas(source);

  const ext = path.extname(filepath);
  const isTS = ext === '.ts' || ext === '.tsx';

  const parserPlugins: NonNullable<ParserOptions['plugins']> = ['jsx'];
  if (isTS) parserPlugins.push('typescript');
  if (isFlow) parserPlugins.push('flow');

  const options = {
    ...pluginOpts,
    environment: {
      ...env,
    },
  };

  try {
    const result = transformSync(source, {
      filename: filepath,
      plugins: [['babel-plugin-react-compiler', options]],
      parserOpts: { plugins: parserPlugins },
      sourceType: 'module',
    });

    if (result?.code === undefined || result.code === null) {
      return { error: null, code: null, transformed: false };
    }

    return { error: null, code: result.code, transformed: true };
  } catch (error: unknown) {
    const message = error instanceof Error ? error.message : String(error);
    return { error: message, code: null, transformed: false };
  }
};

const generateExpectMd = (babelCode: string, originalSource: string): string => {
  let md = '';

  // Input section
  md += '## Input\n\n';
  md += '```javascript\n';
  md += originalSource;
  if (!originalSource.endsWith('\n')) md += '\n';
  md += '```\n\n';

  // Code section
  md += '## Code\n\n';
  md += '```javascript\n';
  md += babelCode;
  if (!babelCode.endsWith('\n')) md += '\n';
  md += '```\n';

  return md;
};

// Main
const filterArg = process.argv.find((_, i) => process.argv[i - 1] === '--filter');
const dryRun = process.argv.includes('--dry-run');

const files = fs.readdirSync(FIXTURES_DIR).filter(f => f.endsWith('.jsx') || f.endsWith('.tsx'));

let processed = 0;
let succeeded = 0;
let failed = 0;
let skipped = 0;

for (const file of files) {
  const name = file.replace(/\.(jsx|tsx)$/, '');

  if (filterArg !== undefined && !name.includes(filterArg)) {
    skipped++;
    continue;
  }

  const filepath = path.join(FIXTURES_DIR, file);
  const source = fs.readFileSync(filepath, 'utf8');

  // Skip fixtures with @skip pragma
  const firstLine = source.split('\n')[0] ?? '';
  if (firstLine.includes('@skip')) {
    console.log(`SKIP: ${name} (@skip pragma)`);
    skipped++;
    continue;
  }

  const result = runBabel(filepath, source);
  processed++;

  if (result.error !== null) {
    console.log(`ERROR: ${name} — ${result.error.split('\n')[0]}`);
    failed++;
    continue;
  }

  if (!result.transformed || result.code === null) {
    console.log(`BAIL: ${name} — compiler did not transform`);
    failed++;
    continue;
  }

  const expectMd = generateExpectMd(result.code, source);
  const expectPath = path.join(FIXTURES_DIR, `${name}.expect.md`);

  if (dryRun) {
    console.log(`WOULD WRITE: ${name} (${String(result.code.length)} chars)`);
  } else {
    fs.writeFileSync(expectPath, expectMd);
    console.log(`OK: ${name}`);
  }
  succeeded++;
}

console.log(`\nProcessed: ${String(processed)}, OK: ${String(succeeded)}, Failed: ${String(failed)}, Skipped: ${String(skipped)}`);
