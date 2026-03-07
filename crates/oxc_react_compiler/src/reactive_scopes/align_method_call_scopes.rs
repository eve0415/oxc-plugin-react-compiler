//! Port of AlignMethodCallScopes.ts.
//!
//! Ensures that method call instructions have scopes such that either:
//! - Both the MethodCall and its property have the same scope
//! - OR neither has a scope
//!
//! Uses a DisjointSet to merge scopes when both the lvalue and the property
//! have scopes, and a mapping to assign/remove scopes when only one side has
//! a scope.
//!
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.

use std::collections::HashMap;

use crate::hir::types::*;
use crate::hir::visitors;

/// A simple disjoint-set (union-find) keyed by ScopeId, mapping to the
/// ReactiveScope value so we can merge ranges.
struct DisjointSet {
    /// Maps a ScopeId to its parent ScopeId.
    parent: HashMap<ScopeId, ScopeId>,
    /// Stores the ReactiveScope data for each scope we've seen.
    scopes: HashMap<ScopeId, ReactiveScope>,
}

impl DisjointSet {
    fn new() -> Self {
        Self {
            parent: HashMap::new(),
            scopes: HashMap::new(),
        }
    }

    fn find(&mut self, id: ScopeId) -> ScopeId {
        if !self.parent.contains_key(&id) {
            return id;
        }
        let parent = self.parent[&id];
        if parent == id {
            return id;
        }
        let root = self.find(parent);
        self.parent.insert(id, root);
        root
    }

    /// Union two scopes together. The first scope's root becomes the canonical root.
    fn union(&mut self, a_scope: &ReactiveScope, b_scope: &ReactiveScope) {
        let a_id = a_scope.id;
        let b_id = b_scope.id;

        // Ensure both are in the set
        self.parent.entry(a_id).or_insert(a_id);
        self.parent.entry(b_id).or_insert(b_id);
        self.scopes.entry(a_id).or_insert_with(|| a_scope.clone());
        self.scopes.entry(b_id).or_insert_with(|| b_scope.clone());

        let root_a = self.find(a_id);
        let root_b = self.find(b_id);
        if root_a != root_b {
            self.parent.insert(root_b, root_a);
        }
    }

    /// Merge ranges: for each scope, update the root's range to encompass
    /// all merged scopes.
    fn merge_ranges(&mut self) {
        let ids: Vec<ScopeId> = self.parent.keys().copied().collect();
        for id in ids {
            let root = self.find(id);
            if id == root {
                continue;
            }
            // Extend root's range with this scope's range
            let scope_range = self.scopes.get(&id).map(|s| s.range.clone());
            if let Some(range) = scope_range
                && let Some(root_scope) = self.scopes.get_mut(&root)
            {
                root_scope.range.start = InstructionId(root_scope.range.start.0.min(range.start.0));
                root_scope.range.end = InstructionId(root_scope.range.end.0.max(range.end.0));
            }
        }
    }

    /// Find the root scope for a given scope id.
    fn find_scope(&mut self, id: ScopeId) -> Option<ReactiveScope> {
        let root = self.find(id);
        self.scopes.get(&root).cloned()
    }
}

/// Align method call scopes so that a MethodCall and its property share the
/// same reactive scope.
pub fn align_method_call_scopes(func: &mut HIRFunction) {
    let debug_align = std::env::var("DEBUG_ALIGN_METHOD_CALL_SCOPES").is_ok();
    let mut scope_mapping: HashMap<IdentifierId, Option<Box<ReactiveScope>>> = HashMap::new();
    let mut merged_scopes = DisjointSet::new();

    // Pass 1: Collect scope relationships from MethodCall instructions.
    for (_block_id, block) in &func.body.blocks {
        for instr in &block.instructions {
            match &instr.value {
                InstructionValue::MethodCall {
                    receiver,
                    property,
                    receiver_optional,
                    call_optional,
                    ..
                } => {
                    let lvalue_scope = &instr.lvalue.identifier.scope;
                    let property_scope = &property.identifier.scope;
                    if debug_align {
                        eprintln!(
                            "[ALIGN_METHOD_SCOPES] instr#{} lvalue_id={} lvalue_scope={:?} property_id={} property_scope={:?}",
                            instr.id.0,
                            instr.lvalue.identifier.id.0,
                            lvalue_scope.as_ref().map(|s| s.id.0),
                            property.identifier.id.0,
                            property_scope.as_ref().map(|s| s.id.0)
                        );
                    }

                    match (lvalue_scope, property_scope) {
                        (Some(lv_scope), Some(prop_scope)) => {
                            // Both have a scope: merge them
                            merged_scopes.union(lv_scope, prop_scope);
                            if debug_align {
                                eprintln!(
                                    "[ALIGN_METHOD_SCOPES] merge scope {} <- {}",
                                    lv_scope.id.0, prop_scope.id.0
                                );
                            }
                        }
                        (Some(lv_scope), None) => {
                            // Call has scope but property doesn't: assign call's scope to property
                            scope_mapping.insert(property.identifier.id, Some(lv_scope.clone()));
                            if debug_align {
                                eprintln!(
                                    "[ALIGN_METHOD_SCOPES] map property_id={} -> scope {}",
                                    property.identifier.id.0, lv_scope.id.0
                                );
                            }
                        }
                        (None, Some(prop_scope)) => {
                            // Flattened optional-call lowering can represent a value-block
                            // boundary as MethodCall + PropertyLoad. Preserve property scope
                            // for optional calls to match upstream behavior.
                            if *receiver_optional || *call_optional {
                                scope_mapping
                                    .insert(property.identifier.id, Some(prop_scope.clone()));
                                if debug_align {
                                    eprintln!(
                                        "[ALIGN_METHOD_SCOPES] keep optional property_id={} scope={}",
                                        property.identifier.id.0, prop_scope.id.0
                                    );
                                }
                            } else {
                                // Property has scope but call doesn't: remove property's scope
                                scope_mapping.insert(property.identifier.id, None);
                                if debug_align {
                                    eprintln!(
                                        "[ALIGN_METHOD_SCOPES] map property_id={} -> <none>",
                                        property.identifier.id.0
                                    );
                                }
                            }
                        }
                        (None, None) => {
                            // Neither has a scope: nothing to do
                        }
                    }
                }
                InstructionValue::FunctionExpression { lowered_func, .. }
                | InstructionValue::ObjectMethod { lowered_func, .. } => {
                    // Recurse into nested functions.
                    // We need mutable access, so we'll handle this in a separate pass.
                    let _ = lowered_func;
                }
                _ => {}
            }
        }
    }

    // Merge the ranges in the disjoint set
    merged_scopes.merge_ranges();

    // Pass 2: Apply scope mappings and merged scopes to all identifier occurrences.
    //
    // Upstream mutates shared Identifier objects, but in Rust HIR places clone
    // Identifier values. We must rewrite every occurrence to keep scopes aligned.
    for (_block_id, block) in &mut func.body.blocks {
        for instr in &mut block.instructions {
            visitors::map_instruction_lvalues(instr, |place| {
                apply_identifier_scope_mapping(
                    &mut place.identifier,
                    &scope_mapping,
                    &mut merged_scopes,
                    debug_align,
                );
            });
            visitors::map_instruction_operands(instr, |place| {
                apply_identifier_scope_mapping(
                    &mut place.identifier,
                    &scope_mapping,
                    &mut merged_scopes,
                    debug_align,
                );
            });
        }
        visitors::map_terminal_operands(&mut block.terminal, |place| {
            apply_identifier_scope_mapping(
                &mut place.identifier,
                &scope_mapping,
                &mut merged_scopes,
                debug_align,
            );
        });
    }

    // Pass 3: Recurse into nested functions.
    for (_block_id, block) in &mut func.body.blocks {
        for instr in &mut block.instructions {
            match &mut instr.value {
                InstructionValue::FunctionExpression { lowered_func, .. }
                | InstructionValue::ObjectMethod { lowered_func, .. } => {
                    align_method_call_scopes(&mut lowered_func.func);
                }
                _ => {}
            }
        }
    }
}

fn apply_identifier_scope_mapping(
    identifier: &mut Identifier,
    scope_mapping: &HashMap<IdentifierId, Option<Box<ReactiveScope>>>,
    merged_scopes: &mut DisjointSet,
    debug_align: bool,
) {
    let ident_id = identifier.id;

    if let Some(mapped_scope) = scope_mapping.get(&ident_id) {
        if debug_align {
            eprintln!(
                "[ALIGN_METHOD_SCOPES] apply identifier_id={} mapped_scope={:?}",
                ident_id.0,
                mapped_scope.as_ref().map(|s| s.id.0)
            );
        }
        identifier.scope = mapped_scope.clone();
    } else if let Some(scope) = &identifier.scope {
        if let Some(merged_scope) = merged_scopes.find_scope(scope.id) {
            identifier.scope = Some(Box::new(merged_scope));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

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

    fn make_place_with_scope(
        ident_id: u32,
        scope_id: u32,
        range_start: u32,
        range_end: u32,
    ) -> Place {
        Place {
            identifier: Identifier {
                id: IdentifierId(ident_id),
                declaration_id: DeclarationId(ident_id),
                name: None,
                mutable_range: MutableRange::default(),
                scope: Some(Box::new(ReactiveScope {
                    id: ScopeId(scope_id),
                    range: MutableRange {
                        start: InstructionId(range_start),
                        end: InstructionId(range_end),
                    },
                    dependencies: vec![],
                    declarations: Default::default(),
                    reassignments: vec![],
                    merged_id: None,
                    early_return_value: None,
                })),
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

    #[test]
    fn test_both_have_scope_merges() {
        // MethodCall where lvalue has scope 1 and property has scope 2
        // After alignment, both should share the same scope with merged range.
        let mut func = make_func(vec![(
            BlockId(0),
            BasicBlock {
                kind: BlockKind::Block,
                id: BlockId(0),
                instructions: vec![Instruction {
                    id: InstructionId(1),
                    lvalue: make_place_with_scope(1, 1, 1, 5),
                    value: InstructionValue::MethodCall {
                        receiver: make_place(10),
                        property: make_place_with_scope(2, 2, 2, 4),
                        args: vec![],
                        receiver_optional: false,
                        call_optional: false,
                        loc: SourceLocation::Generated,
                    },
                    loc: SourceLocation::Generated,
                    effects: None,
                }],
                terminal: Terminal::Return {
                    value: make_place(1),
                    return_variant: ReturnVariant::Explicit,
                    id: InstructionId(10),
                    loc: SourceLocation::Generated,
                },
                preds: HashSet::new(),
                phis: vec![],
            },
        )]);

        align_method_call_scopes(&mut func);

        let lvalue_scope = func.body.blocks[0].1.instructions[0]
            .lvalue
            .identifier
            .scope
            .as_ref()
            .unwrap();
        // Merged range should be min(1,2)..max(5,4) = 1..5
        assert_eq!(lvalue_scope.range.start.0, 1);
        assert_eq!(lvalue_scope.range.end.0, 5);
    }

    #[test]
    fn test_lvalue_has_scope_property_does_not() {
        // MethodCall where lvalue has scope but property does not.
        // The property's identifier should get the lvalue's scope.
        let mut func = make_func(vec![(
            BlockId(0),
            BasicBlock {
                kind: BlockKind::Block,
                id: BlockId(0),
                instructions: vec![
                    // Property instruction (its lvalue id matches the property id in MethodCall)
                    Instruction {
                        id: InstructionId(1),
                        lvalue: make_place(2),
                        value: InstructionValue::Primitive {
                            value: PrimitiveValue::Undefined,
                            loc: SourceLocation::Generated,
                        },
                        loc: SourceLocation::Generated,
                        effects: None,
                    },
                    Instruction {
                        id: InstructionId(2),
                        lvalue: make_place_with_scope(1, 1, 1, 5),
                        value: InstructionValue::MethodCall {
                            receiver: make_place(10),
                            property: make_place(2),
                            args: vec![],
                            receiver_optional: false,
                            call_optional: false,
                            loc: SourceLocation::Generated,
                        },
                        loc: SourceLocation::Generated,
                        effects: None,
                    },
                ],
                terminal: Terminal::Return {
                    value: make_place(1),
                    return_variant: ReturnVariant::Explicit,
                    id: InstructionId(10),
                    loc: SourceLocation::Generated,
                },
                preds: HashSet::new(),
                phis: vec![],
            },
        )]);

        align_method_call_scopes(&mut func);

        // The property instruction's lvalue (id=2) should now have scope 1
        let prop_scope = func.body.blocks[0].1.instructions[0]
            .lvalue
            .identifier
            .scope
            .as_ref();
        assert!(prop_scope.is_some());
        assert_eq!(prop_scope.unwrap().id, ScopeId(1));
    }

    #[test]
    fn test_no_method_calls_is_noop() {
        let mut func = make_func(vec![(
            BlockId(0),
            BasicBlock {
                kind: BlockKind::Block,
                id: BlockId(0),
                instructions: vec![Instruction {
                    id: InstructionId(1),
                    lvalue: make_place(1),
                    value: InstructionValue::Primitive {
                        value: PrimitiveValue::Undefined,
                        loc: SourceLocation::Generated,
                    },
                    loc: SourceLocation::Generated,
                    effects: None,
                }],
                terminal: Terminal::Return {
                    value: make_place(1),
                    return_variant: ReturnVariant::Explicit,
                    id: InstructionId(2),
                    loc: SourceLocation::Generated,
                },
                preds: HashSet::new(),
                phis: vec![],
            },
        )]);

        // Should not panic
        align_method_call_scopes(&mut func);
    }
}
