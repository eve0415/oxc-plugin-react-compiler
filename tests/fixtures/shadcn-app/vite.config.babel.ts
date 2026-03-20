import { defineConfig } from 'vite'
import babel from '@rolldown/plugin-babel'

export default defineConfig({
  root: import.meta.dirname,
  plugins: [
    babel({
      include: ['**/*.tsx', '**/*.ts', '**/*.jsx', '**/*.js'],
      plugins: [['babel-plugin-react-compiler', {}]],
    }),
  ],
  build: {
    outDir: 'dist-babel',
    minify: false,
    rollupOptions: {
      external: ['react', 'react-dom', 'react-dom/client', 'react/jsx-runtime'],
    },
  },
  logLevel: 'silent',
})
