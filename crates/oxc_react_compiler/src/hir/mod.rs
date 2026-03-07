//! High-level Intermediate Representation (HIR).
//!
//! The HIR is the core data structure of the React Compiler. It represents
//! the program in a form suitable for analysis and transformation:
//! - Basic blocks with instructions and terminals
//! - SSA form (after the SSA pass)
//! - Phi nodes for control flow merges
//! - Places (identifier + optional property path) for tracking values

pub mod build;
pub mod build_reactive_scope_terminals;
pub mod builder;
pub mod collect_hoistable_property_loads;
pub mod collect_optional_chain_deps;
pub mod compute_unconditional_blocks;
pub mod derive_minimal_dependencies;
pub mod dominator;
pub mod flatten_reactive_loops;
pub mod flatten_scopes_with_hooks;
pub mod globals;
pub mod merge_consecutive_blocks;
pub mod object_shape;
pub mod propagate_scope_dependencies_hir;
pub mod prune_maybe_throws;
pub mod prune_unused_labels;
pub mod scope_dependency_utils;
pub mod transform_fire;
pub mod types;
pub mod visitors;
