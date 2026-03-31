//! Compiler error types.
//!
//! Port of `CompilerError.ts` from upstream.

use std::fmt;

use crate::hir::types::SourceLocation;

/// Severity levels for compiler diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticSeverity {
    /// Fatal error — bail out of compilation for this function.
    InvalidReact,
    /// Input is valid but cannot be compiled (e.g., unsupported pattern).
    CannotPreserveMemoization,
    /// Todo — feature not yet implemented.
    Todo,
    /// Invariant violation (bug in the compiler).
    Invariant,
}

/// Error category for lint rules, matching upstream's `ErrorCategory` enum.
/// Each variant maps to a distinct ESLint rule name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorCategory {
    Hooks,
    CapitalizedCalls,
    StaticComponents,
    UseMemo,
    Factories,
    PreserveManualMemo,
    IncompatibleLibrary,
    Immutability,
    Globals,
    Refs,
    EffectDependencies,
    EffectSetState,
    EffectDerivationsOfState,
    ErrorBoundaries,
    Purity,
    RenderSetState,
    Invariant,
    Todo,
    Syntax,
    UnsupportedSyntax,
    Config,
    Gating,
    Suppression,
    AutomaticEffectDependencies,
    Fire,
    FBT,
    /// Used by the no-unused-directives rule (not an upstream ErrorCategory).
    UnusedDirective,
}

impl fmt::Display for ErrorCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.rule_name())
    }
}

impl ErrorCategory {
    /// Returns the kebab-case rule name matching upstream's `LintRule.name`.
    pub fn rule_name(self) -> &'static str {
        match self {
            Self::Hooks => "hooks",
            Self::CapitalizedCalls => "capitalized-calls",
            Self::StaticComponents => "static-components",
            Self::UseMemo => "use-memo",
            Self::Factories => "component-hook-factories",
            Self::PreserveManualMemo => "preserve-manual-memoization",
            Self::IncompatibleLibrary => "incompatible-library",
            Self::Immutability => "immutability",
            Self::Globals => "globals",
            Self::Refs => "refs",
            Self::EffectDependencies => "memoized-effect-dependencies",
            Self::EffectSetState => "set-state-in-effect",
            Self::EffectDerivationsOfState => "no-deriving-state-in-effects",
            Self::ErrorBoundaries => "error-boundaries",
            Self::Purity => "purity",
            Self::RenderSetState => "set-state-in-render",
            Self::Invariant => "invariant",
            Self::Todo => "todo",
            Self::Syntax => "syntax",
            Self::UnsupportedSyntax => "unsupported-syntax",
            Self::Config => "config",
            Self::Gating => "gating",
            Self::Suppression => "rule-suppression",
            Self::AutomaticEffectDependencies => "automatic-effect-dependencies",
            Self::Fire => "fire",
            Self::FBT => "fbt",
            Self::UnusedDirective => "no-unused-directives",
        }
    }

    /// Returns the default ESLint severity for this category.
    pub fn default_severity(self) -> ErrorSeverity {
        match self {
            Self::IncompatibleLibrary | Self::UnsupportedSyntax => ErrorSeverity::Warning,
            Self::Todo => ErrorSeverity::Hint,
            _ => ErrorSeverity::Error,
        }
    }

    /// Whether this rule is included in the "recommended" preset.
    pub fn recommended(self) -> bool {
        matches!(
            self,
            Self::StaticComponents
                | Self::UseMemo
                | Self::Factories
                | Self::PreserveManualMemo
                | Self::IncompatibleLibrary
                | Self::Immutability
                | Self::Globals
                | Self::Refs
                | Self::EffectSetState
                | Self::ErrorBoundaries
                | Self::Purity
                | Self::RenderSetState
                | Self::UnsupportedSyntax
                | Self::Config
                | Self::Gating
                | Self::UnusedDirective
        )
    }
}

/// ESLint-level severity for lint rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorSeverity {
    Error,
    Warning,
    Hint,
    Off,
}

impl fmt::Display for ErrorSeverity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Error => f.write_str("error"),
            Self::Warning => f.write_str("warning"),
            Self::Hint => f.write_str("hint"),
            Self::Off => f.write_str("off"),
        }
    }
}

/// A related diagnostic location with context message.
#[derive(Debug, Clone)]
pub struct RelatedDiagnostic {
    pub message: String,
    pub span: Option<oxc_span::Span>,
}

/// An auto-fix suggestion for a diagnostic.
#[derive(Debug, Clone)]
pub enum CompilerSuggestion {
    InsertBefore {
        description: String,
        range: (u32, u32),
        text: String,
    },
    InsertAfter {
        description: String,
        range: (u32, u32),
        text: String,
    },
    Remove {
        description: String,
        range: (u32, u32),
    },
    Replace {
        description: String,
        range: (u32, u32),
        text: String,
    },
}

/// A compiler diagnostic.
#[derive(Debug, Clone)]
pub struct CompilerDiagnostic {
    pub severity: DiagnosticSeverity,
    pub message: String,
    pub category: ErrorCategory,
    pub span: Option<oxc_span::Span>,
    pub related: Vec<RelatedDiagnostic>,
    pub suggestions: Vec<CompilerSuggestion>,
}

impl Default for CompilerDiagnostic {
    fn default() -> Self {
        Self {
            severity: DiagnosticSeverity::Invariant,
            message: String::new(),
            category: ErrorCategory::Invariant,
            span: None,
            related: Vec::new(),
            suggestions: Vec::new(),
        }
    }
}

impl fmt::Display for CompilerDiagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{:?}] {}", self.severity, self.message)
    }
}

/// Error type for bail-out during compilation.
#[derive(Debug, Clone)]
pub struct BailOut {
    pub reason: String,
    pub diagnostics: Vec<CompilerDiagnostic>,
}

impl fmt::Display for BailOut {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BailOut: {}", self.reason)?;
        for diag in &self.diagnostics {
            write!(f, "\n  {diag}")?;
        }
        Ok(())
    }
}

impl std::error::Error for BailOut {}

/// High-level error type for the compilation pipeline.
#[derive(Debug, Clone)]
pub enum CompilerError {
    /// Validation failure — bail to original code
    Bail(BailOut),
    /// Lowering failure
    LoweringFailed(String),
}

impl CompilerError {
    /// Create a bail error for an invariant violation.
    pub fn invariant(reason: impl Into<String>, description: Option<String>) -> Self {
        let reason = reason.into();
        let message = if let Some(ref desc) = description {
            format!("{reason}. {desc}.")
        } else {
            reason.clone()
        };
        Self::Bail(BailOut {
            reason: reason.clone(),
            diagnostics: vec![CompilerDiagnostic {
                severity: DiagnosticSeverity::Invariant,
                message,
                category: ErrorCategory::Invariant,
                ..Default::default()
            }],
        })
    }
}

impl fmt::Display for CompilerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bail(b) => write!(f, "Bail: {}", b),
            Self::LoweringFailed(msg) => write!(f, "Lowering failed: {}", msg),
        }
    }
}

impl std::error::Error for CompilerError {}

impl From<BailOut> for CompilerError {
    fn from(b: BailOut) -> Self {
        Self::Bail(b)
    }
}

/// Extract the OXC span from a `SourceLocation`, if it has one.
pub fn extract_span(loc: &SourceLocation) -> Option<oxc_span::Span> {
    match loc {
        SourceLocation::Source(range) => Some(range.original_span),
        SourceLocation::Generated => None,
    }
}

/// Extract the source range from a `SourceLocation`, if it has one.
pub fn extract_source_range(loc: &SourceLocation) -> Option<&crate::hir::types::SourceRange> {
    match loc {
        SourceLocation::Source(range) => Some(range),
        SourceLocation::Generated => None,
    }
}

/// A lint-related location with line:column info.
#[derive(Debug, Clone)]
pub struct LintRelated {
    pub message: String,
    pub start_line: u32,
    pub start_column: u32,
    pub end_line: u32,
    pub end_column: u32,
}

/// A lint-level auto-fix suggestion.
#[derive(Debug, Clone)]
pub struct LintSuggestion {
    pub description: String,
    pub op: SuggestionOp,
    pub range: (u32, u32),
    pub text: Option<String>,
}

/// The operation type for a suggestion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuggestionOp {
    InsertBefore,
    InsertAfter,
    Remove,
    Replace,
}

impl fmt::Display for SuggestionOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InsertBefore => f.write_str("insert-before"),
            Self::InsertAfter => f.write_str("insert-after"),
            Self::Remove => f.write_str("remove"),
            Self::Replace => f.write_str("replace"),
        }
    }
}

/// A structured lint diagnostic with line:column location info,
/// ready to be returned via NAPI.
#[derive(Debug, Clone)]
pub struct LintDiagnostic {
    pub category: ErrorCategory,
    pub message: String,
    pub severity: ErrorSeverity,
    pub start_line: u32,
    pub start_column: u32,
    pub end_line: u32,
    pub end_column: u32,
    pub has_location: bool,
    pub related: Vec<LintRelated>,
    pub suggestions: Vec<LintSuggestion>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_factory_methods() {
        let err = CompilerError::invariant("unexpected null", None);
        match &err {
            CompilerError::Bail(b) => assert!(!b.diagnostics.is_empty()),
            _ => panic!("expected Bail"),
        }
    }

    #[test]
    fn test_backward_compat() {
        let _diag = CompilerDiagnostic {
            severity: DiagnosticSeverity::InvalidReact,
            message: "test".to_string(),
            category: ErrorCategory::Hooks,
            ..Default::default()
        };
    }

    #[test]
    fn test_error_category_rule_names() {
        assert_eq!(ErrorCategory::Hooks.rule_name(), "hooks");
        assert_eq!(
            ErrorCategory::RenderSetState.rule_name(),
            "set-state-in-render"
        );
        assert_eq!(
            ErrorCategory::PreserveManualMemo.rule_name(),
            "preserve-manual-memoization"
        );
        assert_eq!(
            ErrorCategory::UnusedDirective.rule_name(),
            "no-unused-directives"
        );
    }

    #[test]
    fn test_error_severity_display() {
        assert_eq!(format!("{}", ErrorSeverity::Error), "error");
        assert_eq!(format!("{}", ErrorSeverity::Warning), "warning");
    }

    #[test]
    fn test_suggestion_op_display() {
        assert_eq!(format!("{}", SuggestionOp::InsertBefore), "insert-before");
        assert_eq!(format!("{}", SuggestionOp::Remove), "remove");
    }
}
