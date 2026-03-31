import { execFile } from 'node:child_process';
import { mkdtemp, mkdir, readFile, readdir, rm, symlink, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { promisify } from 'node:util';

import { describe, expect, it } from 'vite-plus/test';

const execFileAsync = promisify(execFile);
const TEST_DIR = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = resolve(TEST_DIR, '..');
const PNPM_DIR = resolve(REPO_ROOT, 'node_modules/.pnpm');
const SYMLINK_TYPE = process.platform === 'win32' ? 'junction' : 'dir';

type CommandResult = {
  code: number | string;
  stdout: string;
  stderr: string;
};

const findOxlintPackageDir = async (): Promise<string> => {
  const entry = (await readdir(PNPM_DIR)).sort().find(name => name.startsWith('oxlint@'));
  if (entry == null) {
    throw new Error(`Could not find Oxlint package in ${PNPM_DIR}`);
  }
  return resolve(PNPM_DIR, entry, 'node_modules/oxlint');
};

const runCommand = async (cwd: string, file: string, args: string[]): Promise<CommandResult> =>
  execFileAsync(file, args, { cwd }).then(
    ({ stdout, stderr }) => ({ code: 0, stdout, stderr }),
    (error: Error & { code?: number | string; stdout?: string; stderr?: string }) => ({
      code: error.code ?? 1,
      stdout: error.stdout ?? '',
      stderr: error.stderr ?? '',
    }),
  );

const createOxlintWorkspace = async (): Promise<{ workspaceDir: string; cleanup: () => Promise<void> }> => {
  const workspaceDir = await mkdtemp(join(tmpdir(), 'oxc-react-compiler-oxlint-'));
  const nodeModulesDir = join(workspaceDir, 'node_modules');

  await mkdir(nodeModulesDir, { recursive: true });
  await symlink(resolve(REPO_ROOT, 'napi'), join(nodeModulesDir, 'oxc-plugin-react-compiler'), SYMLINK_TYPE);
  await symlink(await findOxlintPackageDir(), join(nodeModulesDir, 'oxlint'), SYMLINK_TYPE);

  return {
    workspaceDir,
    cleanup: () => rm(workspaceDir, { recursive: true, force: true }),
  };
};

describe('oxlint js plugin integration', () => {
  it('resolves the public eslint subpath from a workspace install', async () => {
    const { workspaceDir, cleanup } = await createOxlintWorkspace();

    try {
      const scriptPath = join(workspaceDir, 'resolve-eslint-subpath.mjs');
      const outputPath = join(workspaceDir, 'eslint-subpath-result.json');
      await writeFile(
        scriptPath,
        [
          "import { writeFileSync } from 'node:fs';",
          "import reactCompiler from 'oxc-plugin-react-compiler/eslint';",
          `writeFileSync(${JSON.stringify(outputPath)}, JSON.stringify({`,
          '  name: reactCompiler.meta.name,',
          "  hasRecommended: reactCompiler.configs?.recommended != null,",
          '  ruleCount: Object.keys(reactCompiler.rules ?? {}).length,',
          '}));',
          '',
        ].join('\n'),
      );

      const result = await runCommand(workspaceDir, 'node', [scriptPath]);
      expect(result.code).toBe(0);

      const payload = JSON.parse(await readFile(outputPath, 'utf8')) as {
        name: string;
        hasRecommended: boolean;
        ruleCount: number;
      };
      expect(payload.name).toBe('oxc-react-compiler');
      expect(payload.hasRecommended).toBe(true);
      expect(payload.ruleCount).toBeGreaterThan(0);
    } finally {
      await cleanup();
    }
  });

  it('runs compiler diagnostics through oxlint using the documented subpath', async () => {
    const { workspaceDir, cleanup } = await createOxlintWorkspace();

    try {
      const configPath = join(workspaceDir, 'oxlint.json');
      const sourcePath = join(workspaceDir, 'Component.jsx');
      const oxlintBin = join(workspaceDir, 'node_modules/oxlint/bin/oxlint');

      await writeFile(
        configPath,
        `${JSON.stringify(
          {
            jsPlugins: ['oxc-plugin-react-compiler/eslint'],
            rules: {
              'no-unused-vars': 'off',
              'oxc-react-compiler/capitalized-calls': ['error', { environment: { validateNoCapitalizedCalls: [] } }],
            },
          },
          null,
          2,
        )}\n`,
      );
      await writeFile(
        sourcePath,
        [
          'function Component() {',
          '  const x = Foo();',
          '  return <div>{x}</div>;',
          '}',
          '',
        ].join('\n'),
      );

      const result = await runCommand(workspaceDir, 'node', [oxlintBin, '-c', configPath, sourcePath]);
      expect(result.code).not.toBe(0);

      const output = `${result.stdout}${result.stderr}`;
      expect(output).toContain('oxc-react-compiler(capitalized-calls)');
      expect(output).toContain('Capitalized functions are reserved for components');
    } finally {
      await cleanup();
    }
  });
});
