//! SSA conversion passes.
//!
//! Port of the SSA/ directory from upstream React Compiler.
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.

pub(crate) mod eliminate_redundant_phi;
pub(crate) mod enter_ssa;
pub(crate) mod rewrite_instruction_kinds;
