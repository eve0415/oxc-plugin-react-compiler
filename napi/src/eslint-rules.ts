import type { Linter, Rule } from 'eslint';

import type { ErrorSeverity, NapiSuggestion, OxcReactCompilerOptions, RuleDefinition } from './eslint-types';
import { getLintResults } from './eslint-cache';

// ── Rule definition table (26 ErrorCategory rules) ─────────────────

const RULE_DEFINITIONS: RuleDefinition[] = [
  { category: 'Hooks', severity: 'error', name: 'hooks', description: 'Validates the rules of hooks', recommended: false },
  { category: 'CapitalizedCalls', severity: 'error', name: 'capitalized-calls', description: 'Validates against calling capitalized functions/methods instead of using JSX', recommended: false },
  { category: 'StaticComponents', severity: 'error', name: 'static-components', description: 'Validates that components are static, not recreated every render', recommended: true },
  { category: 'UseMemo', severity: 'error', name: 'use-memo', description: 'Validates usage of the useMemo() hook', recommended: true },
  { category: 'Factories', severity: 'error', name: 'component-hook-factories', description: 'Validates against higher order functions defining nested components or hooks', recommended: true },
  { category: 'PreserveManualMemo', severity: 'error', name: 'preserve-manual-memoization', description: 'Validates that existing manual memoization is preserved by the compiler', recommended: true },
  { category: 'IncompatibleLibrary', severity: 'warning', name: 'incompatible-library', description: 'Validates against usage of libraries which are incompatible with memoization', recommended: true },
  { category: 'Immutability', severity: 'error', name: 'immutability', description: 'Validates against mutating props, state, and other immutable values', recommended: true },
  { category: 'Globals', severity: 'error', name: 'globals', description: 'Validates against assignment/mutation of globals during render', recommended: true },
  { category: 'Refs', severity: 'error', name: 'refs', description: 'Validates correct usage of refs, not reading/writing during render', recommended: true },
  { category: 'EffectDependencies', severity: 'error', name: 'memoized-effect-dependencies', description: 'Validates that effect dependencies are memoized', recommended: false },
  { category: 'EffectSetState', severity: 'error', name: 'set-state-in-effect', description: 'Validates against calling setState synchronously in an effect', recommended: true },
  { category: 'EffectDerivationsOfState', severity: 'error', name: 'no-deriving-state-in-effects', description: 'Validates against deriving values from state in an effect', recommended: false },
  { category: 'ErrorBoundaries', severity: 'error', name: 'error-boundaries', description: 'Validates usage of error boundaries instead of try/catch', recommended: true },
  { category: 'Purity', severity: 'error', name: 'purity', description: 'Validates that components/hooks are pure', recommended: true },
  { category: 'RenderSetState', severity: 'error', name: 'set-state-in-render', description: 'Validates against setting state during render', recommended: true },
  { category: 'Invariant', severity: 'error', name: 'invariant', description: 'Internal invariants', recommended: false },
  { category: 'Todo', severity: 'hint', name: 'todo', description: 'Unimplemented features', recommended: false },
  { category: 'Syntax', severity: 'error', name: 'syntax', description: 'Validates against invalid syntax', recommended: false },
  { category: 'UnsupportedSyntax', severity: 'warning', name: 'unsupported-syntax', description: 'Validates against syntax not supported by React Compiler', recommended: true },
  { category: 'Config', severity: 'error', name: 'config', description: 'Validates the compiler configuration options', recommended: true },
  { category: 'Gating', severity: 'error', name: 'gating', description: 'Validates configuration of gating mode', recommended: true },
  { category: 'Suppression', severity: 'error', name: 'rule-suppression', description: 'Validates against suppression of other rules', recommended: false },
  { category: 'AutomaticEffectDependencies', severity: 'error', name: 'automatic-effect-dependencies', description: 'Verifies that automatic effect dependencies are compiled if opted-in', recommended: false },
  { category: 'Fire', severity: 'error', name: 'fire', description: 'Validates usage of fire', recommended: false },
  { category: 'FBT', severity: 'error', name: 'fbt', description: 'Validates usage of fbt', recommended: false },
];

// ── Suggestion → ESLint fixer mapping ──────────────────────────────

const makeSuggestions = (suggestions: NapiSuggestion[]): Rule.SuggestionReportDescriptor[] =>
  suggestions.map(suggestion => {
    const range: [number, number] = [suggestion.rangeStart, suggestion.rangeEnd];
    switch (suggestion.op) {
      case 'insert-before':
        return {
          desc: suggestion.description,
          fix: (fixer: Rule.RuleFixer) => fixer.insertTextBeforeRange(range, suggestion.text ?? ''),
        };
      case 'insert-after':
        return {
          desc: suggestion.description,
          fix: (fixer: Rule.RuleFixer) => fixer.insertTextAfterRange(range, suggestion.text ?? ''),
        };
      case 'replace':
        return {
          desc: suggestion.description,
          fix: (fixer: Rule.RuleFixer) => fixer.replaceTextRange(range, suggestion.text ?? ''),
        };
      case 'remove':
        return {
          desc: suggestion.description,
          fix: (fixer: Rule.RuleFixer) => fixer.removeRange(range),
        };
      default:
        return {
          desc: suggestion.description,
          fix: (fixer: Rule.RuleFixer) => fixer.replaceTextRange(range, suggestion.text ?? ''),
        };
    }
  });

// ── ESLint suppression comment parsing ────────────────────────────

const DISABLE_NEXT_LINE_RE = /eslint-disable-next-line\s+(.+)/;
const DISABLE_LINE_RE = /eslint-disable-line\s+(.+)/;

/**
 * Build a set of line numbers where specific ESLint rules are suppressed.
 * Returns a Set of line numbers where compiler diagnostics should be skipped.
 */
function buildSuppressionLines(
  sourceCode: { getAllComments: () => Array<{ value: string; loc?: { start: { line: number }; end: { line: number } } }> },
  eslintSuppressionRules: string[],
): Set<number> {
  const suppressedLines = new Set<number>();
  if (eslintSuppressionRules.length === 0) return suppressedLines;

  const ruleSet = new Set(eslintSuppressionRules);
  const comments = sourceCode.getAllComments();

  for (const comment of comments) {
    if (comment.loc == null) continue;
    const value = comment.value.trim();

    // eslint-disable-next-line rule1, rule2
    const nextLineMatch = DISABLE_NEXT_LINE_RE.exec(value);
    if (nextLineMatch != null) {
      const rules = nextLineMatch[1].split(',').map(r => r.trim());
      if (rules.some(r => ruleSet.has(r))) {
        suppressedLines.add(comment.loc.end.line + 1);
      }
      continue;
    }

    // eslint-disable-line rule1, rule2
    const lineMatch = DISABLE_LINE_RE.exec(value);
    if (lineMatch != null) {
      const rules = lineMatch[1].split(',').map(r => r.trim());
      if (rules.some(r => ruleSet.has(r))) {
        suppressedLines.add(comment.loc.start.line);
      }
    }
  }

  return suppressedLines;
}

// ── makeRule factory ───────────────────────────────────────────────

const makeRule = (ruleDef: RuleDefinition): Rule.RuleModule => ({
  meta: {
    type: 'problem',
    docs: {
      description: ruleDef.description,
      recommended: ruleDef.recommended,
    },
    fixable: 'code',
    hasSuggestions: true,
    schema: [{ type: 'object', additionalProperties: true }],
  },
  create(context: Rule.RuleContext): Rule.RuleListener {
    const sourceCode = context.sourceCode ?? context.getSourceCode();
    const filename = context.filename ?? context.getFilename();
    const userOpts = (context.options[0] ?? {}) as OxcReactCompilerOptions;

    const diagnostics = getLintResults(filename, sourceCode.text, userOpts);

    // Build suppression lines if eslintSuppressionRules is configured
    const eslintSuppressionRules = userOpts.eslintSuppressionRules ?? [];
    const suppressedLines =
      eslintSuppressionRules.length > 0
        ? buildSuppressionLines(sourceCode as Parameters<typeof buildSuppressionLines>[0], eslintSuppressionRules)
        : null;

    for (const diag of diagnostics) {
      if (diag.category !== ruleDef.category) {
        continue;
      }
      if (diag.startLine == null || diag.startColumn == null) {
        continue;
      }

      // Skip if line is suppressed by an eslint-disable comment for a configured rule
      if (suppressedLines != null && suppressedLines.has(diag.startLine)) {
        continue;
      }

      context.report({
        message: diag.message,
        loc: {
          start: { line: diag.startLine, column: diag.startColumn },
          end: { line: diag.endLine ?? diag.startLine, column: diag.endColumn ?? diag.startColumn },
        },
        suggest: makeSuggestions(diag.suggestions),
      });
    }

    return {};
  },
});

// ── no-unused-directives (special rule — auto-fix) ────────────────

const noUnusedDirectivesRule: Rule.RuleModule = {
  meta: {
    type: 'suggestion',
    docs: {
      description: 'Validates that "use no memo" directives are not unused',
      recommended: true,
    },
    fixable: 'code',
    hasSuggestions: true,
    schema: [{ type: 'object', additionalProperties: true }],
  },
  create(context: Rule.RuleContext): Rule.RuleListener {
    const sourceCode = context.sourceCode ?? context.getSourceCode();
    const filename = context.filename ?? context.getFilename();
    const userOpts = (context.options[0] ?? {}) as OxcReactCompilerOptions;

    const diagnostics = getLintResults(filename, sourceCode.text, userOpts);

    for (const diag of diagnostics) {
      if (diag.category !== 'UnusedDirective') {
        continue;
      }
      if (diag.startLine == null || diag.startColumn == null) {
        continue;
      }

      // Auto-fix: remove the unused directive
      const hasFix = diag.suggestions.length > 0 && diag.suggestions[0].op === 'remove';

      context.report({
        message: diag.message,
        loc: {
          start: { line: diag.startLine, column: diag.startColumn },
          end: { line: diag.endLine ?? diag.startLine, column: diag.endColumn ?? diag.startColumn },
        },
        fix: hasFix
          ? (fixer: Rule.RuleFixer) => fixer.removeRange([diag.suggestions[0].rangeStart, diag.suggestions[0].rangeEnd])
          : undefined,
      });
    }

    return {};
  },
};

// ── Build rule maps ────────────────────────────────────────────────

type RulesConfig = Record<string, { rule: Rule.RuleModule; severity: ErrorSeverity }>;

export const allRules: RulesConfig = {
  ...Object.fromEntries(RULE_DEFINITIONS.map(def => [def.name, { rule: makeRule(def), severity: def.severity }])),
  'no-unused-directives': { rule: noUnusedDirectivesRule, severity: 'error' },
};

export const recommendedRules: RulesConfig = Object.fromEntries(
  Object.entries(allRules).filter(([name]) => {
    if (name === 'no-unused-directives') return true;
    const def = RULE_DEFINITIONS.find(d => d.name === name);
    return def?.recommended === true;
  }),
);

// ── Severity mapping ───────────────────────────────────────────────

export const mapSeverityToESLint = (severity: ErrorSeverity): Linter.StringSeverity => {
  switch (severity) {
    case 'error':
      return 'error';
    case 'warning':
      return 'warn';
    case 'hint':
    case 'off':
      return 'off';
  }
};

export { RULE_DEFINITIONS };
