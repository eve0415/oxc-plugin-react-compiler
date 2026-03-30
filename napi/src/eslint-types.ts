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

/** A suggestion for fixing the diagnostic. */
export interface NapiSuggestion {
  description: string;
  op: string;
  range: [number, number];
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
