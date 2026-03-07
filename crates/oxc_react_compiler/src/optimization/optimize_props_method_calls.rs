//! OptimizePropsMethodCalls — rewrites `props.method(args)` to a direct call.
//!
//! Port of `Optimization/OptimizePropsMethodCalls.ts` from upstream React Compiler.
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.
//!
//! Converts method calls into regular calls where the receiver is the props object:
//!
//! ```text
//! // INPUT
//! props.foo();
//!
//! // OUTPUT
//! const t0 = props.foo;
//! t0();
//! ```
//!
//! Only rewrites when the receiver is directly the props object (i.e. `isPropsType`).
//! Does NOT rewrite `props.foo.bar()` because the receiver there is `props.foo`, not props.

use crate::hir::types::{HIRFunction, Identifier, InstructionValue, Type};

/// Returns true if the identifier has the `BuiltInProps` object shape,
/// matching the upstream `isPropsType` check.
fn is_props_type(id: &Identifier) -> bool {
    matches!(
        &id.type_,
        Type::Object { shape_id: Some(shape) } if shape == "BuiltInProps"
    )
}

/// Rewrite `props.method(args)` → `method(args)` (i.e. `CallExpression` with callee = property).
pub fn optimize_props_method_calls(func: &mut HIRFunction) {
    for (_block_id, block) in &mut func.body.blocks {
        for instr in &mut block.instructions {
            let should_rewrite = matches!(
                &instr.value,
                InstructionValue::MethodCall { receiver, .. }
                    if is_props_type(&receiver.identifier)
            );
            if should_rewrite {
                // Take the current value out and destructure it.
                let old_value = std::mem::replace(
                    &mut instr.value,
                    InstructionValue::Debugger {
                        loc: crate::hir::types::SourceLocation::Generated,
                    },
                );
                if let InstructionValue::MethodCall {
                    property,
                    args,
                    loc,
                    ..
                } = old_value
                {
                    instr.value = InstructionValue::CallExpression {
                        callee: property,
                        args,
                        optional: false,
                        loc,
                    };
                }
            }
        }
    }
}
