import { readdirSync, readFileSync, writeFileSync } from 'node:fs';

const type = process.argv[2];
if (!type || !['patch', 'minor', 'major'].includes(type)) {
  console.error('Usage: vp run bump <patch|minor|major>');
  process.exit(1);
}

const napiPkg = JSON.parse(readFileSync('napi/package.json', 'utf8'));
const prev = napiPkg.version;
const [major, minor, patch] = prev.split('.').map(Number);

const next =
  type === 'major'
    ? `${major + 1}.0.0`
    : type === 'minor'
      ? `${major}.${minor + 1}.0`
      : `${major}.${minor}.${patch + 1}`;

// 1. Cargo.toml workspace version
const cargo = readFileSync('Cargo.toml', 'utf8');
writeFileSync(
  'Cargo.toml',
  cargo.replace(
    /^(version\s*=\s*").+(")/m,
    `$1${next}$2`,
  ),
);

// 2. napi/package.json — version + optionalDependencies
napiPkg.version = next;
for (const dep of Object.keys(napiPkg.optionalDependencies ?? {})) {
  napiPkg.optionalDependencies[dep] = next;
}
writeFileSync('napi/package.json', `${JSON.stringify(napiPkg, null, 2)}\n`);

// 3. napi/npm/*/package.json
for (const dir of readdirSync('napi/npm')) {
  const p = `napi/npm/${dir}/package.json`;
  const pkg = JSON.parse(readFileSync(p, 'utf8'));
  pkg.version = next;
  writeFileSync(p, `${JSON.stringify(pkg, null, 2)}\n`);
}

console.log(`${prev} → ${next}`);
