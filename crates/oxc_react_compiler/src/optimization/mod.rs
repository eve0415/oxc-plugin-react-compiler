//! Optimization passes.
//!
//! Port of the Optimization/ directory from upstream React Compiler.
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.

pub mod constant_propagation;
pub mod dead_code_elimination;
pub mod drop_manual_memoization;
pub mod inline_iifes;
pub mod inline_jsx_transform;
pub mod instruction_reordering;
pub mod lower_context_access;
pub mod name_anonymous_functions;
pub mod optimize_props_method_calls;
pub mod outline_functions;
pub mod outline_jsx;
pub mod remove_unnecessary_try_catch;
