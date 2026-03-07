//! Scope dependency utilities.
//!
//! Port of `ScopeDependencyUtils.ts` from upstream React Compiler.
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.
//!
//! Provides utilities for building HIR instruction sequences that represent
//! scope dependencies, including support for optional property chains.

use super::types::*;

/// The result of building dependency instructions for a single
/// `ReactiveScopeDependency`.
///
/// Contains the HIR blocks that load the dependency value and the final
/// `Place` holding the loaded value.
#[derive(Debug)]
pub struct DependencyInstructions {
    /// The place holding the final loaded dependency value.
    pub place: Place,
    /// The HIR blocks that compute the dependency value.
    pub value: HIR,
    /// The exit block of the dependency HIR.
    pub exit_block_id: BlockId,
}

/// Build HIR instructions that load a `ReactiveScopeDependency`.
///
/// For a simple dependency like `x.a.b`, this produces a sequence of
/// `LoadLocal` and `PropertyLoad` instructions.
///
/// For a dependency with optional chains like `x?.a.b`, this produces
/// optional blocks with branch terminals.
pub fn build_dependency_instructions(
    dep: &ReactiveScopeDependency,
    next_block_id: &mut u32,
    next_identifier_id: &mut u32,
) -> DependencyInstructions {
    let all_non_optional = dep.path.iter().all(|p| !p.optional);

    if all_non_optional {
        build_non_optional_dependency(dep, next_block_id, next_identifier_id)
    } else {
        build_optional_dependency(dep, next_block_id, next_identifier_id)
    }
}

/// Build instructions for a dependency without optional chains.
/// Produces a chain of LoadLocal + PropertyLoad instructions in a single block.
fn build_non_optional_dependency(
    dep: &ReactiveScopeDependency,
    next_block_id: &mut u32,
    next_identifier_id: &mut u32,
) -> DependencyInstructions {
    let block_id = alloc_block_id(next_block_id);
    let loc = dep.identifier.loc.clone();

    let mut instructions = Vec::new();

    // First instruction: LoadLocal
    let curr_id = alloc_identifier_id(next_identifier_id);
    let curr = make_temporary_identifier(curr_id, loc.clone());

    instructions.push(Instruction {
        id: InstructionId::new(1),
        lvalue: Place {
            identifier: curr.clone(),
            effect: Effect::Mutate,
            reactive: false,
            loc: loc.clone(),
        },
        value: InstructionValue::LoadLocal {
            place: Place {
                identifier: dep.identifier.clone(),
                effect: Effect::Freeze,
                reactive: false,
                loc: loc.clone(),
            },
            loc: loc.clone(),
        },
        loc: loc.clone(),
        effects: None,
    });

    let mut current_ident = curr;

    // PropertyLoad for each path entry
    for entry in &dep.path {
        let next_id = alloc_identifier_id(next_identifier_id);
        let next_ident = make_temporary_identifier(next_id, loc.clone());

        instructions.push(Instruction {
            id: InstructionId::new(1),
            lvalue: Place {
                identifier: next_ident.clone(),
                effect: Effect::Mutate,
                reactive: false,
                loc: loc.clone(),
            },
            value: InstructionValue::PropertyLoad {
                object: Place {
                    identifier: current_ident,
                    effect: Effect::Freeze,
                    reactive: false,
                    loc: loc.clone(),
                },
                property: PropertyLiteral::String(entry.property.clone()),
                optional: false,
                loc: loc.clone(),
            },
            loc: loc.clone(),
            effects: None,
        });

        current_ident = next_ident;
    }

    let result_place = Place {
        identifier: current_ident,
        effect: Effect::Freeze,
        reactive: false,
        loc: loc.clone(),
    };

    let exit_block_id = alloc_block_id(next_block_id);

    let blocks = vec![(
        block_id,
        BasicBlock {
            kind: BlockKind::Value,
            id: block_id,
            instructions,
            terminal: Terminal::Unsupported {
                id: InstructionId::new(0),
                loc: SourceLocation::Generated,
            },
            preds: std::collections::HashSet::new(),
            phis: vec![],
        },
    )];

    DependencyInstructions {
        place: result_place,
        value: HIR {
            entry: block_id,
            blocks,
        },
        exit_block_id,
    }
}

/// Build instructions for a dependency with optional chains.
/// This is a simplified version that creates optional blocks.
fn build_optional_dependency(
    dep: &ReactiveScopeDependency,
    next_block_id: &mut u32,
    next_identifier_id: &mut u32,
) -> DependencyInstructions {
    // For optional dependencies, we split at the optional boundary.
    // The non-optional prefix is loaded unconditionally, and each
    // optional segment gets its own branch.
    //
    // For now, we build the full chain in a single block, marking
    // the optional loads. The full optional block structure (with
    // Branch terminals) would be built when integrating with the
    // HIR builder.

    let block_id = alloc_block_id(next_block_id);
    let loc = dep.identifier.loc.clone();

    let mut instructions = Vec::new();

    // LoadLocal for the root identifier
    let curr_id = alloc_identifier_id(next_identifier_id);
    let curr = make_temporary_identifier(curr_id, loc.clone());

    instructions.push(Instruction {
        id: InstructionId::new(1),
        lvalue: Place {
            identifier: curr.clone(),
            effect: Effect::Mutate,
            reactive: false,
            loc: loc.clone(),
        },
        value: InstructionValue::LoadLocal {
            place: Place {
                identifier: dep.identifier.clone(),
                effect: Effect::Freeze,
                reactive: false,
                loc: loc.clone(),
            },
            loc: loc.clone(),
        },
        loc: loc.clone(),
        effects: None,
    });

    let mut current_ident = curr;

    // PropertyLoad for each path entry (with optional flag)
    for entry in &dep.path {
        let next_id = alloc_identifier_id(next_identifier_id);
        let next_ident = make_temporary_identifier(next_id, loc.clone());

        instructions.push(Instruction {
            id: InstructionId::new(1),
            lvalue: Place {
                identifier: next_ident.clone(),
                effect: Effect::Mutate,
                reactive: false,
                loc: loc.clone(),
            },
            value: InstructionValue::PropertyLoad {
                object: Place {
                    identifier: current_ident,
                    effect: Effect::Freeze,
                    reactive: false,
                    loc: loc.clone(),
                },
                property: PropertyLiteral::String(entry.property.clone()),
                optional: entry.optional,
                loc: loc.clone(),
            },
            loc: loc.clone(),
            effects: None,
        });

        current_ident = next_ident;
    }

    let result_place = Place {
        identifier: current_ident,
        effect: Effect::Freeze,
        reactive: false,
        loc: loc.clone(),
    };

    let exit_block_id = alloc_block_id(next_block_id);

    let blocks = vec![(
        block_id,
        BasicBlock {
            kind: BlockKind::Value,
            id: block_id,
            instructions,
            terminal: Terminal::Unsupported {
                id: InstructionId::new(0),
                loc: SourceLocation::Generated,
            },
            preds: std::collections::HashSet::new(),
            phis: vec![],
        },
    )];

    DependencyInstructions {
        place: result_place,
        value: HIR {
            entry: block_id,
            blocks,
        },
        exit_block_id,
    }
}

fn alloc_block_id(counter: &mut u32) -> BlockId {
    let id = BlockId::new(*counter);
    *counter += 1;
    id
}

fn alloc_identifier_id(counter: &mut u32) -> IdentifierId {
    let id = IdentifierId::new(*counter);
    *counter += 1;
    id
}
