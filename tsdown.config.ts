// eslint-disable-next-line typescript/ban-ts-comment -- tsdown types have overload issues with dts options
// @ts-nocheck
import { defineConfig } from 'tsdown';

export default defineConfig({
  entry: ['napi/src/vite.ts'],
  dts: { isolatedDeclarations: true },
  format: 'esm',
  outDir: 'napi/dist',
  inputOptions: {
    external: [/index\.js/, 'vite'],
  },
});
