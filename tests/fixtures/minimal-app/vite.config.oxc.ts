import { defineConfig } from 'vite';

import { reactCompilerOxc } from '../../../napi/src/vite.js';

export default defineConfig({
  root: import.meta.dirname,
  plugins: [reactCompilerOxc()],
  build: {
    outDir: 'dist-oxc',
    minify: false,
    rollupOptions: {
      external: ['react', 'react-dom', 'react-dom/client', 'react/jsx-runtime'],
    },
  },
  logLevel: 'silent',
});
