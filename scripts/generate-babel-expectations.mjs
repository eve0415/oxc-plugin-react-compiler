/**
 * Generate .expect.md files for custom fixtures by running the upstream
 * babel-plugin-react-compiler on each fixture input.
 *
 * Usage:
 *   node scripts/generate-babel-expectations.mjs [--filter pattern]
 *
 * This reads each .jsx/.tsx file in tests/fixtures/compiler/,
 * parses pragmas from the first line, runs babel-plugin-react-compiler
 * with matching options, and writes the output to .expect.md files.
 */

import { transformSync } from '@babel/core';
import fs from 'fs';
import path from 'path';

const FIXTURES_DIR = path.resolve('tests/fixtures/compiler');

// Parse pragma comments from the first line of a fixture file.
// Returns an object with environment config options matching the upstream plugin.
function parsePragmas(source) {
  const firstLine = source.split('\n')[0] || '';
  const env = {};
  const pluginOpts = {};

  // compilationMode
  const modeMatch = firstLine.match(/@compilationMode\(["']?(\w+)["']?\)/);
  if (modeMatch) pluginOpts.compilationMode = modeMatch[1];

  // panicThreshold
  const panicMatch = firstLine.match(/@panicThreshold\(["']?(\w+)["']?\)/);
  if (panicMatch) pluginOpts.panicThreshold = panicMatch[1];

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
}

function runBabel(filepath, source) {
  const { env, pluginOpts, isFlow } = parsePragmas(source);

  const ext = path.extname(filepath);
  const isTS = ext === '.ts' || ext === '.tsx';

  const parserPlugins = ['jsx'];
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

    if (!result || !result.code) {
      return { error: null, code: null, transformed: false };
    }

    return { error: null, code: result.code, transformed: true };
  } catch (err) {
    return { error: err.message, code: null, transformed: false };
  }
}

function generateExpectMd(babelCode, originalSource) {
  // Extract the first few comment lines as documentation
  const lines = originalSource.split('\n');
  const commentLines = [];
  for (const line of lines) {
    if (line.startsWith('//')) {
      commentLines.push(line);
    } else {
      break;
    }
  }

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
}

// Main
const filterArg = process.argv.find((a, i) => process.argv[i - 1] === '--filter');
const dryRun = process.argv.includes('--dry-run');

const files = fs.readdirSync(FIXTURES_DIR).filter(f =>
  f.endsWith('.jsx') || f.endsWith('.tsx')
);

let processed = 0;
let succeeded = 0;
let failed = 0;
let skipped = 0;

for (const file of files) {
  const name = file.replace(/\.(jsx|tsx)$/, '');

  if (filterArg && !name.includes(filterArg)) {
    skipped++;
    continue;
  }

  const filepath = path.join(FIXTURES_DIR, file);
  const source = fs.readFileSync(filepath, 'utf8');

  // Skip fixtures with @skip pragma
  const firstLine = source.split('\n')[0] || '';
  if (firstLine.includes('@skip')) {
    console.log(`SKIP: ${name} (@skip pragma)`);
    skipped++;
    continue;
  }

  const result = runBabel(filepath, source);
  processed++;

  if (result.error) {
    console.log(`ERROR: ${name} — ${result.error.split('\n')[0]}`);
    failed++;
    continue;
  }

  if (!result.transformed || !result.code) {
    console.log(`BAIL: ${name} — compiler did not transform`);
    failed++;
    continue;
  }

  const expectMd = generateExpectMd(result.code, source);
  const expectPath = path.join(FIXTURES_DIR, `${name}.expect.md`);

  if (dryRun) {
    console.log(`WOULD WRITE: ${name} (${result.code.length} chars)`);
  } else {
    fs.writeFileSync(expectPath, expectMd);
    console.log(`OK: ${name}`);
  }
  succeeded++;
}

console.log(`\nProcessed: ${processed}, OK: ${succeeded}, Failed: ${failed}, Skipped: ${skipped}`);
