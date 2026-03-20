//! High-level Intermediate Representation (HIR).
//!
//! The HIR is the core data structure of the React Compiler. It represents
//! the program in a form suitable for analysis and transformation:
//! - Basic blocks with instructions and terminals
//! - SSA form (after the SSA pass)
//! - Phi nodes for control flow merges
//! - Places (identifier + optional property path) for tracking values

pub(crate) mod build;
pub(crate) mod build_reactive_scope_terminals;
pub(crate) mod builder;
pub(crate) mod collect_hoistable_property_loads;
pub(crate) mod collect_optional_chain_deps;
pub(crate) mod compute_unconditional_blocks;
pub(crate) mod derive_minimal_dependencies;
pub(crate) mod dominator;
pub(crate) mod flatten_reactive_loops;
pub(crate) mod flatten_scopes_with_hooks;
pub(crate) mod globals;
pub(crate) mod merge_consecutive_blocks;
pub(crate) mod object_shape;
pub(crate) mod propagate_scope_dependencies_hir;
pub(crate) mod prune_maybe_throws;
pub(crate) mod prune_unused_labels;
pub(crate) mod scope_dependency_utils;
pub(crate) mod transform_fire;
pub(crate) mod types;
pub(crate) mod visitors;
