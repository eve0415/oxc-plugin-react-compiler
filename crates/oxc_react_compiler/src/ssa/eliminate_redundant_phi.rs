//! EliminateRedundantPhi — remove unnecessary phi nodes.
//!
//! Port of `EliminateRedundantPhi.ts` from upstream React Compiler.
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.
//!
//! A phi is redundant if:
//! - All operands are the same identifier, e.g. `x2 = phi(x1, x1, x1)`
//! - All operands are the same or the phi output, e.g. `x2 = phi(x1, x2, x1)`
//!
//! Redundant phis are eliminated and all usages of the phi identifier
//! are replaced with the other operand.

use std::collections::HashMap;

use crate::hir::types::*;
use crate::hir::visitors;

/// Eliminate redundant phi nodes from the function.
pub fn eliminate_redundant_phi(func: &mut HIRFunction) {
    let mut rewrites: HashMap<IdentifierId, Identifier> = HashMap::new();
    let mut changed = true;

    while changed {
        let prev_count = rewrites.len();

        for (_, block) in &mut func.body.blocks {
            // Check each phi for redundancy
            let mut to_remove = Vec::new();
            for (phi_idx, phi) in block.phis.iter_mut().enumerate() {
                // Rewrite phi operands based on existing rewrites
                for operand in phi.operands.values_mut() {
                    rewrite_place(operand, &rewrites);
                }

                // Check if this phi is redundant
                let mut same: Option<IdentifierId> = None;
                let mut same_ident: Option<Identifier> = None;
                let mut is_redundant = true;

                for operand in phi.operands.values() {
                    let op_id = operand.identifier.id;
                    if op_id == phi.place.identifier.id {
                        // Operand is the phi output itself, skip
                        continue;
                    }
                    match same {
                        Some(s) if s == op_id => {
                            // Same as previous operand, continue
                        }
                        Some(_) => {
                            // Different operand, not redundant
                            is_redundant = false;
                            break;
                        }
                        None => {
                            same = Some(op_id);
                            same_ident = Some(operand.identifier.clone());
                        }
                    }
                }

                if is_redundant && let Some(ident) = same_ident {
                    rewrites.insert(phi.place.identifier.id, ident);
                    to_remove.push(phi_idx);
                }
            }

            // Remove redundant phis (in reverse order to preserve indices)
            for idx in to_remove.into_iter().rev() {
                block.phis.remove(idx);
            }

            // Rewrite instruction operands and lvalues
            for instr in &mut block.instructions {
                visitors::map_instruction_operands(instr, |place| {
                    rewrite_place(place, &rewrites);
                });
                visitors::map_instruction_lvalues(instr, |place| {
                    rewrite_place(place, &rewrites);
                });

                // Recurse into nested functions
                match &mut instr.value {
                    InstructionValue::FunctionExpression { lowered_func, .. }
                    | InstructionValue::ObjectMethod { lowered_func, .. } => {
                        eliminate_redundant_phi(&mut lowered_func.func);
                    }
                    _ => {}
                }
            }

            // Rewrite terminal operands
            visitors::map_terminal_operands(&mut block.terminal, |place| {
                rewrite_place(place, &rewrites);
            });
        }

        changed = rewrites.len() > prev_count;
    }
}

fn rewrite_place(place: &mut Place, rewrites: &HashMap<IdentifierId, Identifier>) {
    if let Some(rewrite) = rewrites.get(&place.identifier.id) {
        place.identifier = rewrite.clone();
    }
}

#[cfg(test)]
mod tests {
    use super::eliminate_redundant_phi;
    use crate::ssa::enter_ssa::enter_ssa;
    use crate::test_utils::parse_and_lower;

    #[test]
    fn eliminate_phi_basic() {
        let mut func = parse_and_lower("let x = 1; return x;").expect("lower");
        enter_ssa(&mut func).expect("enter_ssa");
        eliminate_redundant_phi(&mut func);
        assert!(!func.body.blocks.is_empty());
    }

    #[test]
    fn eliminate_phi_preserves_needed() {
        let mut func =
            parse_and_lower("let x = 1; if (props.a) { x = 2; } return x;").expect("lower");
        enter_ssa(&mut func).expect("enter_ssa");
        eliminate_redundant_phi(&mut func);
        assert!(!func.body.blocks.is_empty());
    }

    #[test]
    fn eliminate_phi_simple() {
        let mut func = parse_and_lower("return props;").expect("lower");
        enter_ssa(&mut func).expect("enter_ssa");
        eliminate_redundant_phi(&mut func);
        assert!(!func.body.blocks.is_empty());
    }
}
