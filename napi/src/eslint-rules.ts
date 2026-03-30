import type { Linter, Rule } from 'eslint';

import type { ErrorSeverity, NapiSuggestion, OxcReactCompilerOptions, RuleDefinition } from './eslint-types';
import { getLintResults } from './eslint-cache';

// ── Rule definition table (26 ErrorCategory rules) ─────────────────

const RULE_DEFINITIONS: RuleDefinition[] = [
  { category: 'Hooks', severity: 'error', name: 'hooks', description: 'Validates the rules of hooks', recommended: false },
  { category: 'CapitalizedCalls', severity: 'error', name: 'capitalized-calls', description: 'Validates against calling capitalized functions/methods instead of using JSX', recommended: false },
  { category: 'StaticComponents', severity: 'error', name: 'static-components', description: 'Validates that components are static, not recreated every render. Components that are recreated dynamically can reset state and trigger excessive re-rendering', recommended: true },
  { category: 'UseMemo', severity: 'error', name: 'use-memo', description: 'Validates usage of the useMemo() hook against common mistakes. See [`useMemo()` docs](https://react.dev/reference/react/useMemo) for more information.', recommended: true },
  { category: 'Factories', severity: 'error', name: 'component-hook-factories', description: 'Validates against higher order functions defining nested components or hooks. Components and hooks should be defined at the module level', recommended: true },
  { category: 'PreserveManualMemo', severity: 'error', name: 'preserve-manual-memoization', description: 'Validates that existing manual memoized is preserved by the compiler. React Compiler will only compile components and hooks if its inference [matches or exceeds the existing manual memoization](https://react.dev/learn/react-compiler/introduction#what-should-i-do-about-usememo-usecallback-and-reactmemo)', recommended: true },
  { category: 'IncompatibleLibrary', severity: 'warning', name: 'incompatible-library', description: 'Validates against usage of libraries which are incompatible with memoization (manual or automatic)', recommended: true },
  { category: 'Immutability', severity: 'error', name: 'immutability', description: 'Validates against mutating props, state, and other values that [are immutable](https://react.dev/reference/rules/components-and-hooks-must-be-pure#props-and-state-are-immutable)', recommended: true },
  { category: 'Globals', severity: 'error', name: 'globals', description: 'Validates against assignment/mutation of globals during render, part of ensuring that [side effects must render outside of render](https://react.dev/reference/rules/components-and-hooks-must-be-pure#side-effects-must-run-outside-of-render)', recommended: true },
  { category: 'Refs', severity: 'error', name: 'refs', description: 'Validates correct usage of refs, not reading/writing during render. See the "pitfalls" section in [`useRef()` usage](https://react.dev/reference/react/useRef#usage)', recommended: true },
  { category: 'EffectDependencies', severity: 'error', name: 'memoized-effect-dependencies', description: 'Validates that effect dependencies are memoized', recommended: false },
  { category: 'EffectSetState', severity: 'error', name: 'set-state-in-effect', description: 'Validates against calling setState synchronously in an effect, which can lead to re-renders that degrade performance', recommended: true },
  { category: 'EffectDerivationsOfState', severity: 'error', name: 'no-deriving-state-in-effects', description: 'Validates against deriving values from state in an effect', recommended: false },
  { category: 'ErrorBoundaries', severity: 'error', name: 'error-boundaries', description: 'Validates usage of error boundaries instead of try/catch for errors in child components', recommended: true },
  { category: 'Purity', severity: 'error', name: 'purity', description: 'Validates that [components/hooks are pure](https://react.dev/reference/rules/components-and-hooks-must-be-pure) by checking that they do not call known-impure functions', recommended: true },
  { category: 'RenderSetState', severity: 'error', name: 'set-state-in-render', description: 'Validates against setting state during render, which can trigger additional renders and potential infinite render loops', recommended: true },
  { category: 'Invariant', severity: 'error', name: 'invariant', description: 'Internal invariants', recommended: false },
  { category: 'Todo', severity: 'hint', name: 'todo', description: 'Unimplemented features', recommended: false },
  { category: 'Syntax', severity: 'error', name: 'syntax', description: 'Validates against invalid syntax', recommended: false },
  { category: 'UnsupportedSyntax', severity: 'warning', name: 'unsupported-syntax', description: 'Validates against syntax that we do not plan to support in React Compiler', recommended: true },
  { category: 'Config', severity: 'error', name: 'config', description: 'Validates the compiler configuration options', recommended: true },
  { category: 'Gating', severity: 'error', name: 'gating', description: 'Validates configuration of [gating mode](https://react.dev/reference/react-compiler/gating)', recommended: true },
  { category: 'Suppression', severity: 'error', name: 'rule-suppression', description: 'Validates against suppression of other rules', recommended: false },
  { category: 'AutomaticEffectDependencies', severity: 'error', name: 'automatic-effect-dependencies', description: 'Verifies that automatic effect dependencies are compiled if opted-in', recommended: false },
  { category: 'Fire', severity: 'error', name: 'fire', description: 'Validates usage of `fire`', recommended: false },
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

    for (const diag of diagnostics) {
      if (diag.category !== ruleDef.category) {
        continue;
      }
      if (diag.startLine == null || diag.startColumn == null) {
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

      const hasFix = diag.suggestions.length > 0 && diag.suggestions[0].op === 'remove';

      context.report({
        message: diag.message,
        loc: {
          start: { line: diag.startLine, column: diag.startColumn },
          end: { line: diag.endLine ?? diag.startLine, column: diag.endColumn ?? diag.startColumn },
        },
        suggest: hasFix
          ? [
              {
                desc: 'Remove the directive',
                fix: (fixer: Rule.RuleFixer) =>
                  fixer.removeRange([diag.suggestions[0].rangeStart, diag.suggestions[0].rangeEnd]),
              },
            ]
          : [],
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
