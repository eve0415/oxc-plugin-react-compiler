import { describe, expect, it } from 'vite-plus/test';

import { isFilePartOfSources, withDetectedReanimatedSupport } from '../napi/src/compiler-options.js';

describe('compiler option helpers', () => {
  describe('isFilePartOfSources', () => {
    it('defaults to skipping node_modules files', () => {
      expect(isFilePartOfSources('src/Component.tsx', undefined)).toBe(true);
      expect(isFilePartOfSources('node_modules/pkg/index.js', undefined)).toBe(false);
    });

    it('supports array-based source filters', () => {
      expect(isFilePartOfSources('src/Component.tsx', ['src/'])).toBe(true);
      expect(isFilePartOfSources('lib/Component.tsx', ['src/'])).toBe(false);
    });

    it('supports function-based source filters', () => {
      expect(isFilePartOfSources('src/Component.tsx', filename => filename.startsWith('src/'))).toBe(true);
      expect(isFilePartOfSources('lib/Component.tsx', filename => filename.startsWith('src/'))).toBe(false);
    });
  });

  describe('withDetectedReanimatedSupport', () => {
    it('injects the reanimated environment flag when enabled and detected', () => {
      const options = withDetectedReanimatedSupport(
        { enableReanimatedCheck: true, environment: { enableFire: true } },
        () => true,
      );

      expect(options.environment).toEqual({
        enableFire: true,
        enableCustomTypeDefinitionForReanimated: true,
      });
    });

    it('does not inject the flag when disabled', () => {
      const options = withDetectedReanimatedSupport(
        { enableReanimatedCheck: false, environment: {} },
        () => true,
      );

      expect(options.environment).toEqual({});
    });
  });
});
