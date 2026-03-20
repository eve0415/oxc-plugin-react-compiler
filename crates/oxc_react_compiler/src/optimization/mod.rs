//! Optimization passes.
//!
//! Port of the Optimization/ directory from upstream React Compiler.
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.

pub(crate) mod constant_propagation;
pub(crate) mod dead_code_elimination;
pub(crate) mod drop_manual_memoization;
pub(crate) mod inline_iifes;
pub(crate) mod inline_jsx_transform;
pub(crate) mod instruction_reordering;
pub(crate) mod lower_context_access;
pub(crate) mod name_anonymous_functions;
pub(crate) mod optimize_props_method_calls;
pub(crate) mod outline_functions;
pub(crate) mod outline_jsx;
