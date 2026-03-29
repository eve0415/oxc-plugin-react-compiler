import { readFileSync, readdirSync, writeFileSync } from 'node:fs';

const [, , type] = process.argv;
if (type !== 'patch' && type !== 'minor' && type !== 'major') {
  console.error('Usage: vp run bump <patch|minor|major>');
  process.exit(1);
}

interface PackageJson {
  version: string;
  [key: string]: unknown;
}

const readPackageJson = (filePath: string): PackageJson => {
  const raw: unknown = JSON.parse(readFileSync(filePath, 'utf8'));
  if (typeof raw !== 'object' || raw === null || !('version' in raw) || typeof (raw as Record<string, unknown>)['version'] !== 'string') {
    throw new Error(`Invalid package.json at ${filePath}`);
  }
  return raw as PackageJson; // eslint-disable-line @typescript-eslint/no-unsafe-type-assertion -- validated above
};

const napiPkg = readPackageJson('napi/package.json');
const prev = napiPkg.version;
const parts = prev.split('.').map(Number);
const major = parts[0] ?? 0;
const minor = parts[1] ?? 0;
const patch = parts[2] ?? 0;

const next =
  type === 'major'
    ? `${String(major + 1)}.0.0`
    : type === 'minor'
      ? `${String(major)}.${String(minor + 1)}.0`
      : `${String(major)}.${String(minor)}.${String(patch + 1)}`;

// 1. Cargo.toml workspace version
const cargo = readFileSync('Cargo.toml', 'utf8');
writeFileSync('Cargo.toml', cargo.replace(/^(version\s*=\s*").+(")/m, `$1${next}$2`));

// 2. napi/package.json — version
napiPkg.version = next;
writeFileSync('napi/package.json', `${JSON.stringify(napiPkg, null, 2)}\n`);

// 3. napi/npm/*/package.json
for (const dir of readdirSync('napi/npm')) {
  const p = `napi/npm/${dir}/package.json`;
  const pkg = readPackageJson(p);
  pkg.version = next;
  writeFileSync(p, `${JSON.stringify(pkg, null, 2)}\n`);
}

console.log(`${prev} → ${next}`);
