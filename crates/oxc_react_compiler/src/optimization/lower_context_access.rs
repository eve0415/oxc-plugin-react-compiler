//! LowerContextAccess — rewrite `useContext` destructuring to selector form.
//!
//! Port of `Optimization/LowerContextAccess.ts` from upstream React Compiler.
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.

use std::collections::{HashMap, HashSet};

use crate::environment::Environment;
use crate::hir::builder::reverse_postorder_blocks;
use crate::hir::prune_maybe_throws::mark_instruction_ids;
use crate::hir::types::*;
use crate::hir::visitors;
use crate::options::LowerContextAccessConfig;
use crate::ssa::enter_ssa;
use crate::type_inference;

/// Rewrite `useContext` destructuring calls to lowered selector-based calls.
///
/// Returns `true` when the pass rewrote at least one call.
pub fn lower_context_access(
    func: &mut HIRFunction,
    lowered_context_callee_config: &LowerContextAccessConfig,
) -> bool {
    let mut context_access: HashSet<IdentifierId> = HashSet::new();
    let mut context_keys: HashMap<IdentifierId, Vec<String>> = HashMap::new();

    for (_block_id, block) in &func.body.blocks {
        for instr in &block.instructions {
            match &instr.value {
                InstructionValue::CallExpression { callee, .. }
                    if is_use_context_hook_type(&callee.identifier) =>
                {
                    context_access.insert(instr.lvalue.identifier.id);
                }
                InstructionValue::Destructure { lvalue, value, .. } => {
                    let destructure_id = value.identifier.id;
                    if !context_access.contains(&destructure_id) {
                        continue;
                    }

                    let Some(keys) = get_context_keys(lvalue) else {
                        debug_log(&format!(
                            "[LOWER_CONTEXT_ACCESS] bail: unsupported-destructure lvalue_id={}",
                            instr.lvalue.identifier.id.0
                        ));
                        return false;
                    };

                    if context_keys.contains_key(&destructure_id) {
                        // TODO parity: accessing the same context value over multiple statements.
                        debug_log(&format!(
                            "[LOWER_CONTEXT_ACCESS] bail: duplicate-destructure context_id={}",
                            destructure_id.0
                        ));
                        return false;
                    }

                    context_keys.insert(destructure_id, keys);
                }
                _ => {}
            }
        }
    }

    if context_access.is_empty() || context_keys.is_empty() {
        return false;
    }

    let env = func.env.clone();
    let mut temp_alloc = TempPlaceAllocator::new(func);
    let mut changed = false;

    for (_block_id, block) in &mut func.body.blocks {
        let mut next_instructions: Option<Vec<Instruction>> = None;

        for i in 0..block.instructions.len() {
            let mut instr = block.instructions[i].clone();
            let maybe_keys = if let InstructionValue::CallExpression { callee, .. } = &instr.value {
                if is_use_context_hook_type(&callee.identifier) {
                    context_keys.get(&instr.lvalue.identifier.id).cloned()
                } else {
                    None
                }
            } else {
                None
            };

            if let Some(keys) = maybe_keys {
                if next_instructions.is_none() {
                    next_instructions = Some(block.instructions[..i].to_vec());
                }
                let lowered_binding = lowered_context_binding(lowered_context_callee_config);
                let Some(selector_fn_instr) = emit_selector_fn(&env, &mut temp_alloc, &keys) else {
                    debug_log("[LOWER_CONTEXT_ACCESS] selector generation failed; skipping pass");
                    return false;
                };

                let mut rewritten_existing_callee = false;
                if let Some(next) = next_instructions.as_mut() {
                    next.push(selector_fn_instr.clone());
                    if let InstructionValue::CallExpression { callee, .. } = &instr.value {
                        let callee_id = callee.identifier.id;
                        if let Some(def_instr) = next
                            .iter_mut()
                            .rev()
                            .find(|candidate| candidate.lvalue.identifier.id == callee_id)
                            && let InstructionValue::LoadGlobal { binding, .. } =
                                &mut def_instr.value
                        {
                            *binding = lowered_binding.clone();
                            rewritten_existing_callee = true;
                        }
                    }
                }

                if let InstructionValue::CallExpression { callee, args, .. } = &mut instr.value {
                    if !rewritten_existing_callee {
                        let lowered_context_callee_instr = emit_load_lowered_context_callee(
                            &mut temp_alloc,
                            lowered_binding.clone(),
                        );
                        if let Some(next) = next_instructions.as_mut() {
                            next.push(lowered_context_callee_instr.clone());
                        }
                        *callee = lowered_context_callee_instr.lvalue;
                    }
                    args.push(Argument::Place(selector_fn_instr.lvalue.clone()));
                }

                debug_log(&format!(
                    "[LOWER_CONTEXT_ACCESS] rewrite call lvalue_id={} keys={:?}",
                    instr.lvalue.identifier.id.0, keys
                ));
                changed = true;
            }

            if let Some(next) = next_instructions.as_mut() {
                next.push(instr);
            }
        }

        if let Some(next) = next_instructions {
            block.instructions = next;
        }
    }

    if changed {
        mark_instruction_ids(&mut func.body);
        type_inference::infer_types(func);
    }

    changed
}

fn debug_log(msg: &str) {
    if std::env::var("DEBUG_LOWER_CONTEXT_ACCESS").is_ok() {
        eprintln!("{msg}");
    }
}

fn is_use_context_hook_type(ident: &Identifier) -> bool {
    matches!(
        &ident.type_,
        Type::Function { shape_id: Some(shape_id), .. } if shape_id == "BuiltInUseContextHookId"
    )
}

struct TempPlaceAllocator {
    next_identifier_id: u32,
}

impl TempPlaceAllocator {
    fn new(func: &HIRFunction) -> Self {
        Self {
            next_identifier_id: max_identifier_id(func).saturating_add(1),
        }
    }

    fn create_temporary_place(&mut self) -> Place {
        let identifier_id = IdentifierId::new(self.next_identifier_id);
        self.next_identifier_id = self.next_identifier_id.saturating_add(1);
        Place {
            identifier: make_temporary_identifier(identifier_id, SourceLocation::Generated),
            effect: Effect::Unknown,
            reactive: false,
            loc: SourceLocation::Generated,
        }
    }
}

fn max_identifier_id(func: &HIRFunction) -> u32 {
    fn record_place_id(max_id: &mut u32, place: &Place) {
        *max_id = (*max_id).max(place.identifier.id.0);
    }

    let mut max_id = 0u32;

    for param in &func.params {
        match param {
            Argument::Place(place) | Argument::Spread(place) => record_place_id(&mut max_id, place),
        }
    }
    record_place_id(&mut max_id, &func.returns);
    for place in &func.context {
        record_place_id(&mut max_id, place);
    }

    for (_, block) in &func.body.blocks {
        for phi in &block.phis {
            record_place_id(&mut max_id, &phi.place);
            for operand in phi.operands.values() {
                record_place_id(&mut max_id, operand);
            }
        }

        for instr in &block.instructions {
            visitors::for_each_instruction_lvalue(instr, |place| {
                record_place_id(&mut max_id, place);
            });
            visitors::for_each_instruction_operand(instr, |place| {
                record_place_id(&mut max_id, place);
            });

            match &instr.value {
                InstructionValue::FunctionExpression { lowered_func, .. }
                | InstructionValue::ObjectMethod { lowered_func, .. } => {
                    max_id = max_id.max(max_identifier_id(&lowered_func.func));
                }
                _ => {}
            }
        }

        visitors::for_each_terminal_operand(&block.terminal, |place| {
            record_place_id(&mut max_id, place);
        });
    }

    max_id
}

fn create_temporary_place(temp_alloc: &mut TempPlaceAllocator) -> Place {
    temp_alloc.create_temporary_place()
}

fn lowered_context_binding(
    lowered_context_callee_config: &LowerContextAccessConfig,
) -> NonLocalBinding {
    NonLocalBinding::ImportSpecifier {
        name: lowered_context_callee_config.imported_name.clone(),
        module: lowered_context_callee_config.module.clone(),
        imported: lowered_context_callee_config.imported_name.clone(),
    }
}

fn emit_load_lowered_context_callee(
    temp_alloc: &mut TempPlaceAllocator,
    binding: NonLocalBinding,
) -> Instruction {
    Instruction {
        id: InstructionId(0),
        lvalue: create_temporary_place(temp_alloc),
        effects: None,
        loc: SourceLocation::Generated,
        value: InstructionValue::LoadGlobal {
            binding,
            loc: SourceLocation::Generated,
        },
    }
}

fn get_context_keys(value: &LValuePattern) -> Option<Vec<String>> {
    match &value.pattern {
        Pattern::Array(_) => None,
        Pattern::Object(pattern) => {
            let mut keys = Vec::new();
            for prop in &pattern.properties {
                let ObjectPropertyOrSpread::Property(prop) = prop else {
                    return None;
                };
                if prop.type_ != ObjectPropertyType::Property {
                    return None;
                }
                let ObjectPropertyKey::Identifier(key) = &prop.key else {
                    return None;
                };
                let Some(IdentifierName::Named(_)) = &prop.place.identifier.name else {
                    return None;
                };
                keys.push(key.clone());
            }
            Some(keys)
        }
    }
}

fn emit_property_load(
    temp_alloc: &mut TempPlaceAllocator,
    obj: &Place,
    property: &str,
) -> (Vec<Instruction>, Place) {
    let object = create_temporary_place(temp_alloc);
    let load_local_instr = Instruction {
        id: InstructionId(0),
        lvalue: object.clone(),
        effects: None,
        loc: SourceLocation::Generated,
        value: InstructionValue::LoadLocal {
            place: obj.clone(),
            loc: SourceLocation::Generated,
        },
    };

    let element = create_temporary_place(temp_alloc);
    let load_prop_instr = Instruction {
        id: InstructionId(0),
        lvalue: element.clone(),
        effects: None,
        loc: SourceLocation::Generated,
        value: InstructionValue::PropertyLoad {
            object,
            property: PropertyLiteral::String(property.to_string()),
            optional: false,
            loc: SourceLocation::Generated,
        },
    };

    (vec![load_local_instr, load_prop_instr], element)
}

fn emit_array_instr(elements: Vec<Place>, temp_alloc: &mut TempPlaceAllocator) -> Instruction {
    let array_lvalue = create_temporary_place(temp_alloc);
    Instruction {
        id: InstructionId(0),
        lvalue: array_lvalue,
        effects: None,
        loc: SourceLocation::Generated,
        value: InstructionValue::ArrayExpression {
            elements: elements.into_iter().map(ArrayElement::Place).collect(),
            loc: SourceLocation::Generated,
        },
    }
}

fn emit_selector_fn(
    env: &Environment,
    temp_alloc: &mut TempPlaceAllocator,
    keys: &[String],
) -> Option<Instruction> {
    let mut obj = create_temporary_place(temp_alloc);
    promote_temporary(&mut obj.identifier);

    let mut instructions: Vec<Instruction> = Vec::new();
    let mut elements: Vec<Place> = Vec::new();
    for key in keys {
        let (mut emitted, element) = emit_property_load(temp_alloc, &obj, key);
        instructions.append(&mut emitted);
        elements.push(element);
    }

    let array_instr = emit_array_instr(elements, temp_alloc);
    let return_value = array_instr.lvalue.clone();
    instructions.push(array_instr);

    let block_id = make_block_id(0);
    let block = BasicBlock {
        kind: BlockKind::Block,
        id: block_id,
        instructions,
        terminal: Terminal::Return {
            value: return_value,
            return_variant: ReturnVariant::Explicit,
            id: make_instruction_id(0),
            loc: SourceLocation::Generated,
        },
        preds: HashSet::new(),
        phis: Vec::new(),
    };

    let mut selector_fn = HIRFunction {
        env: env.clone(),
        loc: SourceLocation::Generated,
        id: None,
        fn_type: ReactFunctionType::Other,
        params: vec![Argument::Place(obj)],
        returns: create_temporary_place(temp_alloc),
        context: Vec::new(),
        body: HIR {
            entry: block_id,
            blocks: vec![(block_id, block)],
        },
        generator: false,
        async_: false,
        directives: Vec::new(),
        aliasing_effects: None,
    };

    reverse_postorder_blocks(&mut selector_fn.body);
    mark_instruction_ids(&mut selector_fn.body);
    if let Err(err) = enter_ssa::enter_ssa(&mut selector_fn) {
        debug_log(&format!(
            "[LOWER_CONTEXT_ACCESS] selector enter_ssa failed: {:?}",
            err
        ));
        return None;
    }
    type_inference::infer_types(&mut selector_fn);

    Some(Instruction {
        id: make_instruction_id(0),
        value: InstructionValue::FunctionExpression {
            name: None,
            lowered_func: LoweredFunction { func: selector_fn },
            expr_type: FunctionExpressionType::ArrowFunctionExpression,
            loc: SourceLocation::Generated,
        },
        lvalue: create_temporary_place(temp_alloc),
        effects: None,
        loc: SourceLocation::Generated,
    })
}

/// Promote a temporary identifier to a named identifier.
/// Mirrors upstream `promoteTemporary` which sets name to `#t{declarationId}`.
fn promote_temporary(identifier: &mut Identifier) {
    if identifier.name.is_none() {
        identifier.name = Some(IdentifierName::Promoted(format!(
            "#t{}",
            identifier.declaration_id.0
        )));
    }
}
