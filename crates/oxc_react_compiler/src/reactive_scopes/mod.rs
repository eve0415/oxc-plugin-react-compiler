//! Reactive scope analysis and code generation.
//!
//! Port of the ReactiveScopes/ directory from upstream React Compiler.
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.

pub(crate) mod align_method_call_scopes;
pub(crate) mod align_object_method_scopes;
pub(crate) mod align_scopes;
pub(crate) mod build_reactive_function;
pub(crate) mod extract_scope_destructuring;
pub(crate) mod infer_reactive;
pub(crate) mod infer_scope_variables;
pub(crate) mod memoize_fbt_operands;
pub(crate) mod merge_overlapping_scopes;
pub(crate) mod merge_scopes_invalidate_together;
pub(crate) mod promote_used_temporaries;
pub(crate) mod propagate_early_returns;
pub(crate) mod prune_always_invalidating_reactive;
pub(crate) mod prune_hoisted_contexts;
pub(crate) mod prune_initialization_dependencies;
pub(crate) mod prune_non_escaping_scopes;
pub(crate) mod prune_non_reactive_deps_reactive;
pub(crate) mod prune_unused_labels_reactive;
pub(crate) mod prune_unused_lvalues;
pub(crate) mod prune_unused_scopes_reactive;
pub(crate) mod rename_variables;
pub(crate) mod stabilize_block_ids;
