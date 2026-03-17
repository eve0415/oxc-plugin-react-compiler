//! Compiler error types.
//!
//! Port of `CompilerError.ts` from upstream.
//!
//! This module provides:
//! - [`ErrorCategory`] — 25 error categories matching upstream exactly
//! - [`ErrorSeverity`] — 4-level severity (Error, Warning, Hint, Off)
//! - [`LintRule`] and [`get_rule_for_category`] — ESLint rule metadata per category
//! - [`CompilerDiagnosticDetail`] — source locations and hints for rich diagnostics
//! - [`print_error_summary`] — heading + reason formatting matching upstream output
//!
//! Legacy types ([`DiagnosticSeverity`], [`CompilerDiagnostic`], [`BailOut`], [`CompilerError`])
//! are preserved for backward compatibility during migration.

use std::fmt;

// =============================================================================
// New upstream-aligned types (Phase 1)
// =============================================================================

/// Error categories matching upstream `ErrorCategory` enum exactly.
///
/// Reference: `CompilerError.ts:568-670`
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
}

/// Severity levels matching upstream `ErrorSeverity` enum.
///
/// Reference: `CompilerError.ts:15-35`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorSeverity {
    /// An actionable error that the developer can fix.
    Error,
    /// An error that the developer may not necessarily be able to fix
    /// (e.g., unsupported syntax).
    Warning,
    /// Not an error. Informational hints shown in tools like Forgive.
    Hint,
    /// These errors will not be reported anywhere. Useful for WIP validations.
    Off,
}

/// ESLint rule metadata for an [`ErrorCategory`].
///
/// Reference: `CompilerError.ts:672-701`
#[derive(Debug, Clone)]
pub struct LintRule {
    pub category: ErrorCategory,
    pub severity: ErrorSeverity,
    pub name: &'static str,
    pub description: &'static str,
    pub recommended: bool,
}

/// Returns the [`LintRule`] for a given [`ErrorCategory`], matching upstream
/// `getRuleForCategory()` exactly.
///
/// Reference: `CompilerError.ts:705-980`
pub fn get_rule_for_category(category: ErrorCategory) -> LintRule {
    match category {
        ErrorCategory::AutomaticEffectDependencies => LintRule {
            category,
            severity: ErrorSeverity::Error,
            name: "automatic-effect-dependencies",
            description: "Verifies that automatic effect dependencies are compiled if opted-in",
            recommended: false,
        },
        ErrorCategory::CapitalizedCalls => LintRule {
            category,
            severity: ErrorSeverity::Error,
            name: "capitalized-calls",
            description: "Validates against calling capitalized functions/methods instead of using JSX",
            recommended: false,
        },
        ErrorCategory::Config => LintRule {
            category,
            severity: ErrorSeverity::Error,
            name: "config",
            description: "Validates the compiler configuration options",
            recommended: true,
        },
        ErrorCategory::EffectDependencies => LintRule {
            category,
            severity: ErrorSeverity::Error,
            name: "memoized-effect-dependencies",
            description: "Validates that effect dependencies are memoized",
            recommended: false,
        },
        ErrorCategory::EffectDerivationsOfState => LintRule {
            category,
            severity: ErrorSeverity::Error,
            name: "no-deriving-state-in-effects",
            description: "Validates against deriving values from state in an effect",
            recommended: false,
        },
        ErrorCategory::EffectSetState => LintRule {
            category,
            severity: ErrorSeverity::Error,
            name: "set-state-in-effect",
            description: "Validates against calling setState synchronously in an effect, which can lead to re-renders that degrade performance",
            recommended: true,
        },
        ErrorCategory::ErrorBoundaries => LintRule {
            category,
            severity: ErrorSeverity::Error,
            name: "error-boundaries",
            description: "Validates usage of error boundaries instead of try/catch for errors in child components",
            recommended: true,
        },
        ErrorCategory::Factories => LintRule {
            category,
            severity: ErrorSeverity::Error,
            name: "component-hook-factories",
            description: "Validates against higher order functions defining nested components or hooks. Components and hooks should be defined at the module level",
            recommended: true,
        },
        ErrorCategory::FBT => LintRule {
            category,
            severity: ErrorSeverity::Error,
            name: "fbt",
            description: "Validates usage of fbt",
            recommended: false,
        },
        ErrorCategory::Fire => LintRule {
            category,
            severity: ErrorSeverity::Error,
            name: "fire",
            description: "Validates usage of `fire`",
            recommended: false,
        },
        ErrorCategory::Gating => LintRule {
            category,
            severity: ErrorSeverity::Error,
            name: "gating",
            description: "Validates configuration of gating mode",
            recommended: true,
        },
        ErrorCategory::Globals => LintRule {
            category,
            severity: ErrorSeverity::Error,
            name: "globals",
            description: "Validates against assignment/mutation of globals during render",
            recommended: true,
        },
        ErrorCategory::Hooks => LintRule {
            category,
            severity: ErrorSeverity::Error,
            name: "hooks",
            description: "Validates the rules of hooks",
            recommended: false,
        },
        ErrorCategory::Immutability => LintRule {
            category,
            severity: ErrorSeverity::Error,
            name: "immutability",
            description: "Validates against mutating props, state, and other values that are immutable",
            recommended: true,
        },
        ErrorCategory::IncompatibleLibrary => LintRule {
            category,
            severity: ErrorSeverity::Warning,
            name: "incompatible-library",
            description: "Validates against usage of libraries which are incompatible with memoization (manual or automatic)",
            recommended: true,
        },
        ErrorCategory::Invariant => LintRule {
            category,
            severity: ErrorSeverity::Error,
            name: "invariant",
            description: "Internal invariants",
            recommended: false,
        },
        ErrorCategory::PreserveManualMemo => LintRule {
            category,
            severity: ErrorSeverity::Error,
            name: "preserve-manual-memoization",
            description: "Validates that existing manual memoization is preserved by the compiler",
            recommended: true,
        },
        ErrorCategory::Purity => LintRule {
            category,
            severity: ErrorSeverity::Error,
            name: "purity",
            description: "Validates that components/hooks are pure by checking that they do not call known-impure functions",
            recommended: true,
        },
        ErrorCategory::Refs => LintRule {
            category,
            severity: ErrorSeverity::Error,
            name: "refs",
            description: "Validates correct usage of refs, not reading/writing during render",
            recommended: true,
        },
        ErrorCategory::RenderSetState => LintRule {
            category,
            severity: ErrorSeverity::Error,
            name: "set-state-in-render",
            description: "Validates against setting state during render, which can trigger additional renders and potential infinite render loops",
            recommended: true,
        },
        ErrorCategory::StaticComponents => LintRule {
            category,
            severity: ErrorSeverity::Error,
            name: "static-components",
            description: "Validates that components are static, not recreated every render",
            recommended: true,
        },
        ErrorCategory::Suppression => LintRule {
            category,
            severity: ErrorSeverity::Error,
            name: "rule-suppression",
            description: "Validates against suppression of other rules",
            recommended: false,
        },
        ErrorCategory::Syntax => LintRule {
            category,
            severity: ErrorSeverity::Error,
            name: "syntax",
            description: "Validates against invalid syntax",
            recommended: false,
        },
        ErrorCategory::Todo => LintRule {
            category,
            severity: ErrorSeverity::Hint,
            name: "todo",
            description: "Unimplemented features",
            recommended: false,
        },
        ErrorCategory::UnsupportedSyntax => LintRule {
            category,
            severity: ErrorSeverity::Warning,
            name: "unsupported-syntax",
            description: "Validates against syntax that we do not plan to support in React Compiler",
            recommended: true,
        },
        ErrorCategory::UseMemo => LintRule {
            category,
            severity: ErrorSeverity::Error,
            name: "use-memo",
            description: "Validates usage of the useMemo() hook against common mistakes",
            recommended: true,
        },
    }
}

/// Detail entry for a [`CompilerDiagnostic`], matching upstream exactly.
///
/// Reference: `CompilerError.ts:45-57`
#[derive(Debug, Clone)]
pub enum CompilerDiagnosticDetail {
    Error {
        loc: Option<SourceRange>,
        message: Option<String>,
    },
    Hint {
        message: String,
    },
}

/// A source range for error reporting.
#[derive(Debug, Clone, Copy)]
pub struct SourceRange {
    pub start: SourcePosition,
    pub end: SourcePosition,
}

/// A position in source code.
#[derive(Debug, Clone, Copy)]
pub struct SourcePosition {
    pub line: u32,
    pub column: u32,
}

/// Suggestion operations matching upstream `CompilerSuggestionOperation`.
///
/// Reference: `CompilerError.ts:59-79`
#[derive(Debug, Clone)]
pub enum CompilerSuggestion {
    InsertBefore {
        range: (u32, u32),
        description: String,
        text: String,
    },
    InsertAfter {
        range: (u32, u32),
        description: String,
        text: String,
    },
    Remove {
        range: (u32, u32),
        description: String,
    },
    Replace {
        range: (u32, u32),
        description: String,
        text: String,
    },
}

/// Returns the error heading for a category, matching upstream `printErrorSummary()`.
///
/// Reference: `CompilerError.ts:517-563`
pub fn error_heading(category: ErrorCategory) -> &'static str {
    match category {
        ErrorCategory::EffectDependencies
        | ErrorCategory::IncompatibleLibrary
        | ErrorCategory::PreserveManualMemo
        | ErrorCategory::UnsupportedSyntax => "Compilation Skipped",
        ErrorCategory::Invariant => "Invariant",
        ErrorCategory::Todo => "Todo",
        // All other categories produce "Error"
        ErrorCategory::AutomaticEffectDependencies
        | ErrorCategory::CapitalizedCalls
        | ErrorCategory::Config
        | ErrorCategory::EffectDerivationsOfState
        | ErrorCategory::EffectSetState
        | ErrorCategory::ErrorBoundaries
        | ErrorCategory::Factories
        | ErrorCategory::FBT
        | ErrorCategory::Fire
        | ErrorCategory::Gating
        | ErrorCategory::Globals
        | ErrorCategory::Hooks
        | ErrorCategory::Immutability
        | ErrorCategory::Purity
        | ErrorCategory::Refs
        | ErrorCategory::RenderSetState
        | ErrorCategory::StaticComponents
        | ErrorCategory::Suppression
        | ErrorCategory::Syntax
        | ErrorCategory::UseMemo => "Error",
    }
}

/// Format an error summary line: `"{heading}: {reason}"`.
///
/// Reference: `CompilerError.ts:517-563`
pub fn print_error_summary(category: ErrorCategory, reason: &str) -> String {
    format!("{}: {}", error_heading(category), reason)
}

/// Format a diagnostic as a single-line string matching upstream `CompilerDiagnostic.toString()`.
///
/// Format: `"{heading}: {reason}. {description}. ({line}:{column})"`
///
/// Reference: `CompilerError.ts:188-198`
pub fn format_diagnostic_string(
    category: ErrorCategory,
    reason: &str,
    description: Option<&str>,
    primary_loc: Option<SourceRange>,
) -> String {
    let mut buf = print_error_summary(category, reason);
    if let Some(desc) = description {
        buf.push_str(". ");
        buf.push_str(desc);
        buf.push('.');
    }
    if let Some(loc) = primary_loc {
        buf.push_str(&format!(" ({}:{})", loc.start.line, loc.start.column));
    }
    buf
}

/// Format a full error message with code frames matching upstream
/// `CompilerError.printErrorMessage()`.
///
/// Reference: `CompilerError.ts:390-399`
pub fn format_error_message(
    diagnostics: &[(
        ErrorCategory,
        &str,
        Option<&str>,
        &[CompilerDiagnosticDetail],
    )],
    source: &str,
) -> String {
    let count = diagnostics.len();
    let mut buf = format!(
        "Found {} error{}:\n\n",
        count,
        if count == 1 { "" } else { "s" }
    );
    for (i, (category, reason, description, details)) in diagnostics.iter().enumerate() {
        if i > 0 {
            buf.push_str("\n\n");
        }
        buf.push_str(&print_error_summary(*category, reason));
        if let Some(desc) = description {
            buf.push_str("\n\n");
            buf.push_str(desc);
            buf.push('.');
        }
        for detail in *details {
            match detail {
                CompilerDiagnosticDetail::Error { loc, message } => {
                    if let Some(loc) = loc {
                        buf.push_str("\n\n");
                        let frame =
                            generate_code_frame(source, *loc, message.as_deref().unwrap_or(""));
                        buf.push_str(&frame);
                    }
                }
                CompilerDiagnosticDetail::Hint { message } => {
                    buf.push_str("\n\n");
                    buf.push_str(message);
                }
            }
        }
    }
    buf
}

/// Generate a simple code frame around a source location.
///
/// Produces output similar to Babel's `codeFrameColumns`:
/// ```text
///   1 | const x = 1;
/// > 2 | foo(x);
///     |     ^ Error message
///   3 | const y = 2;
/// ```
pub fn generate_code_frame(source: &str, loc: SourceRange, message: &str) -> String {
    let lines: Vec<&str> = source.lines().collect();
    let start_line = loc.start.line as usize;
    let end_line = loc.end.line as usize;

    if start_line == 0 || start_line > lines.len() {
        return message.to_string();
    }

    let context_before = 1;
    let context_after = 1;
    let frame_start = start_line.saturating_sub(context_before);
    let frame_end = (end_line + context_after).min(lines.len());

    let max_line_num = frame_end;
    let gutter_width = max_line_num.to_string().len();

    let mut buf = String::new();
    for line_idx in frame_start..frame_end {
        let line_num = line_idx + 1;
        let line_content = lines.get(line_idx).unwrap_or(&"");
        let is_error_line = line_num >= start_line && line_num <= end_line;
        let marker = if is_error_line { ">" } else { " " };

        buf.push_str(&format!(
            "{} {:>width$} | {}\n",
            marker,
            line_num,
            line_content,
            width = gutter_width
        ));

        if is_error_line && line_num == start_line && !message.is_empty() {
            let col = loc.start.column as usize;
            let end_col = if start_line == end_line {
                loc.end.column as usize
            } else {
                line_content.len()
            };
            let underline_len = if end_col > col { end_col - col } else { 1 };
            buf.push_str(&format!(
                "  {:>width$} | {}{}  {}\n",
                "",
                " ".repeat(col),
                "^".repeat(underline_len),
                message,
                width = gutter_width
            ));
        }
    }
    buf
}

// =============================================================================
// Legacy types (preserved for backward compatibility during Phase 2 migration)
// =============================================================================

/// Severity levels for compiler diagnostics.
///
/// **Deprecated:** Will be replaced by [`ErrorCategory`] + [`ErrorSeverity`] in Phase 2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticSeverity {
    /// Fatal error — bail out of compilation for this function.
    InvalidReact,
    /// Input is valid but cannot be compiled (e.g., unsupported pattern).
    CannotPreserveMemoization,
    /// Internal compiler invariant violation.
    InvalidConfig,
    /// Todo — feature not yet implemented.
    Todo,
    /// Invariant violation (bug in the compiler).
    Invariant,
}

impl DiagnosticSeverity {
    /// Map legacy severity to an [`ErrorCategory`].
    pub fn to_error_category(self) -> ErrorCategory {
        match self {
            Self::InvalidReact => ErrorCategory::Purity,
            Self::CannotPreserveMemoization => ErrorCategory::PreserveManualMemo,
            Self::InvalidConfig => ErrorCategory::Config,
            Self::Todo => ErrorCategory::Todo,
            Self::Invariant => ErrorCategory::Invariant,
        }
    }
}

/// A compiler diagnostic.
///
/// **Note:** The `category` field defaults to `None` for legacy call sites.
/// Phase 2 migration will fill in the correct [`ErrorCategory`] for each site.
#[derive(Debug, Clone)]
pub struct CompilerDiagnostic {
    pub severity: DiagnosticSeverity,
    pub message: String,
    /// Upstream error category. `None` for unmigrated legacy call sites.
    pub category: Option<ErrorCategory>,
}

impl CompilerDiagnostic {
    /// Derive the effective [`ErrorSeverity`] for this diagnostic.
    pub fn error_severity(&self) -> ErrorSeverity {
        if let Some(cat) = self.category {
            get_rule_for_category(cat).severity
        } else {
            // Fallback: map from legacy severity
            match self.severity {
                DiagnosticSeverity::InvalidReact
                | DiagnosticSeverity::CannotPreserveMemoization
                | DiagnosticSeverity::InvalidConfig
                | DiagnosticSeverity::Invariant => ErrorSeverity::Error,
                DiagnosticSeverity::Todo => ErrorSeverity::Hint,
            }
        }
    }

    /// Get the effective [`ErrorCategory`].
    pub fn effective_category(&self) -> ErrorCategory {
        self.category
            .unwrap_or_else(|| self.severity.to_error_category())
    }

    /// Format this diagnostic as a single-line string matching upstream output.
    pub fn to_upstream_string(&self) -> String {
        let cat = self.effective_category();
        print_error_summary(cat, &self.message)
    }
}

impl Default for CompilerDiagnostic {
    fn default() -> Self {
        Self {
            severity: DiagnosticSeverity::Invariant,
            message: String::new(),
            category: None,
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

impl BailOut {
    /// Format this bailout's diagnostics in upstream `CompilerError.toString()` style.
    pub fn to_upstream_string(&self) -> String {
        if self.diagnostics.is_empty() {
            return self.reason.clone();
        }
        self.diagnostics
            .iter()
            .map(|d| d.to_upstream_string())
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

impl fmt::Display for BailOut {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BailOut: {}", self.reason)
    }
}

impl std::error::Error for BailOut {}

/// High-level error type for the compilation pipeline.
#[derive(Debug, Clone)]
pub enum CompilerError {
    /// Validation failure — bail to original code
    Bail(BailOut),
    /// Internal error — also bail to original code
    Internal(String),
    /// Lowering failure
    LoweringFailed(String),
}

impl CompilerError {
    // -- Factory methods matching upstream CompilerError static methods --

    /// Create a bail error for an invariant violation.
    ///
    /// Reference: `CompilerError.ts:285-300`
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
                category: Some(ErrorCategory::Invariant),
            }],
        })
    }

    /// Create a bail error for a Todo (unimplemented feature).
    ///
    /// Reference: `CompilerError.ts:308-319`
    pub fn throw_todo(reason: impl Into<String>, description: Option<String>) -> Self {
        let reason = reason.into();
        let message = if let Some(ref desc) = description {
            format!("{reason}. {desc}.")
        } else {
            reason.clone()
        };
        Self::Bail(BailOut {
            reason: reason.clone(),
            diagnostics: vec![CompilerDiagnostic {
                severity: DiagnosticSeverity::Todo,
                message,
                category: Some(ErrorCategory::Todo),
            }],
        })
    }

    /// Create a bail error for invalid JS syntax.
    ///
    /// Reference: `CompilerError.ts:321-332`
    pub fn throw_invalid_js(reason: impl Into<String>, description: Option<String>) -> Self {
        let reason = reason.into();
        let message = if let Some(ref desc) = description {
            format!("{reason}. {desc}.")
        } else {
            reason.clone()
        };
        Self::Bail(BailOut {
            reason: reason.clone(),
            diagnostics: vec![CompilerDiagnostic {
                severity: DiagnosticSeverity::InvalidReact,
                message,
                category: Some(ErrorCategory::Syntax),
            }],
        })
    }

    /// Create a bail error for invalid React usage.
    ///
    /// Reference: `CompilerError.ts:334-338`
    pub fn throw_invalid_react(
        category: ErrorCategory,
        reason: impl Into<String>,
        description: Option<String>,
    ) -> Self {
        let reason = reason.into();
        let message = if let Some(ref desc) = description {
            format!("{reason}. {desc}.")
        } else {
            reason.clone()
        };
        Self::Bail(BailOut {
            reason: reason.clone(),
            diagnostics: vec![CompilerDiagnostic {
                severity: DiagnosticSeverity::InvalidReact,
                message,
                category: Some(category),
            }],
        })
    }

    /// Create a bail error for invalid compiler configuration.
    ///
    /// Reference: `CompilerError.ts:340-351`
    pub fn throw_invalid_config(reason: impl Into<String>, description: Option<String>) -> Self {
        let reason = reason.into();
        let message = if let Some(ref desc) = description {
            format!("{reason}. {desc}.")
        } else {
            reason.clone()
        };
        Self::Bail(BailOut {
            reason: reason.clone(),
            diagnostics: vec![CompilerDiagnostic {
                severity: DiagnosticSeverity::InvalidConfig,
                message,
                category: Some(ErrorCategory::Config),
            }],
        })
    }

    /// Create a bail error with an explicit category.
    ///
    /// Reference: `CompilerError.ts:353-357`
    pub fn throw_diagnostic(
        category: ErrorCategory,
        reason: impl Into<String>,
        description: Option<String>,
    ) -> Self {
        let reason = reason.into();
        let severity = match get_rule_for_category(category).severity {
            ErrorSeverity::Error => DiagnosticSeverity::InvalidReact,
            ErrorSeverity::Warning => DiagnosticSeverity::CannotPreserveMemoization,
            ErrorSeverity::Hint => DiagnosticSeverity::Todo,
            ErrorSeverity::Off => DiagnosticSeverity::Todo,
        };
        let message = if let Some(ref desc) = description {
            format!("{reason}. {desc}.")
        } else {
            reason.clone()
        };
        Self::Bail(BailOut {
            reason: reason.clone(),
            diagnostics: vec![CompilerDiagnostic {
                severity,
                message,
                category: Some(category),
            }],
        })
    }

    /// Returns true if this error contains any active diagnostics.
    pub fn has_any_errors(&self) -> bool {
        match self {
            Self::Bail(b) => !b.diagnostics.is_empty(),
            Self::Internal(_) | Self::LoweringFailed(_) => true,
        }
    }

    /// Returns true if any diagnostic has [`ErrorSeverity::Error`] severity.
    pub fn has_errors(&self) -> bool {
        match self {
            Self::Bail(b) => b
                .diagnostics
                .iter()
                .any(|d| d.error_severity() == ErrorSeverity::Error),
            Self::Internal(_) | Self::LoweringFailed(_) => true,
        }
    }

    /// Returns true if there are no Errors and at least one Warning.
    pub fn has_warning(&self) -> bool {
        match self {
            Self::Bail(b) => {
                let mut has_warn = false;
                for d in &b.diagnostics {
                    if d.error_severity() == ErrorSeverity::Error {
                        return false;
                    }
                    if d.error_severity() == ErrorSeverity::Warning {
                        has_warn = true;
                    }
                }
                has_warn
            }
            _ => false,
        }
    }

    /// Returns true if there are no Errors/Warnings and at least one Hint.
    pub fn has_hints(&self) -> bool {
        match self {
            Self::Bail(b) => {
                let mut has_hint = false;
                for d in &b.diagnostics {
                    match d.error_severity() {
                        ErrorSeverity::Error | ErrorSeverity::Warning => return false,
                        ErrorSeverity::Hint => has_hint = true,
                        ErrorSeverity::Off => {}
                    }
                }
                has_hint
            }
            _ => false,
        }
    }

    /// Format in upstream `CompilerError.toString()` style.
    pub fn to_upstream_string(&self) -> String {
        match self {
            Self::Bail(b) => b.to_upstream_string(),
            Self::Internal(msg) => format!("Invariant: {msg}"),
            Self::LoweringFailed(msg) => format!("Invariant: {msg}"),
        }
    }
}

impl fmt::Display for CompilerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bail(b) => write!(f, "Bail: {}", b),
            Self::Internal(msg) => write!(f, "Internal: {}", msg),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_all_categories_have_rules() {
        let categories = [
            ErrorCategory::Hooks,
            ErrorCategory::CapitalizedCalls,
            ErrorCategory::StaticComponents,
            ErrorCategory::UseMemo,
            ErrorCategory::Factories,
            ErrorCategory::PreserveManualMemo,
            ErrorCategory::IncompatibleLibrary,
            ErrorCategory::Immutability,
            ErrorCategory::Globals,
            ErrorCategory::Refs,
            ErrorCategory::EffectDependencies,
            ErrorCategory::EffectSetState,
            ErrorCategory::EffectDerivationsOfState,
            ErrorCategory::ErrorBoundaries,
            ErrorCategory::Purity,
            ErrorCategory::RenderSetState,
            ErrorCategory::Invariant,
            ErrorCategory::Todo,
            ErrorCategory::Syntax,
            ErrorCategory::UnsupportedSyntax,
            ErrorCategory::Config,
            ErrorCategory::Gating,
            ErrorCategory::Suppression,
            ErrorCategory::AutomaticEffectDependencies,
            ErrorCategory::Fire,
            ErrorCategory::FBT,
        ];
        for cat in categories {
            let rule = get_rule_for_category(cat);
            assert_eq!(rule.category, cat);
            assert!(!rule.name.is_empty());
            assert!(!rule.description.is_empty());
        }
    }

    #[test]
    fn test_error_heading() {
        assert_eq!(error_heading(ErrorCategory::Hooks), "Error");
        assert_eq!(error_heading(ErrorCategory::Invariant), "Invariant");
        assert_eq!(error_heading(ErrorCategory::Todo), "Todo");
        assert_eq!(
            error_heading(ErrorCategory::PreserveManualMemo),
            "Compilation Skipped"
        );
        assert_eq!(
            error_heading(ErrorCategory::UnsupportedSyntax),
            "Compilation Skipped"
        );
    }

    #[test]
    fn test_print_error_summary() {
        assert_eq!(
            print_error_summary(ErrorCategory::Hooks, "Invalid hook call"),
            "Error: Invalid hook call"
        );
        assert_eq!(
            print_error_summary(ErrorCategory::Invariant, "unexpected null"),
            "Invariant: unexpected null"
        );
    }

    #[test]
    fn test_format_diagnostic_string() {
        let s = format_diagnostic_string(
            ErrorCategory::Hooks,
            "Invalid hook call",
            Some("Hooks must be called unconditionally"),
            Some(SourceRange {
                start: SourcePosition {
                    line: 10,
                    column: 5,
                },
                end: SourcePosition {
                    line: 10,
                    column: 20,
                },
            }),
        );
        assert_eq!(
            s,
            "Error: Invalid hook call. Hooks must be called unconditionally. (10:5)"
        );
    }

    #[test]
    fn test_severity_mapping() {
        // Upstream: Todo → Hint
        assert_eq!(
            get_rule_for_category(ErrorCategory::Todo).severity,
            ErrorSeverity::Hint
        );
        // Upstream: UnsupportedSyntax → Warning
        assert_eq!(
            get_rule_for_category(ErrorCategory::UnsupportedSyntax).severity,
            ErrorSeverity::Warning
        );
        // Upstream: IncompatibleLibrary → Warning
        assert_eq!(
            get_rule_for_category(ErrorCategory::IncompatibleLibrary).severity,
            ErrorSeverity::Warning
        );
        // Most categories → Error
        assert_eq!(
            get_rule_for_category(ErrorCategory::Hooks).severity,
            ErrorSeverity::Error
        );
    }

    #[test]
    fn test_code_frame() {
        let source = "const x = 1;\nfoo(x);\nconst y = 2;";
        let frame = generate_code_frame(
            source,
            SourceRange {
                start: SourcePosition { line: 2, column: 0 },
                end: SourcePosition { line: 2, column: 3 },
            },
            "unexpected call",
        );
        assert!(frame.contains("foo(x)"));
        assert!(frame.contains("^^^"));
        assert!(frame.contains("unexpected call"));
    }

    #[test]
    fn test_factory_methods() {
        let err = CompilerError::invariant("unexpected null", None);
        assert!(err.has_any_errors());
        assert!(err.has_errors());
        assert!(err.to_upstream_string().contains("Invariant:"));

        let err = CompilerError::throw_todo("not yet implemented", None);
        assert!(err.has_any_errors());
        assert!(err.to_upstream_string().contains("Todo:"));
    }

    #[test]
    fn test_backward_compat() {
        // Existing code pattern must still work
        let diag = CompilerDiagnostic {
            severity: DiagnosticSeverity::InvalidReact,
            message: "test".to_string(),
            category: None,
        };
        assert_eq!(diag.error_severity(), ErrorSeverity::Error);
    }
}
