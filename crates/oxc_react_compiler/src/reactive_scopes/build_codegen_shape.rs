//! Shape builder types for reactive codegen.
//!
//! These types describe the structured shape of a compiled function body,
//! enabling the AST backend to emit OXC AST nodes directly from the shape
//! rather than reparsing string output.

use oxc_ast::ast;

use crate::error::CompilerError;

// CachePrologue and FastRefreshPrologue are now defined in codegen_ast.rs.
pub use super::codegen_ast::{CachePrologue, FastRefreshPrologue};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GeneratedBodyShape {
    Unknown,
    Block {
        inner: Box<GeneratedBodyShape>,
    },
    Labeled {
        label: String,
        inner: Box<GeneratedBodyShape>,
    },
    Switch {
        discriminant: String,
        cases: Vec<GeneratedSwitchCase>,
    },
    DebuggerStatements(usize),
    ExpressionStatements(Vec<String>),
    AssignmentStatements(Vec<GeneratedAssignment>),
    GuardedBody {
        test: String,
        inner: Box<GeneratedBodyShape>,
    },
    GuardedExpressionStatements {
        test: String,
        expressions: Vec<String>,
    },
    GuardedReturnPrefix {
        test: String,
        consequent: Option<String>,
        inner: Box<GeneratedBodyShape>,
    },
    ConditionalBranches {
        test: String,
        consequent: Box<GeneratedBodyShape>,
        alternate: Box<GeneratedBodyShape>,
    },
    GuardedAssignments {
        test: String,
        assignments: Vec<GeneratedAssignment>,
    },
    WhileLoop {
        test: String,
        body: Box<GeneratedBodyShape>,
    },
    DoWhileLoop {
        test: String,
        body: Box<GeneratedBodyShape>,
    },
    ForLoop {
        init: Option<String>,
        test: Option<String>,
        update: Option<String>,
        body: Box<GeneratedBodyShape>,
    },
    ForInLoop {
        left: String,
        right: String,
        body: Box<GeneratedBodyShape>,
    },
    ForOfLoop {
        left: String,
        right: String,
        body: Box<GeneratedBodyShape>,
    },
    GuardedAssignmentExpressions {
        test: String,
        assignments: Vec<GeneratedAssignment>,
        expressions: Vec<String>,
    },
    ZeroDependencyMemoizedCachedValues {
        sentinel_slot: u32,
        setup_statements: Vec<String>,
        cached_values: Vec<GeneratedCachedValue>,
        restored_values: Vec<GeneratedCachedValue>,
    },
    MemoizedCachedValues {
        deps: Vec<(u32, String)>,
        setup_statements: Vec<String>,
        cached_values: Vec<GeneratedCachedValue>,
        restored_values: Vec<GeneratedCachedValue>,
    },
    MemoizedEarlyReturnSentinel {
        deps: Vec<(u32, String)>,
        setup_statements: Vec<String>,
        cached_values: Vec<GeneratedCachedValue>,
        restored_values: Vec<GeneratedCachedValue>,
        sentinel_name: String,
        final_return: Option<String>,
        fallback_body: Option<Box<GeneratedBodyShape>>,
    },
    TryCatch {
        catch_param: Option<String>,
        try_body: Box<GeneratedBodyShape>,
        catch_body: Box<GeneratedBodyShape>,
    },
    Break(Option<String>),
    Continue(Option<String>),
    ReturnVoid,
    ReturnIdentifier(String),
    ReturnExpression(String),
    ThrowExpression(String),
    BoundExpressionReturn {
        value_name: String,
        value_kind: ast::VariableDeclarationKind,
        expression: String,
    },
    AssignedExpressionReturn {
        value_name: String,
        value_kind: ast::VariableDeclarationKind,
        expression: String,
    },
    ZeroDependencyMemoizedReturn {
        value_name: String,
        value_kind: ast::VariableDeclarationKind,
        value_slot: u32,
        memoized_bindings: Vec<GeneratedBinding>,
        memoized_assignments: Vec<GeneratedAssignment>,
        memoized_expressions: Vec<String>,
        memoized_setup_statements: Vec<String>,
        memoized_expr: Option<String>,
    },
    ZeroDependencyMemoizedExistingReturn {
        value_name: String,
        value_slot: u32,
        memoized_bindings: Vec<GeneratedBinding>,
        memoized_assignments: Vec<GeneratedAssignment>,
        memoized_expressions: Vec<String>,
        memoized_setup_statements: Vec<String>,
        memoized_expr: Option<String>,
    },
    SingleDependencyMemoizedReturn {
        value_name: String,
        value_kind: ast::VariableDeclarationKind,
        dep_slot: u32,
        dep_expr: String,
        value_slot: u32,
        memoized_bindings: Vec<GeneratedBinding>,
        memoized_assignments: Vec<GeneratedAssignment>,
        memoized_expressions: Vec<String>,
        memoized_setup_statements: Vec<String>,
        memoized_expr: Option<String>,
    },
    SingleDependencyMemoizedExistingReturn {
        value_name: String,
        dep_slot: u32,
        dep_expr: String,
        value_slot: u32,
        memoized_bindings: Vec<GeneratedBinding>,
        memoized_assignments: Vec<GeneratedAssignment>,
        memoized_expressions: Vec<String>,
        memoized_setup_statements: Vec<String>,
        memoized_expr: Option<String>,
    },
    MultiDependencyMemoizedReturn {
        value_name: String,
        value_kind: ast::VariableDeclarationKind,
        deps: Vec<(u32, String)>,
        value_slot: u32,
        memoized_bindings: Vec<GeneratedBinding>,
        memoized_assignments: Vec<GeneratedAssignment>,
        memoized_expressions: Vec<String>,
        memoized_setup_statements: Vec<String>,
        memoized_expr: Option<String>,
    },
    MultiDependencyMemoizedExistingReturn {
        value_name: String,
        deps: Vec<(u32, String)>,
        value_slot: u32,
        memoized_bindings: Vec<GeneratedBinding>,
        memoized_assignments: Vec<GeneratedAssignment>,
        memoized_expressions: Vec<String>,
        memoized_setup_statements: Vec<String>,
        memoized_expr: Option<String>,
    },
    MemoizedComputedReturn {
        value_name: String,
        value_kind: Option<ast::VariableDeclarationKind>,
        deps: Vec<(u32, String)>,
        value_slot: u32,
        computation: Box<GeneratedBodyShape>,
    },
    WrappedReturnExpression {
        source_name: String,
        expression: String,
        inner: Box<GeneratedBodyShape>,
    },
    AssignedAliasReturn {
        alias_name: String,
        source_name: String,
        inner: Box<GeneratedBodyShape>,
    },
    AliasedReturn {
        alias_name: String,
        alias_kind: ast::VariableDeclarationKind,
        source_name: String,
        inner: Box<GeneratedBodyShape>,
    },
    PrefixedDeclarations {
        declarations: Vec<GeneratedDeclaration>,
        inner: Box<GeneratedBodyShape>,
    },
    PrefixedBindings {
        bindings: Vec<GeneratedBinding>,
        inner: Box<GeneratedBodyShape>,
    },
    PrefixedExpressionStatements {
        expressions: Vec<String>,
        inner: Box<GeneratedBodyShape>,
    },
    PrefixedAssignments {
        assignments: Vec<GeneratedAssignment>,
        inner: Box<GeneratedBodyShape>,
    },
    Sequential {
        prefix: Box<GeneratedBodyShape>,
        inner: Box<GeneratedBodyShape>,
    },
    SingleSlotMemoizedReturn {
        value_name: String,
        value_kind: ast::VariableDeclarationKind,
        temp_name: String,
        memoized_bindings: Vec<GeneratedBinding>,
        memoized_assignments: Vec<GeneratedAssignment>,
        memoized_expressions: Vec<String>,
        memoized_expr: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedBinding {
    pub kind: ast::VariableDeclarationKind,
    pub pattern: String,
    pub expression: String,
    pub promote_to_function_declaration: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedDeclaration {
    pub kind: ast::VariableDeclarationKind,
    pub pattern: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedAssignment {
    pub target: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedSwitchCase {
    pub test: Option<String>,
    pub consequent: GeneratedBodyShape,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedCachedValue {
    pub name: String,
    pub slot: u32,
}

pub struct CodegenResult {
    /// Structured shape of the emitted body for downstream AST-based rewrites.
    pub body_shape: GeneratedBodyShape,
    /// Number of cache slots used.
    pub cache_size: u32,
    /// Whether the function needs the cache import.
    pub needs_cache_import: bool,
    /// Rendered parameter names for this function body.
    pub param_names: Vec<String>,
    /// Whether the function needs the makeReadOnly import (enableEmitFreeze).
    pub needs_freeze_import: bool,
    /// Whether this function has a fire rewrite (needs useFire import).
    pub has_fire_rewrite: bool,
    /// Whether this function emitted runtime hook guards.
    pub needs_hook_guards: bool,
    /// Whether this function needs the top-level hook guard try/finally wrapper.
    pub needs_function_hook_guard_wrapper: bool,
    /// Whether this function emitted `$structuralCheck` calls.
    pub needs_structural_check_import: bool,
    /// Structured cache prologue metadata for AST emission.
    pub cache_prologue: Option<CachePrologue>,
    /// Deferred codegen error (upstream invariant parity).
    pub error: Option<CompilerError>,
}
