//! Collect optional chain dependencies from the HIR CFG.
//!
//! Port of `CollectOptionalChainDependencies.ts` from upstream React Compiler
//! (babel-plugin-react-compiler v1.0.0).
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.
//!
//! This module traverses the HIR CFG to find optional chaining patterns (a?.b?.c)
//! and builds three outputs:
//! 1. `temporaries_read_in_optional` - Maps identifier IDs to their optional chain dependencies
//! 2. `processed_instrs_in_optional` - Set of instruction/terminal IDs to skip in main dep collection
//! 3. `hoistable_objects` - Maps block IDs to safe-to-evaluate objects from optional chains

use std::collections::{HashMap, HashSet};

use crate::hir::types::*;

// ---------------------------------------------------------------------------
// Output type
// ---------------------------------------------------------------------------

/// Sidemap produced by collecting optional chain dependencies.
pub struct OptionalChainSidemap {
    /// Stores the correct property mapping (e.g. `a?.b` instead of `a.b`) for
    /// dependency calculation. Note that we currently do not store anything on
    /// outer phi nodes.
    pub temporaries_read_in_optional: HashMap<IdentifierId, ReactiveScopeDependency>,

    /// Records instructions (PropertyLoads, StoreLocals, and test terminals)
    /// processed in this pass. When extracting dependencies in
    /// PropagateScopeDependencies, these instructions are skipped.
    ///
    pub processed_instrs_in_optional: HashSet<ProcessedOptionalNode>,

    /// Records optional chains for which we can safely evaluate non-optional
    /// PropertyLoads. e.g. given `a?.b.c`, we can evaluate any load from `a?.b`
    /// at the optional terminal in bb1.
    pub hoistable_objects: HashMap<BlockId, ReactiveScopeDependency>,
}

/// Distinguishes deferred optional-chain nodes by function + node id.
///
/// Upstream uses object identity (`Instruction | Terminal`) as set keys, but
/// Rust ports cannot rely on pointer identity across rebuilt clones. Encoding
/// function identity avoids collisions from reused `InstructionId`s in nested
/// lowered functions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProcessedOptionalNode {
    pub function_key: usize,
    pub id: InstructionId,
    pub kind: ProcessedOptionalNodeKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProcessedOptionalNodeKind {
    Instruction,
    Terminal,
}

// ---------------------------------------------------------------------------
// Internal traversal context
// ---------------------------------------------------------------------------

struct OptionalTraversalContext<'a> {
    /// Blocks of the current function being traversed.
    blocks: &'a [(BlockId, BasicBlock)],
    current_fn_key: usize,

    /// Track optional blocks to avoid outer calls into nested optionals.
    seen_optionals: HashSet<BlockId>,

    processed_instrs_in_optional: HashSet<ProcessedOptionalNode>,
    temporaries_read_in_optional: HashMap<IdentifierId, ReactiveScopeDependency>,
    hoistable_objects: HashMap<BlockId, ReactiveScopeDependency>,
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Collect optional chain sidemaps from an HIR function.
///
/// This traverses the function (and nested function expressions) to find
/// optional chaining patterns and produce sidemaps used by dependency
/// collection to properly represent optional chains as dependencies.
pub fn collect_optional_chain_sidemap(func: &HIRFunction) -> OptionalChainSidemap {
    let mut context = OptionalTraversalContext {
        blocks: &func.body.blocks,
        current_fn_key: func as *const HIRFunction as usize,
        seen_optionals: HashSet::new(),
        processed_instrs_in_optional: HashSet::new(),
        temporaries_read_in_optional: HashMap::new(),
        hoistable_objects: HashMap::new(),
    };
    traverse_function(func, &mut context);
    OptionalChainSidemap {
        temporaries_read_in_optional: context.temporaries_read_in_optional,
        processed_instrs_in_optional: context.processed_instrs_in_optional,
        hoistable_objects: context.hoistable_objects,
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Look up a block by ID in a blocks slice.
fn get_block(blocks: &[(BlockId, BasicBlock)], id: BlockId) -> &BasicBlock {
    blocks
        .iter()
        .find(|(bid, _)| *bid == id)
        .map(|(_, b)| b)
        .unwrap_or_else(|| panic!("[OptionalChainDeps] Block {:?} not found", id))
}

/// Convert a `PropertyLiteral` to the string representation used in
/// `DependencyPathEntry`.
fn property_literal_to_string(prop: &PropertyLiteral) -> String {
    match prop {
        PropertyLiteral::String(s) => s.clone(),
        PropertyLiteral::Number(n) => n.to_string(),
    }
}

fn primitive_to_property_string(value: &PrimitiveValue) -> Option<String> {
    match value {
        PrimitiveValue::String(s) => Some(s.clone()),
        PrimitiveValue::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Traversal
// ---------------------------------------------------------------------------

/// Traverse a function and all nested function expressions to collect
/// optional chain sidemaps.
fn traverse_function<'a>(func: &'a HIRFunction, context: &mut OptionalTraversalContext<'a>) {
    for (_block_id, block) in &func.body.blocks {
        // Recurse into nested function expressions / object methods.
        for instr in &block.instructions {
            match &instr.value {
                InstructionValue::FunctionExpression { lowered_func, .. }
                | InstructionValue::ObjectMethod { lowered_func, .. } => {
                    let saved_blocks = context.blocks;
                    let saved_fn_key = context.current_fn_key;
                    context.blocks = &lowered_func.func.body.blocks;
                    context.current_fn_key = &lowered_func.func as *const HIRFunction as usize;
                    traverse_function(&lowered_func.func, context);
                    context.blocks = saved_blocks;
                    context.current_fn_key = saved_fn_key;
                }
                _ => {}
            }
        }

        // If this block has an Optional terminal that we haven't already
        // processed as part of an outer optional chain, start traversal here.
        if let Terminal::Optional { .. } = &block.terminal
            && !context.seen_optionals.contains(&block.id)
        {
            traverse_optional_block(block, context, None);
        }
    }
}

/// Result of matching an optional test block's consequent.
struct MatchConsequentResult {
    /// The IdentifierId of the StoreLocal's lvalue (the value visible after the
    /// optional chain completes).
    consequent_id: IdentifierId,
    /// The property being loaded.
    property: PropertyLiteral,
    /// The IdentifierId of the PropertyLoad's lvalue.
    property_id: IdentifierId,
    /// The InstructionId of the PropertyLoad/ComputedLoad instruction.
    property_load_instr_id: InstructionId,
    /// The InstructionId of the StoreLocal instruction (to record as processed).
    store_local_instr_id: InstructionId,
    /// The goto target of the consequent block.
    consequent_goto: BlockId,
}

/// Match the consequent and alternate blocks of a Branch terminal inside an
/// optional chain.
///
/// Returns the property load computed by the consequent block, or `None` if the
/// consequent block is not a simple PropertyLoad + StoreLocal sequence.
fn match_optional_test_block(
    // Branch terminal fields
    test_place: &Place,
    consequent_block_id: BlockId,
    alternate_block_id: BlockId,
    _terminal_loc: &SourceLocation,
    blocks: &[(BlockId, BasicBlock)],
) -> Option<MatchConsequentResult> {
    let consequent_block = get_block(blocks, consequent_block_id);

    if consequent_block.instructions.len() == 2
        && let (
            InstructionValue::PropertyLoad {
                object, property, ..
            },
            InstructionValue::StoreLocal { lvalue, value, .. },
        ) = (
            &consequent_block.instructions[0].value,
            &consequent_block.instructions[1].value,
        )
    {
        // Invariant: PropertyLoad's object must match the Branch test
        assert_eq!(
            object.identifier.id, test_place.identifier.id,
            "[OptionalChainDeps] Inconsistent optional chaining property load: \
                 Test={:?} PropertyLoad base={:?}",
            test_place.identifier.id, object.identifier.id
        );

        // Invariant: StoreLocal's value must match PropertyLoad's lvalue
        assert_eq!(
            value.identifier.id, consequent_block.instructions[0].lvalue.identifier.id,
            "[OptionalChainDeps] Unexpected storeLocal"
        );

        // The consequent block must end with a Goto(Break)
        if let Terminal::Goto {
            variant: GotoVariant::Break,
            block: goto_target,
            ..
        } = &consequent_block.terminal
        {
            // Validate the alternate block structure
            let alternate = get_block(blocks, alternate_block_id);
            assert!(
                alternate.instructions.len() == 2
                    && matches!(
                        &alternate.instructions[0].value,
                        InstructionValue::Primitive { .. }
                    )
                    && matches!(
                        &alternate.instructions[1].value,
                        InstructionValue::StoreLocal { .. }
                    ),
                "[OptionalChainDeps] Unexpected alternate structure"
            );

            return Some(MatchConsequentResult {
                consequent_id: lvalue.place.identifier.id,
                property: property.clone(),
                property_id: consequent_block.instructions[0].lvalue.identifier.id,
                property_load_instr_id: consequent_block.instructions[0].id,
                store_local_instr_id: consequent_block.instructions[1].id,
                consequent_goto: *goto_target,
            });
        }

        return None;
    }

    None
}

/// Traverse into an optional block and all transitively referenced blocks to
/// collect sidemaps of optional chain dependencies.
///
/// Returns the `IdentifierId` representing the optional block if the block and
/// all transitively referenced optional blocks precisely represent a chain of
/// property loads. If any part of the optional chain is not hoistable, returns
/// `None`.
fn traverse_optional_block(
    optional: &BasicBlock,
    context: &mut OptionalTraversalContext,
    outer_alternate: Option<BlockId>,
) -> Option<IdentifierId> {
    let debug_trace = std::env::var("DEBUG_OPTIONAL_SIDEMAP_TRACE").is_ok();
    context.seen_optionals.insert(optional.id);

    // Extract the Optional terminal fields
    let (opt_optional, opt_test_block_id, opt_fallthrough, opt_loc) = match &optional.terminal {
        Terminal::Optional {
            optional,
            test,
            fallthrough,
            loc,
            ..
        } => (*optional, *test, *fallthrough, loc),
        _ => panic!(
            "[OptionalChainDeps] Expected Optional terminal, got {:?}",
            optional.terminal
        ),
    };

    let maybe_test = get_block(context.blocks, opt_test_block_id);
    if debug_trace {
        eprintln!(
            "[OPTIONAL_TRACE] enter optional_block={} opt_optional={} test_block={} fallthrough={} outer_alternate={:?} test_term={:?}",
            optional.id.0,
            opt_optional,
            opt_test_block_id.0,
            opt_fallthrough.0,
            outer_alternate.map(|b| b.0),
            maybe_test.terminal
        );
        for instr in &maybe_test.instructions {
            eprintln!(
                "[OPTIONAL_TRACE]   test_instr id={} lvalue={} value={:?}",
                instr.id.0, instr.lvalue.identifier.id.0, instr.value
            );
        }
    }

    let test_place: &Place;
    let test_consequent: BlockId;
    let test_alternate: BlockId;
    let test_terminal_id: InstructionId;
    let _test_loc: &SourceLocation;
    let base_object: ReactiveScopeDependency;

    match &maybe_test.terminal {
        Terminal::Branch {
            id,
            test,
            consequent,
            alternate,
            loc,
            ..
        } => {
            // Base case: the test block directly has a Branch terminal.
            // This means it's the innermost optional in the chain.
            assert!(
                opt_optional,
                "[OptionalChainDeps] Expect base case to be always optional"
            );

            // Only match base expressions that are straightforward PropertyLoad chains.
            if maybe_test.instructions.is_empty() {
                return None;
            }
            if !matches!(
                &maybe_test.instructions[0].value,
                InstructionValue::LoadLocal { .. } | InstructionValue::LoadContext { .. }
            ) {
                return None;
            }

            let mut path: Vec<DependencyPathEntry> = Vec::new();
            let mut prev_lvalue = maybe_test.instructions[0].lvalue.identifier.id;
            let mut idx = 1usize;
            while idx < maybe_test.instructions.len() {
                match &maybe_test.instructions[idx].value {
                    InstructionValue::PropertyLoad {
                        object,
                        property,
                        optional: load_optional,
                        ..
                    } => {
                        if object.identifier.id != prev_lvalue {
                            return None;
                        }
                        path.push(DependencyPathEntry {
                            property: property_literal_to_string(property),
                            // OXC lowering preserves `optional` on base test
                            // PropertyLoad instructions for chains like
                            // `a?.b.c?.d`; keep that token so downstream deps
                            // retain source-accurate optional markers.
                            optional: *load_optional,
                        });
                        if debug_trace {
                            eprintln!(
                                "[OPTIONAL_TRACE]   base PropertyLoad object={} property={} stored_optional={}",
                                object.identifier.id.0,
                                property_literal_to_string(property),
                                load_optional
                            );
                        }
                        context
                            .processed_instrs_in_optional
                            .insert(ProcessedOptionalNode {
                                function_key: context.current_fn_key,
                                id: maybe_test.instructions[idx].id,
                                kind: ProcessedOptionalNodeKind::Instruction,
                            });
                        prev_lvalue = maybe_test.instructions[idx].lvalue.identifier.id;
                        idx += 1;
                    }
                    InstructionValue::ComputedLoad {
                        object,
                        property,
                        optional: load_optional,
                        ..
                    } => {
                        if object.identifier.id != prev_lvalue {
                            return None;
                        }
                        let property_name = maybe_test
                            .instructions
                            .iter()
                            .find(|instr| instr.lvalue.identifier.id == property.identifier.id)
                            .and_then(|instr| match &instr.value {
                                InstructionValue::Primitive { value, .. } => {
                                    primitive_to_property_string(value)
                                }
                                _ => None,
                            });
                        let Some(property_name) = property_name else {
                            return None;
                        };
                        path.push(DependencyPathEntry {
                            property: property_name,
                            // See PropertyLoad handling above.
                            optional: *load_optional,
                        });
                        if debug_trace {
                            eprintln!(
                                "[OPTIONAL_TRACE]   base ComputedLoad object={} property={} stored_optional={}",
                                object.identifier.id.0,
                                path.last().map(|p| p.property.as_str()).unwrap_or(""),
                                load_optional
                            );
                        }
                        context
                            .processed_instrs_in_optional
                            .insert(ProcessedOptionalNode {
                                function_key: context.current_fn_key,
                                id: maybe_test.instructions[idx].id,
                                kind: ProcessedOptionalNodeKind::Instruction,
                            });
                        prev_lvalue = maybe_test.instructions[idx].lvalue.identifier.id;
                        idx += 1;
                    }
                    InstructionValue::Primitive { .. } => {
                        // Primitive keys consumed by a following ComputedLoad are
                        // handled by that ComputedLoad branch.
                        idx += 1;
                    }
                    _ => return None,
                }
            }

            // Invariant: the branch test must be the last instruction's lvalue
            let last_instr = maybe_test.instructions.last().unwrap();
            assert_eq!(
                test.identifier.id, last_instr.lvalue.identifier.id,
                "[OptionalChainDeps] Unexpected test expression"
            );

            // Extract the base identifier from the initial load.
            let base_place = match &maybe_test.instructions[0].value {
                InstructionValue::LoadLocal { place, .. }
                | InstructionValue::LoadContext { place, .. } => place,
                _ => unreachable!(),
            };

            base_object = ReactiveScopeDependency {
                identifier: base_place.identifier.clone(),
                path,
            };

            test_place = test;
            test_consequent = *consequent;
            test_alternate = *alternate;
            test_terminal_id = *id;
            _test_loc = loc;
        }

        Terminal::Optional {
            fallthrough: inner_fallthrough,
            loc: _inner_loc,
            ..
        } => {
            // Nested optional: the test block itself has an Optional terminal,
            // meaning this is either:
            // - <inner_optional>?.property (optional=true)
            // - <inner_optional>.property  (optional=false)
            // - <inner_optional> <other operation>
            // - an optional base block with a separate nested optional-chain
            let test_block = get_block(context.blocks, *inner_fallthrough);
            match &test_block.terminal {
                Terminal::Branch {
                    id: tb_id,
                    test: tb_test,
                    consequent: tb_consequent,
                    alternate: tb_alternate,
                    loc: tb_loc,
                    ..
                } => {
                    // Recurse into the inner optional block to collect inner
                    // optional-chain expressions.
                    let inner_optional =
                        traverse_optional_block(maybe_test, context, Some(*tb_alternate));

                    let inner_optional = match inner_optional {
                        Some(id) => id,
                        None => return None,
                    };
                    if debug_trace {
                        eprintln!(
                            "[OPTIONAL_TRACE] nested optional_block={} got inner_optional={} tb_test={} tb_conseq={} tb_alt={}",
                            optional.id.0,
                            inner_optional.0,
                            tb_test.identifier.id.0,
                            tb_consequent.0,
                            tb_alternate.0
                        );
                    }

                    // Check that the inner optional is part of the same
                    // optional-chain as the outer one.
                    if tb_test.identifier.id != inner_optional {
                        return None;
                    }

                    if !opt_optional {
                        // If this is a non-optional load participating in an
                        // optional chain (e.g. loading `c` in `a?.b.c`), record
                        // that PropertyLoads from the inner optional value are
                        // hoistable.
                        let dep = context
                            .temporaries_read_in_optional
                            .get(&inner_optional)
                            .unwrap_or_else(|| {
                                panic!(
                                    "[OptionalChainDeps] Expected temporary for {:?}",
                                    inner_optional
                                )
                            })
                            .clone();
                        context.hoistable_objects.insert(optional.id, dep);
                    }

                    base_object = context
                        .temporaries_read_in_optional
                        .get(&inner_optional)
                        .unwrap_or_else(|| {
                            panic!(
                                "[OptionalChainDeps] Expected temporary for {:?}",
                                inner_optional
                            )
                        })
                        .clone();

                    test_place = tb_test;
                    test_consequent = *tb_consequent;
                    test_alternate = *tb_alternate;
                    test_terminal_id = *tb_id;
                    _test_loc = tb_loc;
                }
                other => {
                    panic!(
                        "[OptionalChainDeps] Unexpected terminal kind `{:?}` \
                         for optional fallthrough block",
                        std::mem::discriminant(other)
                    );
                }
            }
        }

        _ => {
            // Not a Branch or Optional terminal in the test block -- bail out.
            return None;
        }
    }

    // Validate: if this optional's alternate matches the outer alternate, the
    // optional block must have no instructions (otherwise two unrelated
    // optional chains may have been incorrectly concatenated).
    if let Some(outer_alt) = outer_alternate
        && test_alternate == outer_alt
    {
        assert!(
            optional.instructions.is_empty(),
            "[OptionalChainDeps] Unexpected instructions in an inner optional block. \
                 This indicates that the compiler may be incorrectly concatenating \
                 two unrelated optional chains"
        );
    }

    // Try to match the consequent block of the branch as a simple
    // PropertyLoad + StoreLocal.
    let match_result = match_optional_test_block(
        test_place,
        test_consequent,
        test_alternate,
        opt_loc,
        context.blocks,
    );

    let match_result = match match_result {
        Some(r) => r,
        None => {
            // Optional chain consequent is not hoistable, e.g. a?.[computed()]
            return None;
        }
    };

    // Invariant: consequent goto must equal the optional terminal's fallthrough
    assert_eq!(
        match_result.consequent_goto, opt_fallthrough,
        "[OptionalChainDeps] Unexpected optional goto-fallthrough: {:?} != {:?}",
        match_result.consequent_goto, opt_fallthrough
    );

    // Build the dependency for this level of the optional chain.
    let mut path = base_object.path.clone();
    path.push(DependencyPathEntry {
        property: property_literal_to_string(&match_result.property),
        optional: opt_optional,
    });
    if debug_trace {
        eprintln!(
            "[OPTIONAL_TRACE] emit optional_block={} consequent_id={} property={} opt_optional={} final_path={}",
            optional.id.0,
            match_result.consequent_id.0,
            property_literal_to_string(&match_result.property),
            opt_optional,
            path.iter()
                .map(|p| format!("{}{}", p.property, if p.optional { "?" } else { "" }))
                .collect::<Vec<_>>()
                .join(".")
        );
    }
    let load = ReactiveScopeDependency {
        identifier: base_object.identifier.clone(),
        path,
    };

    // Record optional-chain load + StoreLocal instructions and branch terminal as processed.
    context
        .processed_instrs_in_optional
        .insert(ProcessedOptionalNode {
            function_key: context.current_fn_key,
            id: match_result.property_load_instr_id,
            kind: ProcessedOptionalNodeKind::Instruction,
        });
    context
        .processed_instrs_in_optional
        .insert(ProcessedOptionalNode {
            function_key: context.current_fn_key,
            id: match_result.store_local_instr_id,
            kind: ProcessedOptionalNodeKind::Instruction,
        });

    // Record the exact branch terminal we matched for this optional step.
    context
        .processed_instrs_in_optional
        .insert(ProcessedOptionalNode {
            function_key: context.current_fn_key,
            id: test_terminal_id,
            kind: ProcessedOptionalNodeKind::Terminal,
        });

    // Record the dependency for both the consequent ID and property ID.
    context
        .temporaries_read_in_optional
        .insert(match_result.consequent_id, load.clone());
    context
        .temporaries_read_in_optional
        .insert(match_result.property_id, load);

    Some(match_result.consequent_id)
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn make_identifier(id: u32, name: Option<&str>) -> Identifier {
        Identifier {
            id: IdentifierId(id),
            declaration_id: DeclarationId(id),
            name: name.map(|n| IdentifierName::Named(n.to_string())),
            mutable_range: MutableRange::default(),
            scope: None,
            type_: Type::Poly,
            loc: SourceLocation::Generated,
        }
    }

    fn make_place(id: u32, name: Option<&str>) -> Place {
        Place {
            identifier: make_identifier(id, name),
            effect: Effect::Unknown,
            reactive: false,
            loc: SourceLocation::Generated,
        }
    }

    fn make_empty_func() -> HIRFunction {
        HIRFunction {
            env: crate::environment::Environment::new(crate::options::EnvironmentConfig::default()),
            loc: SourceLocation::Generated,
            id: None,
            fn_type: ReactFunctionType::Component,
            params: vec![],
            returns: make_place(99, None),
            context: vec![],
            body: HIR {
                entry: BlockId(0),
                blocks: vec![],
            },
            generator: false,
            async_: false,
            directives: vec![],
            aliasing_effects: None,
        }
    }

    #[test]
    fn test_empty_function_returns_empty_sidemap() {
        let func = make_empty_func();
        let result = collect_optional_chain_sidemap(&func);
        let function_key = &func as *const HIRFunction as usize;
        assert!(result.temporaries_read_in_optional.is_empty());
        assert!(result.processed_instrs_in_optional.is_empty());
        assert!(result.hoistable_objects.is_empty());
    }

    #[test]
    fn test_property_literal_to_string() {
        assert_eq!(
            property_literal_to_string(&PropertyLiteral::String("foo".to_string())),
            "foo"
        );
        assert_eq!(
            property_literal_to_string(&PropertyLiteral::Number(42.0)),
            "42"
        );
    }

    /// Build an HIR representing `a?.b`:
    ///
    /// bb0: Optional optional=true test=bb1 fallthrough=bb4
    /// bb1:
    ///   $0 = LoadLocal 'a'
    ///   Branch test=$0 consequent=bb2 alternate=bb3
    /// bb2:
    ///   $1 = PropertyLoad $0.'b'
    ///   StoreLocal $2 = $1
    ///   Goto(Break) -> bb4
    /// bb3:
    ///   $3 = Primitive(undefined)
    ///   StoreLocal $4 = $3
    ///   Goto(Break) -> bb4
    /// bb4:
    ///   phi($2, $4)
    ///   Return $5
    #[test]
    fn test_simple_optional_chain() {
        let blocks = vec![
            (
                BlockId(0),
                BasicBlock {
                    kind: BlockKind::Block,
                    id: BlockId(0),
                    instructions: vec![],
                    terminal: Terminal::Optional {
                        optional: true,
                        test: BlockId(1),
                        fallthrough: BlockId(4),
                        id: InstructionId(0),
                        loc: SourceLocation::Generated,
                    },
                    preds: HashSet::new(),
                    phis: vec![],
                },
            ),
            (
                BlockId(1),
                BasicBlock {
                    kind: BlockKind::Value,
                    id: BlockId(1),
                    instructions: vec![Instruction {
                        id: InstructionId(1),
                        lvalue: make_place(0, None),
                        value: InstructionValue::LoadLocal {
                            place: make_place(100, Some("a")),
                            loc: SourceLocation::Generated,
                        },
                        loc: SourceLocation::Generated,
                        effects: None,
                    }],
                    terminal: Terminal::Branch {
                        test: make_place(0, None),
                        consequent: BlockId(2),
                        alternate: BlockId(3),
                        fallthrough: BlockId(4),
                        id: InstructionId(2),
                        loc: SourceLocation::Generated,
                    },
                    preds: HashSet::new(),
                    phis: vec![],
                },
            ),
            (
                BlockId(2),
                BasicBlock {
                    kind: BlockKind::Value,
                    id: BlockId(2),
                    instructions: vec![
                        Instruction {
                            id: InstructionId(3),
                            lvalue: make_place(1, None),
                            value: InstructionValue::PropertyLoad {
                                object: make_place(0, None),
                                property: PropertyLiteral::String("b".to_string()),
                                optional: false,
                                loc: SourceLocation::Generated,
                            },
                            loc: SourceLocation::Generated,
                            effects: None,
                        },
                        Instruction {
                            id: InstructionId(4),
                            lvalue: make_place(2, None),
                            value: InstructionValue::StoreLocal {
                                lvalue: LValue {
                                    place: make_place(2, None),
                                    kind: InstructionKind::Const,
                                },
                                value: make_place(1, None),
                                loc: SourceLocation::Generated,
                            },
                            loc: SourceLocation::Generated,
                            effects: None,
                        },
                    ],
                    terminal: Terminal::Goto {
                        block: BlockId(4),
                        variant: GotoVariant::Break,
                        id: InstructionId(5),
                        loc: SourceLocation::Generated,
                    },
                    preds: HashSet::new(),
                    phis: vec![],
                },
            ),
            (
                BlockId(3),
                BasicBlock {
                    kind: BlockKind::Value,
                    id: BlockId(3),
                    instructions: vec![
                        Instruction {
                            id: InstructionId(6),
                            lvalue: make_place(3, None),
                            value: InstructionValue::Primitive {
                                value: PrimitiveValue::Undefined,
                                loc: SourceLocation::Generated,
                            },
                            loc: SourceLocation::Generated,
                            effects: None,
                        },
                        Instruction {
                            id: InstructionId(7),
                            lvalue: make_place(4, None),
                            value: InstructionValue::StoreLocal {
                                lvalue: LValue {
                                    place: make_place(4, None),
                                    kind: InstructionKind::Const,
                                },
                                value: make_place(3, None),
                                loc: SourceLocation::Generated,
                            },
                            loc: SourceLocation::Generated,
                            effects: None,
                        },
                    ],
                    terminal: Terminal::Goto {
                        block: BlockId(4),
                        variant: GotoVariant::Break,
                        id: InstructionId(8),
                        loc: SourceLocation::Generated,
                    },
                    preds: HashSet::new(),
                    phis: vec![],
                },
            ),
            (
                BlockId(4),
                BasicBlock {
                    kind: BlockKind::Block,
                    id: BlockId(4),
                    instructions: vec![],
                    terminal: Terminal::Return {
                        value: make_place(5, None),
                        return_variant: ReturnVariant::Explicit,
                        id: InstructionId(9),
                        loc: SourceLocation::Generated,
                    },
                    preds: HashSet::new(),
                    phis: vec![],
                },
            ),
        ];

        let func = HIRFunction {
            env: crate::environment::Environment::new(crate::options::EnvironmentConfig::default()),
            loc: SourceLocation::Generated,
            id: None,
            fn_type: ReactFunctionType::Component,
            params: vec![],
            returns: make_place(99, None),
            context: vec![],
            body: HIR {
                entry: BlockId(0),
                blocks,
            },
            generator: false,
            async_: false,
            directives: vec![],
            aliasing_effects: None,
        };

        let result = collect_optional_chain_sidemap(&func);

        // Should have temporaries for the consequent_id ($2) and property_id ($1)
        assert!(
            result
                .temporaries_read_in_optional
                .contains_key(&IdentifierId(2))
        );
        assert!(
            result
                .temporaries_read_in_optional
                .contains_key(&IdentifierId(1))
        );

        // Both should map to a?.b (identifier 'a', path [b, optional=true])
        let dep = &result.temporaries_read_in_optional[&IdentifierId(2)];
        assert_eq!(dep.identifier.id, IdentifierId(100));
        assert_eq!(dep.path.len(), 1);
        assert_eq!(dep.path[0].property, "b");
        assert!(dep.path[0].optional);

        // The StoreLocal instruction (id=4) and Branch terminal (id=2) should be processed
        assert!(
            result
                .processed_instrs_in_optional
                .contains(&ProcessedOptionalNode {
                    function_key,
                    id: InstructionId(4),
                    kind: ProcessedOptionalNodeKind::Instruction
                })
        );
        assert!(
            result
                .processed_instrs_in_optional
                .contains(&ProcessedOptionalNode {
                    function_key,
                    id: InstructionId(2),
                    kind: ProcessedOptionalNodeKind::Terminal
                })
        );
    }
}
