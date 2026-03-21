import { createRequire } from 'node:module';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = dirname(fileURLToPath(import.meta.url));
const require = createRequire(import.meta.url);

const { platform } = process;
const { arch } = process;

const triples = {
  'linux-x64': 'linux-x64-gnu',
  'linux-arm64': 'linux-arm64-gnu',
  'darwin-x64': 'darwin-x64',
  'darwin-arm64': 'darwin-arm64',
  'win32-x64': 'win32-x64-msvc',
  'win32-arm64': 'win32-arm64-msvc',
};

const triple = triples[`${platform}-${arch}`];
if (!triple) {
  throw new Error(`Unsupported platform: ${platform}-${arch}`);
}

const bindingPath = join(__dirname, `oxc_react_compiler_napi.${triple}.node`);
const nativeBinding = require(bindingPath);

export const { transform } = nativeBinding;
