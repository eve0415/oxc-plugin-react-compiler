//! Validation passes for the HIR.
//!
//! These passes validate the HIR and report errors that cause bail-out.
//! Port of the `Validation/` directory from upstream React Compiler.

pub(crate) mod validate_context_variable_lvalues;
pub(crate) mod validate_hooks_usage;
pub(crate) mod validate_locals_not_reassigned_after_render;
pub(crate) mod validate_memoized_effect_dependencies;
pub(crate) mod validate_no_capitalized_calls;
pub(crate) mod validate_no_derived_computations_in_effects;
pub(crate) mod validate_no_freezing_known_mutable_functions;
pub(crate) mod validate_no_impure_functions_in_render;
pub(crate) mod validate_no_jsx_in_try_statement;
pub(crate) mod validate_no_ref_access_in_render;
pub(crate) mod validate_no_set_state_in_effects;
pub(crate) mod validate_no_set_state_in_render;
pub(crate) mod validate_preserved_manual_memoization;
pub(crate) mod validate_static_components;
pub(crate) mod validate_use_memo;
