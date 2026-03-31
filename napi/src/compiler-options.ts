import { createRequire } from 'node:module';

export type ReactCompilerCompilationMode = 'infer' | 'syntax' | 'annotation' | 'all';
export type ReactCompilerPanicThreshold = 'none' | 'all';
export type ReactCompilerSources = string[] | ((filename: string) => boolean);

type HasEnvironment = {
  environment?: Record<string, unknown>;
  enableReanimatedCheck?: boolean;
};

const require = createRequire(import.meta.url);

const hasModule = (name: string): boolean => {
  try {
    require.resolve(name);
    return true;
  } catch (error) {
    if (
      error instanceof Error &&
      'code' in error &&
      error.code === 'MODULE_NOT_FOUND' &&
      error.message.includes(name)
    ) {
      return false;
    }
    throw error;
  }
};

export const isFilePartOfSources = (
  filename: string,
  sources: ReactCompilerSources | undefined,
): boolean => {
  if (sources == null) {
    return !filename.includes('node_modules');
  }
  if (typeof sources === 'function') {
    return sources(filename);
  }
  return sources.some(prefix => filename.includes(prefix));
};

export const withDetectedReanimatedSupport = <T extends HasEnvironment>(
  options: T,
  moduleExists: (name: string) => boolean = hasModule,
): T => {
  if (options.enableReanimatedCheck === false || !moduleExists('react-native-reanimated')) {
    return options;
  }
  return {
    ...options,
    environment: {
      ...options.environment,
      enableCustomTypeDefinitionForReanimated: true,
    },
  };
};
