//! Port of AlignObjectMethodScopes.ts.
//!
//! Align scopes of object method values to that of their enclosing object
//! expressions. To produce a well-formed JS program in Codegen, object methods
//! and object expressions must be in the same ReactiveBlock as object method
//! definitions must be inlined.
//!
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.

use std::collections::{HashMap, HashSet};

use crate::hir::types::*;

/// A disjoint-set (union-find) keyed by ScopeId, storing ReactiveScope data
/// so we can merge ranges after finding connected components.
struct DisjointSet {
    parent: HashMap<ScopeId, ScopeId>,
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

    fn union(&mut self, a_scope: &ReactiveScope, b_scope: &ReactiveScope) {
        let a_id = a_scope.id;
        let b_id = b_scope.id;

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

    /// Canonicalize: returns a map from each ScopeId to its root ScopeId.
    fn canonicalize(&mut self) -> HashMap<ScopeId, ScopeId> {
        let ids: Vec<ScopeId> = self.parent.keys().copied().collect();
        let mut result = HashMap::new();
        for id in ids {
            let root = self.find(id);
            result.insert(id, root);
        }
        result
    }

    fn get_scope(&self, id: ScopeId) -> Option<&ReactiveScope> {
        self.scopes.get(&id)
    }

    fn get_scope_mut(&mut self, id: ScopeId) -> Option<&mut ReactiveScope> {
        self.scopes.get_mut(&id)
    }
}

/// Find scopes that need to be merged: object method declarations that appear
/// as operands of an ObjectExpression should share the same scope as the
/// ObjectExpression's lvalue.
fn find_scopes_to_merge(func: &HIRFunction) -> DisjointSet {
    // Collect identifiers that are ObjectMethod lvalues.
    let mut object_method_decls: HashSet<IdentifierId> = HashSet::new();
    let mut ds = DisjointSet::new();

    for (_block_id, block) in &func.body.blocks {
        for instr in &block.instructions {
            match &instr.value {
                InstructionValue::ObjectMethod { .. } => {
                    object_method_decls.insert(instr.lvalue.identifier.id);
                }
                InstructionValue::ObjectExpression { properties, .. } => {
                    // Check each operand of the ObjectExpression
                    for prop in properties {
                        let operand = match prop {
                            ObjectPropertyOrSpread::Property(p) => &p.place,
                            ObjectPropertyOrSpread::Spread(p) => p,
                        };

                        if object_method_decls.contains(&operand.identifier.id) {
                            let operand_scope = &operand.identifier.scope;
                            let lvalue_scope = &instr.lvalue.identifier.scope;

                            // Upstream invariant: both should be non-null
                            if let (Some(op_scope), Some(lv_scope)) = (operand_scope, lvalue_scope)
                            {
                                ds.union(op_scope, lv_scope);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }

    ds
}

/// Align object method scopes to their enclosing object expression scopes.
pub fn align_object_method_scopes(func: &mut HIRFunction) {
    // First, recurse into nested functions (scopes are disjoint across functions).
    for (_block_id, block) in &mut func.body.blocks {
        for instr in &mut block.instructions {
            match &mut instr.value {
                InstructionValue::ObjectMethod { lowered_func, .. }
                | InstructionValue::FunctionExpression { lowered_func, .. } => {
                    align_object_method_scopes(&mut lowered_func.func);
                }
                _ => {}
            }
        }
    }

    let mut merge_set = find_scopes_to_merge(func);
    let scope_groups_map = merge_set.canonicalize();

    // Step 1: Merge affected scopes' ranges to their canonical root.
    for (&scope_id, &root_id) in &scope_groups_map {
        if scope_id != root_id {
            let scope_range = merge_set.get_scope(scope_id).map(|s| s.range.clone());
            if let Some(range) = scope_range
                && let Some(root_scope) = merge_set.get_scope_mut(root_id)
            {
                root_scope.range.start = InstructionId(root_scope.range.start.0.min(range.start.0));
                root_scope.range.end = InstructionId(root_scope.range.end.0.max(range.end.0));
            }
        }
    }

    // Step 2: Repoint identifiers whose scopes were merged.
    for (_block_id, block) in &mut func.body.blocks {
        for instr in &mut block.instructions {
            if let Some(scope) = &instr.lvalue.identifier.scope
                && let Some(&root_id) = scope_groups_map.get(&scope.id)
                && let Some(root_scope) = merge_set.get_scope(root_id)
            {
                instr.lvalue.identifier.scope = Some(Box::new(root_scope.clone()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn test_object_method_scope_aligned_to_object() {
        // Instruction 1: ObjectMethod lvalue id=1 scope=1 range[1,3)
        // Instruction 2: ObjectExpression lvalue id=2 scope=2 range[2,5)
        //   with property referencing id=1
        // After alignment, scope 1 and scope 2 should be merged.
        let mut func = make_func(vec![(
            BlockId(0),
            BasicBlock {
                kind: BlockKind::Block,
                id: BlockId(0),
                instructions: vec![
                    Instruction {
                        id: InstructionId(1),
                        lvalue: make_place_with_scope(1, 1, 1, 3),
                        value: InstructionValue::ObjectMethod {
                            lowered_func: LoweredFunction {
                                func: make_func(vec![(
                                    BlockId(0),
                                    BasicBlock {
                                        kind: BlockKind::Block,
                                        id: BlockId(0),
                                        instructions: vec![],
                                        terminal: Terminal::Return {
                                            value: make_place(50),
                                            return_variant: ReturnVariant::Explicit,
                                            id: InstructionId(51),
                                            loc: SourceLocation::Generated,
                                        },
                                        preds: HashSet::new(),
                                        phis: vec![],
                                    },
                                )]),
                            },
                            loc: SourceLocation::Generated,
                        },
                        loc: SourceLocation::Generated,
                        effects: None,
                    },
                    Instruction {
                        id: InstructionId(2),
                        lvalue: make_place_with_scope(2, 2, 2, 5),
                        value: InstructionValue::ObjectExpression {
                            properties: vec![ObjectPropertyOrSpread::Property(ObjectProperty {
                                key: ObjectPropertyKey::Identifier("method".to_string()),
                                type_: ObjectPropertyType::Method,
                                place: make_place_with_scope(1, 1, 1, 3),
                            })],
                            loc: SourceLocation::Generated,
                        },
                        loc: SourceLocation::Generated,
                        effects: None,
                    },
                ],
                terminal: Terminal::Return {
                    value: make_place(2),
                    return_variant: ReturnVariant::Explicit,
                    id: InstructionId(10),
                    loc: SourceLocation::Generated,
                },
                preds: HashSet::new(),
                phis: vec![],
            },
        )]);

        align_object_method_scopes(&mut func);

        // Both instructions should now share the same scope id
        let scope_1 = func.body.blocks[0].1.instructions[0]
            .lvalue
            .identifier
            .scope
            .as_ref()
            .unwrap();
        let scope_2 = func.body.blocks[0].1.instructions[1]
            .lvalue
            .identifier
            .scope
            .as_ref()
            .unwrap();
        assert_eq!(scope_1.id, scope_2.id);
        // The merged range should encompass both: min(1,2)..max(3,5) = 1..5
        assert_eq!(scope_1.range.start.0, 1);
        assert_eq!(scope_1.range.end.0, 5);
    }

    #[test]
    fn test_no_object_methods_is_noop() {
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
        align_object_method_scopes(&mut func);
    }
}
