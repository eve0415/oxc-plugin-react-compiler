//! Validates against components that are created dynamically and whose identity
//! is not guaranteed to be stable (which would cause the component to reset on
//! each re-render).
//!
//! Port of `ValidateStaticComponents.ts` from upstream React Compiler.
//!
//! This pass runs on the **HIR** (CFG form).
//! It is gated behind `env.config.validateStaticComponents`.

use std::collections::HashMap;

use crate::error::{BailOut, CompilerDiagnostic, CompilerError, DiagnosticSeverity};
use crate::hir::types::*;

/// Validates that components used in JSX are not dynamically created during render.
pub fn validate_static_components(func: &HIRFunction) -> Result<(), CompilerError> {
    let mut known_dynamic_components: HashMap<IdentifierId, SourceLocation> = HashMap::new();
    let mut diagnostics: Vec<CompilerDiagnostic> = Vec::new();

    for (_bid, block) in &func.body.blocks {
        'phis: for phi in &block.phis {
            for operand in phi.operands.values() {
                if let Some(loc) = known_dynamic_components.get(&operand.identifier.id) {
                    known_dynamic_components.insert(phi.place.identifier.id, loc.clone());
                    continue 'phis;
                }
            }
        }

        for instr in &block.instructions {
            let lvalue_id = instr.lvalue.identifier.id;
            match &instr.value {
                InstructionValue::FunctionExpression { loc, .. }
                | InstructionValue::NewExpression { loc, .. }
                | InstructionValue::MethodCall { loc, .. }
                | InstructionValue::CallExpression { loc, .. } => {
                    known_dynamic_components.insert(lvalue_id, loc.clone());
                }
                InstructionValue::LoadLocal { place, .. } => {
                    if let Some(loc) = known_dynamic_components.get(&place.identifier.id) {
                        known_dynamic_components.insert(lvalue_id, loc.clone());
                    }
                }
                InstructionValue::StoreLocal { lvalue, value, .. } => {
                    if let Some(loc) = known_dynamic_components.get(&value.identifier.id) {
                        let loc = loc.clone();
                        known_dynamic_components.insert(instr.lvalue.identifier.id, loc.clone());
                        known_dynamic_components.insert(lvalue.place.identifier.id, loc);
                    }
                }
                InstructionValue::JsxExpression { tag, .. } => {
                    if let JsxTag::Component(tag_place) = tag
                        && known_dynamic_components.contains_key(&tag_place.identifier.id)
                    {
                        diagnostics.push(CompilerDiagnostic {
                                severity: DiagnosticSeverity::InvalidReact,
                                message: "Components created during render will reset their state each time they are created. Declare components outside of render".to_string(),
                                category: None,
                            });
                    }
                }
                _ => {}
            }
        }
    }

    if diagnostics.is_empty() {
        Ok(())
    } else {
        Err(CompilerError::Bail(BailOut {
            reason: "Dynamic component creation".to_string(),
            diagnostics,
        }))
    }
}
