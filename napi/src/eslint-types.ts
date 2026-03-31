import type { ReactCompilerCompilationMode, ReactCompilerPanicThreshold, ReactCompilerSources } from './compiler-options';

/** Location within source text, using 1-based lines and 0-based columns. */
export interface DiagnosticLocation {
  line: number;
  column: number;
  endLine: number;
  endColumn: number;
}

/** A related diagnostic (e.g., "this value was mutated here"). */
export interface NapiRelatedDiagnostic {
  message: string;
  startLine: number;
  startColumn: number;
  endLine: number;
  endColumn: number;
}

/** A suggestion for fixing the diagnostic (as returned by the NAPI binding). */
export interface NapiSuggestion {
  description: string;
  op: string;
  rangeStart: number;
  rangeEnd: number;
  text?: string;
}

/** A single lint diagnostic returned from the NAPI `lint()` function. */
export interface NapiLintDiagnostic {
  category: string;
  message: string;
  severity: string;
  startLine?: number;
  startColumn?: number;
  endLine?: number;
  endColumn?: number;
  related: NapiRelatedDiagnostic[];
  suggestions: NapiSuggestion[];
}

/** ESLint-level severity. */
export type ErrorSeverity = 'error' | 'warning' | 'hint' | 'off';

/** Rule definition matching upstream LintRule. */
export interface RuleDefinition {
  category: string;
  severity: ErrorSeverity;
  name: string;
  description: string;
  recommended: boolean;
}

/** Environment configuration for the React Compiler. */
export interface OxcReactCompilerEnvironment {
  validateHooksUsage?: boolean;
  validateRefAccessDuringRender?: boolean;
  validateNoSetStateInRender?: boolean;
  validateNoSetStateInEffects?: boolean;
  validateNoDerivedComputationsInEffects?: boolean;
  validateNoJsxInTryStatements?: boolean;
  validateStaticComponents?: boolean;
  validateMemoizedEffectDependencies?: boolean;
  validateNoCapitalizedCalls?: string[];
  validateNoImpureFunctionsInRender?: boolean;
  validateNoFreezingKnownMutableFunctions?: boolean;
  validateNoVoidUseMemo?: boolean;
  validateBlocklistedImports?: string[];
  validatePreserveExistingMemoizationGuarantees?: boolean;
  validateNoDynamicallyCreatedComponentsOrHooks?: boolean;
  enableFire?: boolean;
  enableUseTypeAnnotations?: boolean;
  enableTreatRefLikeIdentifiersAsRefs?: boolean;
  enableTreatSetIdentifiersAsStateSetters?: boolean;
  hookPattern?: string;
  inferEffectDependencies?: {
    function: { source: string; importSpecifierName: string };
    autodepsIndex: number;
  }[];
  inlineJsxTransform?: { elementSymbol: string; globalDevVar: string };
  lowerContextAccess?: { module: string; importedName: string };
  customMacros?: { name: string; props: string[] }[];
  [key: string]: unknown;
}

/**
 * Options accepted by each ESLint rule via `context.options[0]`.
 * Mirrors upstream's PluginOptions structure.
 */
export interface OxcReactCompilerOptions {
  compilationMode?: ReactCompilerCompilationMode;
  panicThreshold?: ReactCompilerPanicThreshold;
  target?: string;
  environment?: OxcReactCompilerEnvironment;
  customOptOutDirectives?: string[];
  ignoreUseNoForget?: boolean;
  eslintSuppressionRules?: string[];
  flowSuppressions?: boolean;
  gating?: { source: string; importSpecifierName: string };
  dynamicGating?: { source: string };
  sources?: ReactCompilerSources;
  enableReanimatedCheck?: boolean;
}
