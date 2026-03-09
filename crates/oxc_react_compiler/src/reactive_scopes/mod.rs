//! Reactive scope analysis and code generation.
//!
//! Port of the ReactiveScopes/ directory from upstream React Compiler.
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.

pub mod align_method_call_scopes;
pub mod align_object_method_scopes;
pub mod align_scopes;
pub mod build_reactive_function;
pub mod codegen_reactive;
pub mod extract_scope_destructuring;
pub mod fuse_trailing_nullish_return_into_scope;
pub mod infer_reactive;
pub mod infer_scope_variables;
pub mod memoize_fbt_operands;
pub mod merge_overlapping_scopes;
pub mod merge_scopes_invalidate_together;
pub mod promote_used_temporaries;
pub mod propagate_early_returns;
pub mod propagate_scope_dependencies;
pub mod prune_always_invalidating_reactive;
pub mod prune_hoisted_contexts;
pub mod prune_initialization_dependencies;
pub mod prune_non_escaping_scopes;
pub mod prune_non_reactive_deps;
pub mod prune_non_reactive_deps_reactive;
pub mod prune_scopes;
pub mod prune_unused_labels_reactive;
pub mod prune_unused_lvalues;
pub mod prune_unused_scopes_reactive;
pub mod rename_variables;
pub mod stabilize_block_ids;
