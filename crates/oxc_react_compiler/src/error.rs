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

impl Default for CompilerDiagnostic {
    fn default() -> Self {
        Self {
            severity: DiagnosticSeverity::Invariant,
            message: String::new(),
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
        };
    }
}
