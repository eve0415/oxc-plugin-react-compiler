//! FlattenScopesWithHooksOrUseHIR — remove reactive scopes containing hook calls.
//!
//! Port of `ReactiveScopes/FlattenScopesWithHooksOrUseHIR.ts` from upstream React Compiler.
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.
//!
//! Hooks cannot be called conditionally, and the `use` operator must always be
//! called if the component needs its return value. Memoizing a scope that contains
//! a hook or `use` call would make that call conditional in the output. This pass
//! finds and removes any reactive scopes that transitively contain a hook or `use`
//! call.
//!
//! For scopes that contain only a single hook call instruction and immediately
//! fall through, the scope is converted to a `Label` terminal (to be cleaned up
//! by `PruneUnusedLabels`). Otherwise, the scope is converted to `PrunedScope`.

use crate::environment::Environment;
use std::collections::HashSet;

use super::types::*;

/// Holds an active scope entry: the block containing the Scope terminal
/// and the block where the scope falls through.
struct ActiveScope {
    /// Block ID of the block whose terminal is `Scope`.
    block: BlockId,
    /// The fallthrough block ID of that Scope terminal.
    fallthrough: BlockId,
}

/// Check if an identifier represents a hook or `use` operator.
///
/// In the upstream compiler, this checks `getHookKind(fn.env, callee.identifier)`
/// and `isUseOperator(callee.identifier)`. Since we don't yet have full type-based
/// hook resolution, we approximate by checking the identifier's name against the
/// standard hook naming convention (`use` or `use[A-Z]...`).
fn is_hook_or_use_identifier(ident: &Identifier) -> bool {
    if is_hook_function_type(&ident.type_) {
        return true;
    }
    match &ident.name {
        Some(name) => Environment::is_hook_name(name.value()),
        None => false,
    }
}

/// Check if an instruction is a hook or `use` call.
fn is_hook_or_use_call(instr: &Instruction, hook_aliases: &HashSet<IdentifierId>) -> bool {
    match &instr.value {
        InstructionValue::CallExpression { callee, .. } => {
            is_hook_or_use_identifier(&callee.identifier)
                || hook_aliases.contains(&callee.identifier.id)
        }
        InstructionValue::MethodCall { property, .. } => {
            is_hook_or_use_identifier(&property.identifier)
                || hook_aliases.contains(&property.identifier.id)
        }
        _ => false,
    }
}

fn is_hook_or_use_name(name: &str) -> bool {
    if Environment::is_hook_name(name) || name == "use" {
        return true;
    }
    // Module-local aliases may be mangled as `Prefix$useState` or
    // `Internal$Reassigned$useHook`. Upstream `getHookKind` resolves these by
    // symbol type; mirror that behavior here by checking `$`-separated suffixes.
    name.rsplit('$')
        .any(|segment| Environment::is_hook_name(segment) || segment == "use")
}

fn is_hook_function_type(ty: &Type) -> bool {
    match ty {
        Type::Function {
            shape_id: Some(shape_id),
            ..
        } => matches!(
            shape_id.as_str(),
            "BuiltInUseStateHookId"
                | "BuiltInUseReducerHookId"
                | "BuiltInUseContextHookId"
                | "BuiltInUseRefHookId"
                | "BuiltInUseMemoHookId"
                | "BuiltInUseCallbackHookId"
                | "BuiltInUseEffectHookId"
                | "BuiltInUseTransitionHookId"
                | "BuiltInUseImperativeHandleHookId"
                | "BuiltInUseActionStateHookId"
                | "BuiltInDefaultMutatingHookId"
                | "BuiltInDefaultNonmutatingHookId"
        ),
        _ => false,
    }
}

fn hook_name_for_binding(binding: &NonLocalBinding) -> &str {
    match binding {
        NonLocalBinding::ImportSpecifier { imported, .. } => imported.as_str(),
        _ => binding.name(),
    }
}

/// Track identifiers that alias a hook/use callee.
///
/// Lowering often emits:
///   `t1 = LoadGlobal(useSomething)` then `t2 = CallExpression(callee=t1, ...)`
/// In that form the callee identifier has no name, so we need alias tracking.
fn update_hook_aliases(instr: &Instruction, hook_aliases: &mut HashSet<IdentifierId>) {
    match &instr.value {
        InstructionValue::LoadGlobal { binding, .. } => {
            if is_hook_or_use_name(hook_name_for_binding(binding)) {
                hook_aliases.insert(instr.lvalue.identifier.id);
            }
        }
        InstructionValue::PropertyLoad {
            property: PropertyLiteral::String(name),
            ..
        } => {
            if is_hook_or_use_name(name) {
                hook_aliases.insert(instr.lvalue.identifier.id);
            }
        }
        InstructionValue::ComputedLoad { property, .. } => {
            if hook_aliases.contains(&property.identifier.id) {
                hook_aliases.insert(instr.lvalue.identifier.id);
            }
        }
        InstructionValue::LoadLocal { place, .. } | InstructionValue::LoadContext { place, .. } => {
            if hook_aliases.contains(&place.identifier.id) {
                hook_aliases.insert(instr.lvalue.identifier.id);
            }
        }
        InstructionValue::TypeCastExpression { value, .. } => {
            if hook_aliases.contains(&value.identifier.id) {
                hook_aliases.insert(instr.lvalue.identifier.id);
            }
        }
        InstructionValue::Primitive { value, .. } => {
            if let PrimitiveValue::String(name) = value
                && is_hook_or_use_name(name)
            {
                hook_aliases.insert(instr.lvalue.identifier.id);
            }
        }
        _ => {}
    }
}

/// Flatten reactive scopes that contain hook or `use` calls.
///
/// The algorithm:
/// 1. Walk blocks in order, tracking active scope terminals.
/// 2. When a hook/use call is encountered, mark all active scopes for pruning.
/// 3. In a second pass, rewrite marked scope terminals:
///    - If the scope body has exactly 1 instruction and its terminal is a Goto
///      to the scope's fallthrough, convert to `Label` (will be cleaned up later).
///    - Otherwise convert to `PrunedScope`.
pub fn flatten_scopes_with_hooks_or_use_hir(func: &mut HIRFunction) {
    let mut active_scopes: Vec<ActiveScope> = Vec::new();
    let mut prune: Vec<BlockId> = Vec::new();
    let mut hook_aliases: HashSet<IdentifierId> = HashSet::new();
    let debug_hooks = std::env::var("DEBUG_FLATTEN_HOOKS").is_ok();

    // Phase 1: Identify which scope blocks need pruning.
    for idx in 0..func.body.blocks.len() {
        let block_id = func.body.blocks[idx].1.id;

        // Remove active scopes whose fallthrough matches the current block.
        active_scopes.retain(|s| s.fallthrough != block_id);

        // Check instructions for hook/use calls.
        for instr_idx in 0..func.body.blocks[idx].1.instructions.len() {
            let instr = &func.body.blocks[idx].1.instructions[instr_idx];
            update_hook_aliases(instr, &mut hook_aliases);
            if debug_hooks {
                match &instr.value {
                    InstructionValue::CallExpression { callee, .. } => {
                        let direct = is_hook_or_use_identifier(&callee.identifier);
                        let alias = hook_aliases.contains(&callee.identifier.id);
                        if direct || alias {
                            eprintln!(
                                "[FLATTEN_HOOKS] bb{} instr#{} call callee_id={} decl={} name={:?} direct={} alias={} active_scopes={:?}",
                                block_id.0,
                                instr.id.0,
                                callee.identifier.id.0,
                                callee.identifier.declaration_id.0,
                                callee.identifier.name,
                                direct,
                                alias,
                                active_scopes.iter().map(|s| s.block.0).collect::<Vec<_>>()
                            );
                        }
                    }
                    InstructionValue::MethodCall { property, .. } => {
                        let direct = is_hook_or_use_identifier(&property.identifier);
                        let alias = hook_aliases.contains(&property.identifier.id);
                        if direct || alias {
                            eprintln!(
                                "[FLATTEN_HOOKS] bb{} instr#{} method callee_id={} decl={} name={:?} direct={} alias={} active_scopes={:?}",
                                block_id.0,
                                instr.id.0,
                                property.identifier.id.0,
                                property.identifier.declaration_id.0,
                                property.identifier.name,
                                direct,
                                alias,
                                active_scopes.iter().map(|s| s.block.0).collect::<Vec<_>>()
                            );
                        }
                    }
                    _ => {}
                }
            }
            if is_hook_or_use_call(instr, &hook_aliases) {
                if debug_hooks {
                    eprintln!(
                        "[FLATTEN_HOOKS] pruning scopes from bb{} instr#{} -> {:?}",
                        block_id.0,
                        instr.id.0,
                        active_scopes.iter().map(|s| s.block.0).collect::<Vec<_>>()
                    );
                }
                // All currently active scopes must be pruned.
                for scope in &active_scopes {
                    prune.push(scope.block);
                }
                active_scopes.clear();
                break; // No need to check further instructions in this block.
            }
        }

        // If this block's terminal is a Scope, track it.
        if let Terminal::Scope { fallthrough, .. } = &func.body.blocks[idx].1.terminal {
            active_scopes.push(ActiveScope {
                block: block_id,
                fallthrough: *fallthrough,
            });
        }
    }

    if prune.is_empty() {
        return;
    }

    // Phase 2: Rewrite the identified scope terminals.
    for prune_block_id in &prune {
        // Find the block index for this block ID.
        let block_idx = match func
            .body
            .blocks
            .iter()
            .position(|(_, b)| b.id == *prune_block_id)
        {
            Some(idx) => idx,
            None => continue,
        };

        // Extract the Scope terminal.
        let terminal = &func.body.blocks[block_idx].1.terminal;
        let (scope_body_block_id, scope_fallthrough, term_id, term_loc, scope) = match terminal {
            Terminal::Scope {
                block,
                fallthrough,
                id,
                loc,
                scope,
            } => (*block, *fallthrough, *id, loc.clone(), scope.clone()),
            _ => {
                // Should be a scope terminal per the algorithm, but be defensive.
                continue;
            }
        };

        // Check if the scope body is a single-instruction block that gotos the fallthrough.
        // If so, convert to Label; otherwise convert to PrunedScope.
        let body_idx = func
            .body
            .blocks
            .iter()
            .position(|(_, b)| b.id == scope_body_block_id);

        let use_label = if let Some(body_idx) = body_idx {
            let body = &func.body.blocks[body_idx].1;
            body.instructions.len() == 1
                && matches!(
                    &body.terminal,
                    Terminal::Goto { block, .. } if *block == scope_fallthrough
                )
        } else {
            false
        };

        if use_label {
            func.body.blocks[block_idx].1.terminal = Terminal::Label {
                block: scope_body_block_id,
                fallthrough: scope_fallthrough,
                id: term_id,
                loc: term_loc,
            };
        } else {
            func.body.blocks[block_idx].1.terminal = Terminal::PrunedScope {
                block: scope_body_block_id,
                fallthrough: scope_fallthrough,
                scope,
                id: term_id,
                loc: term_loc,
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};

    fn make_place(ident_id: u32) -> Place {
        Place {
            identifier: Identifier {
                id: IdentifierId(ident_id),
                declaration_id: DeclarationId(ident_id),
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

    fn make_named_place(ident_id: u32, name: &str) -> Place {
        Place {
            identifier: Identifier {
                id: IdentifierId(ident_id),
                declaration_id: DeclarationId(ident_id),
                name: Some(IdentifierName::Named(name.to_string())),
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

    fn make_func(blocks: Vec<(BlockId, BasicBlock)>) -> HIRFunction {
        HIRFunction {
            env: crate::environment::Environment::new(crate::options::EnvironmentConfig::default()),
            loc: SourceLocation::Generated,
            id: None,
            fn_type: ReactFunctionType::Component,
            params: vec![],
            returns: make_place(99),
            context: vec![],
            body: HIR {
                entry: BlockId(0),
                blocks,
            },
            generator: false,
            async_: false,
            directives: vec![],
            aliasing_effects: None,
        }
    }

    fn make_scope() -> ReactiveScope {
        ReactiveScope {
            id: ScopeId(1),
            range: MutableRange {
                start: InstructionId(1),
                end: InstructionId(10),
            },
            dependencies: vec![],
            declarations: Default::default(),
            reassignments: vec![],
            merged_id: None,
            early_return_value: None,
        }
    }

    #[test]
    fn test_scope_with_hook_call_is_pruned() {
        // Block 0: Scope { block: 1, fallthrough: 2 }
        // Block 1: [CallExpression(useState)] + [Primitive] -> Goto -> 2
        // Block 2: Return
        //
        // The scope wraps block 1 which contains a useState call.
        // Since block 1 has 2 instructions, it should become PrunedScope (not Label).
        let mut func = make_func(vec![
            (
                BlockId(0),
                BasicBlock {
                    kind: BlockKind::Block,
                    id: BlockId(0),
                    instructions: vec![],
                    terminal: Terminal::Scope {
                        block: BlockId(1),
                        fallthrough: BlockId(2),
                        scope: make_scope(),
                        id: InstructionId(1),
                        loc: SourceLocation::Generated,
                    },
                    preds: HashSet::new(),
                    phis: vec![],
                },
            ),
            (
                BlockId(1),
                BasicBlock {
                    kind: BlockKind::Block,
                    id: BlockId(1),
                    instructions: vec![
                        Instruction {
                            id: InstructionId(2),
                            lvalue: make_place(1),
                            value: InstructionValue::CallExpression {
                                callee: make_named_place(10, "useState"),
                                args: vec![],
                                optional: false,
                                loc: SourceLocation::Generated,
                            },
                            loc: SourceLocation::Generated,
                            effects: None,
                        },
                        Instruction {
                            id: InstructionId(3),
                            lvalue: make_place(2),
                            value: InstructionValue::Primitive {
                                value: PrimitiveValue::Null,
                                loc: SourceLocation::Generated,
                            },
                            loc: SourceLocation::Generated,
                            effects: None,
                        },
                    ],
                    terminal: Terminal::Goto {
                        block: BlockId(2),
                        variant: GotoVariant::Break,
                        id: InstructionId(4),
                        loc: SourceLocation::Generated,
                    },
                    preds: HashSet::new(),
                    phis: vec![],
                },
            ),
            (
                BlockId(2),
                BasicBlock {
                    kind: BlockKind::Block,
                    id: BlockId(2),
                    instructions: vec![],
                    terminal: Terminal::Return {
                        value: make_place(1),
                        return_variant: ReturnVariant::Explicit,
                        id: InstructionId(5),
                        loc: SourceLocation::Generated,
                    },
                    preds: HashSet::new(),
                    phis: vec![],
                },
            ),
        ]);

        flatten_scopes_with_hooks_or_use_hir(&mut func);

        // Block 0 should now have PrunedScope terminal (body has 2 instructions, not 1).
        assert!(
            matches!(func.body.blocks[0].1.terminal, Terminal::PrunedScope { .. }),
            "Expected PrunedScope, got {:?}",
            std::mem::discriminant(&func.body.blocks[0].1.terminal)
        );
    }

    #[test]
    fn test_scope_with_single_hook_call_becomes_label() {
        // Block 0: Scope { block: 1, fallthrough: 2 }
        // Block 1: [CallExpression(useEffect)] -> Goto -> 2
        // Block 2: Return
        //
        // The scope body has exactly 1 instruction and gotos the fallthrough,
        // so it should become a Label terminal.
        let mut func = make_func(vec![
            (
                BlockId(0),
                BasicBlock {
                    kind: BlockKind::Block,
                    id: BlockId(0),
                    instructions: vec![],
                    terminal: Terminal::Scope {
                        block: BlockId(1),
                        fallthrough: BlockId(2),
                        scope: make_scope(),
                        id: InstructionId(1),
                        loc: SourceLocation::Generated,
                    },
                    preds: HashSet::new(),
                    phis: vec![],
                },
            ),
            (
                BlockId(1),
                BasicBlock {
                    kind: BlockKind::Block,
                    id: BlockId(1),
                    instructions: vec![Instruction {
                        id: InstructionId(2),
                        lvalue: make_place(1),
                        value: InstructionValue::CallExpression {
                            callee: make_named_place(10, "useEffect"),
                            args: vec![],
                            optional: false,
                            loc: SourceLocation::Generated,
                        },
                        loc: SourceLocation::Generated,
                        effects: None,
                    }],
                    terminal: Terminal::Goto {
                        block: BlockId(2),
                        variant: GotoVariant::Break,
                        id: InstructionId(3),
                        loc: SourceLocation::Generated,
                    },
                    preds: HashSet::new(),
                    phis: vec![],
                },
            ),
            (
                BlockId(2),
                BasicBlock {
                    kind: BlockKind::Block,
                    id: BlockId(2),
                    instructions: vec![],
                    terminal: Terminal::Return {
                        value: make_place(1),
                        return_variant: ReturnVariant::Explicit,
                        id: InstructionId(4),
                        loc: SourceLocation::Generated,
                    },
                    preds: HashSet::new(),
                    phis: vec![],
                },
            ),
        ]);

        flatten_scopes_with_hooks_or_use_hir(&mut func);

        // Block 0 should now have Label terminal.
        assert!(
            matches!(func.body.blocks[0].1.terminal, Terminal::Label { .. }),
            "Expected Label, got {:?}",
            std::mem::discriminant(&func.body.blocks[0].1.terminal)
        );
    }

    #[test]
    fn test_scope_without_hooks_not_pruned() {
        // Block 0: Scope { block: 1, fallthrough: 2 }
        // Block 1: [Primitive] -> Goto -> 2
        // Block 2: Return
        //
        // No hook calls, so the scope should remain.
        let mut func = make_func(vec![
            (
                BlockId(0),
                BasicBlock {
                    kind: BlockKind::Block,
                    id: BlockId(0),
                    instructions: vec![],
                    terminal: Terminal::Scope {
                        block: BlockId(1),
                        fallthrough: BlockId(2),
                        scope: make_scope(),
                        id: InstructionId(1),
                        loc: SourceLocation::Generated,
                    },
                    preds: HashSet::new(),
                    phis: vec![],
                },
            ),
            (
                BlockId(1),
                BasicBlock {
                    kind: BlockKind::Block,
                    id: BlockId(1),
                    instructions: vec![Instruction {
                        id: InstructionId(2),
                        lvalue: make_place(1),
                        value: InstructionValue::Primitive {
                            value: PrimitiveValue::Null,
                            loc: SourceLocation::Generated,
                        },
                        loc: SourceLocation::Generated,
                        effects: None,
                    }],
                    terminal: Terminal::Goto {
                        block: BlockId(2),
                        variant: GotoVariant::Break,
                        id: InstructionId(3),
                        loc: SourceLocation::Generated,
                    },
                    preds: HashSet::new(),
                    phis: vec![],
                },
            ),
            (
                BlockId(2),
                BasicBlock {
                    kind: BlockKind::Block,
                    id: BlockId(2),
                    instructions: vec![],
                    terminal: Terminal::Return {
                        value: make_place(1),
                        return_variant: ReturnVariant::Explicit,
                        id: InstructionId(4),
                        loc: SourceLocation::Generated,
                    },
                    preds: HashSet::new(),
                    phis: vec![],
                },
            ),
        ]);

        flatten_scopes_with_hooks_or_use_hir(&mut func);

        // Block 0 should still have Scope terminal.
        assert!(
            matches!(func.body.blocks[0].1.terminal, Terminal::Scope { .. }),
            "Expected Scope to remain, got {:?}",
            std::mem::discriminant(&func.body.blocks[0].1.terminal)
        );
    }

    #[test]
    fn test_use_operator_call_prunes_scope() {
        // Block 0: Scope { block: 1, fallthrough: 2 }
        // Block 1: [CallExpression(use)] + [Primitive] -> Goto -> 2
        // Block 2: Return
        //
        // `use` is the use() operator and should trigger pruning.
        let mut func = make_func(vec![
            (
                BlockId(0),
                BasicBlock {
                    kind: BlockKind::Block,
                    id: BlockId(0),
                    instructions: vec![],
                    terminal: Terminal::Scope {
                        block: BlockId(1),
                        fallthrough: BlockId(2),
                        scope: make_scope(),
                        id: InstructionId(1),
                        loc: SourceLocation::Generated,
                    },
                    preds: HashSet::new(),
                    phis: vec![],
                },
            ),
            (
                BlockId(1),
                BasicBlock {
                    kind: BlockKind::Block,
                    id: BlockId(1),
                    instructions: vec![
                        Instruction {
                            id: InstructionId(2),
                            lvalue: make_place(1),
                            value: InstructionValue::CallExpression {
                                callee: make_named_place(10, "use"),
                                args: vec![],
                                optional: false,
                                loc: SourceLocation::Generated,
                            },
                            loc: SourceLocation::Generated,
                            effects: None,
                        },
                        Instruction {
                            id: InstructionId(3),
                            lvalue: make_place(2),
                            value: InstructionValue::Primitive {
                                value: PrimitiveValue::Null,
                                loc: SourceLocation::Generated,
                            },
                            loc: SourceLocation::Generated,
                            effects: None,
                        },
                    ],
                    terminal: Terminal::Goto {
                        block: BlockId(2),
                        variant: GotoVariant::Break,
                        id: InstructionId(4),
                        loc: SourceLocation::Generated,
                    },
                    preds: HashSet::new(),
                    phis: vec![],
                },
            ),
            (
                BlockId(2),
                BasicBlock {
                    kind: BlockKind::Block,
                    id: BlockId(2),
                    instructions: vec![],
                    terminal: Terminal::Return {
                        value: make_place(1),
                        return_variant: ReturnVariant::Explicit,
                        id: InstructionId(5),
                        loc: SourceLocation::Generated,
                    },
                    preds: HashSet::new(),
                    phis: vec![],
                },
            ),
        ]);

        flatten_scopes_with_hooks_or_use_hir(&mut func);

        // Block 0 should now have PrunedScope terminal.
        assert!(
            matches!(func.body.blocks[0].1.terminal, Terminal::PrunedScope { .. }),
            "Expected PrunedScope for use() call, got {:?}",
            std::mem::discriminant(&func.body.blocks[0].1.terminal)
        );
    }
}
