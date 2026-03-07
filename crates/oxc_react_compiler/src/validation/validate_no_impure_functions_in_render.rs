//! Validates that known impure functions are not called during render.
//!
//! Port of `ValidateNoImpureFunctionsInRender.ts` from upstream React Compiler.
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.
//!
//! Checks that known-impure functions are not called during render. Examples of
//! invalid functions to call during render are `Math.random()` and `Date.now()`.
//! Users may extend this set of impure functions via a module type provider and
//! specifying functions with `impure: true`.
//!
//! TODO: The upstream checks `getFunctionCallSignature(fn.env, callee.identifier.type)`
//! and looks for `signature.impure === true`. This requires the environment and type
//! system to be fully wired up. For now, this is a simplified stub.

use crate::error::CompilerError;
use crate::hir::types::*;

/// Validates that known impure functions are not called during render.
///
/// This is currently a simplified stub that always passes validation.
/// The full implementation requires:
/// - Access to the function's environment (fn.env)
/// - The `getFunctionCallSignature` helper from InferMutationAliasingEffects
/// - Checking the `impure` flag on function signatures
///
/// Once the environment and type inference infrastructure is complete,
/// this should check CallExpression and MethodCall instructions for
/// callees whose function signatures have `impure: true`.
pub fn validate_no_impure_functions_in_render(_func: &HIRFunction) -> Result<(), CompilerError> {
    // TODO: Implement full impure function detection. The upstream pass:
    //
    // 1. Iterates over all blocks and instructions
    // 2. For MethodCall and CallExpression instructions:
    //    a. Gets the callee (property for MethodCall, callee for CallExpression)
    //    b. Calls getFunctionCallSignature(fn.env, callee.identifier.type)
    //    c. If signature.impure === true, pushes a diagnostic
    // 3. Returns collected errors
    //
    // This requires the environment's type system to be available, which
    // maps identifiers to function signatures with impure flags.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hir::types::*;

    fn make_test_place(id: u32) -> Place {
        Place {
            identifier: Identifier {
                id: IdentifierId(id),
                declaration_id: DeclarationId(id),
                name: None,
                mutable_range: MutableRange::default(),
                scope: None,
                type_: Type::Poly,
                loc: SourceLocation::Generated,
            },
            effect: Effect::Unknown,
            reactive: false,
            loc: SourceLocation::Generated,
        }
    }

    fn make_hir_function(blocks: Vec<(BlockId, BasicBlock)>) -> HIRFunction {
        HIRFunction {
            env: crate::environment::Environment::new(crate::options::EnvironmentConfig::default()),
            loc: SourceLocation::Generated,
            id: None,
            fn_type: ReactFunctionType::Component,
            params: vec![],
            returns: make_test_place(0),
            context: vec![],
            body: HIR {
                entry: blocks.first().map(|(id, _)| *id).unwrap_or(BlockId(0)),
                blocks,
            },
            generator: false,
            async_: false,
            directives: vec![],
            aliasing_effects: None,
        }
    }

    #[test]
    fn test_stub_always_passes() {
        let func = make_hir_function(vec![]);
        assert!(validate_no_impure_functions_in_render(&func).is_ok());
    }
}
