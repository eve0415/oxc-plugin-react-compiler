//! Compiler error types.
//!
//! Port of `CompilerError.ts` from upstream.

use std::fmt;

/// Severity levels for compiler diagnostics.
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

/// A compiler diagnostic.
#[derive(Debug, Clone)]
pub struct CompilerDiagnostic {
    pub severity: DiagnosticSeverity,
    pub message: String,
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
