//! SSA conversion passes.
//!
//! Port of the SSA/ directory from upstream React Compiler.
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.

pub mod eliminate_redundant_phi;
pub mod enter_ssa;
pub mod rewrite_instruction_kinds;
