//! Port of upstream `Optimization/OutlineJsx.ts`.
//!
//! Outlines clusters of nested JSX instructions inside callbacks into a
//! synthesized component function (`_temp`, `_temp1`, ...).
//! This pass is gated by `@enableJsxOutlining` (env config).
//!
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.

use std::collections::{HashMap, HashSet};

use crate::environment::Environment;
use crate::hir::types::*;

use super::dead_code_elimination::dead_code_elimination;
use super::outline_functions::OutlinedFunction;

#[derive(Default)]
struct OutlineState {
    jsx: Vec<Instruction>,
    children: HashSet<IdentifierId>,
}

struct OutlineBatch<'a> {
    env: &'a Environment,
    globals: &'a HashMap<IdentifierId, Instruction>,
    state: &'a mut OutlineState,
    rewrite_instr: &'a mut HashMap<InstructionId, Vec<Instruction>>,
    identifier_name_overrides: &'a mut HashMap<IdentifierId, IdentifierName>,
    outlined: &'a mut Vec<OutlinedFunction>,
    ctx: &'a mut OutlineCtx,
}

#[derive(Debug, Clone)]
struct OutlinedJsxAttribute {
    original_name: String,
    new_name: String,
    place: Place,
}

#[derive(Default)]
struct OldToNewProps {
    by_id: HashMap<IdentifierId, OutlinedJsxAttribute>,
    ordered: Vec<OutlinedJsxAttribute>,
}

struct ProcessResult {
    instrs: Vec<Instruction>,
    outlined: OutlinedFunction,
    place_name_overrides: HashMap<IdentifierId, IdentifierName>,
}

struct OutlineCtx {
    used_names: HashSet<String>,
    name_counter: u32,
    debug: bool,
}

/// Outline JSX clusters into synthesized outlined functions.
pub fn outline_jsx(func: &mut HIRFunction) -> Vec<OutlinedFunction> {
    let mut outlined = Vec::new();
    let mut used_names = HashSet::new();
    collect_used_names(func, &mut used_names);

    let mut ctx = OutlineCtx {
        used_names,
        name_counter: 0,
        debug: std::env::var("DEBUG_OUTLINE_JSX").is_ok(),
    };
    outline_jsx_impl(func, &mut outlined, &mut ctx);
    outlined
}

fn outline_jsx_impl(
    func: &mut HIRFunction,
    outlined: &mut Vec<OutlinedFunction>,
    ctx: &mut OutlineCtx,
) {
    let fn_type = func.fn_type;
    let env = func.env.clone();
    let mut globals: HashMap<IdentifierId, Instruction> = HashMap::new();

    for (_, block) in &mut func.body.blocks {
        let mut rewrite_instr: HashMap<InstructionId, Vec<Instruction>> = HashMap::new();
        let mut identifier_name_overrides: HashMap<IdentifierId, IdentifierName> = HashMap::new();
        let mut state = OutlineState::default();

        for idx in (0..block.instructions.len()).rev() {
            // Clone for state/rewrites. Mut borrows below are instruction-local.
            let instr_snapshot = block.instructions[idx].clone();

            match &mut block.instructions[idx].value {
                InstructionValue::LoadGlobal { .. } => {
                    globals.insert(instr_snapshot.lvalue.identifier.id, instr_snapshot);
                }
                InstructionValue::FunctionExpression { lowered_func, .. } => {
                    outline_jsx_impl(&mut lowered_func.func, outlined, ctx);
                }
                InstructionValue::ObjectMethod { lowered_func, .. } => {
                    outline_jsx_impl(&mut lowered_func.func, outlined, ctx);
                }
                InstructionValue::JsxExpression { children, .. } => {
                    if !state
                        .children
                        .contains(&instr_snapshot.lvalue.identifier.id)
                    {
                        process_and_outline_jsx(
                            fn_type,
                            OutlineBatch {
                                env: &env,
                                globals: &globals,
                                state: &mut state,
                                rewrite_instr: &mut rewrite_instr,
                                identifier_name_overrides: &mut identifier_name_overrides,
                                outlined,
                                ctx,
                            },
                        );
                        state = OutlineState::default();
                    }

                    state.jsx.push(instr_snapshot);
                    if let Some(children) = children {
                        for child in children {
                            state.children.insert(child.identifier.id);
                        }
                    }
                }
                _ => {}
            }
        }

        process_and_outline_jsx(
            fn_type,
            OutlineBatch {
                env: &env,
                globals: &globals,
                state: &mut state,
                rewrite_instr: &mut rewrite_instr,
                identifier_name_overrides: &mut identifier_name_overrides,
                outlined,
                ctx,
            },
        );

        if !rewrite_instr.is_empty() {
            let mut new_instrs: Vec<Instruction> = Vec::with_capacity(block.instructions.len());
            for instr in &block.instructions {
                if let Some(replacements) = rewrite_instr.get(&instr.id) {
                    new_instrs.extend(replacements.iter().cloned());
                } else {
                    new_instrs.push(instr.clone());
                }
            }
            block.instructions = new_instrs;
        }

        if !identifier_name_overrides.is_empty() {
            apply_identifier_name_overrides(&mut block.instructions, &identifier_name_overrides);
        }
    }

    dead_code_elimination(func);
}

fn process_and_outline_jsx(fn_type: ReactFunctionType, batch: OutlineBatch<'_>) {
    let OutlineBatch {
        env,
        globals,
        state,
        rewrite_instr,
        identifier_name_overrides,
        outlined,
        ctx,
    } = batch;
    if state.jsx.len() <= 1 {
        return;
    }

    let Some(first_seen) = state.jsx.first() else {
        return;
    };
    let mut jsx = state.jsx.clone();
    jsx.sort_by_key(|instr| instr.id);

    if let Some(result) = process(fn_type, env, &jsx, globals, ctx) {
        if ctx.debug {
            eprintln!(
                "[OUTLINE_JSX] outlined={} replacing_instr={} cluster_size={}",
                result.outlined.name,
                first_seen.id.0,
                jsx.len()
            );
        }
        outlined.push(result.outlined);
        identifier_name_overrides.extend(result.place_name_overrides);
        rewrite_instr.insert(first_seen.id, result.instrs);
    }
}

fn process(
    fn_type: ReactFunctionType,
    env: &Environment,
    jsx: &[Instruction],
    globals: &HashMap<IdentifierId, Instruction>,
    ctx: &mut OutlineCtx,
) -> Option<ProcessResult> {
    // Upstream only outlines JSX in callbacks (non-Component functions).
    if fn_type == ReactFunctionType::Component {
        return None;
    }

    let props = collect_props(jsx)?;
    let mut place_name_overrides = HashMap::new();
    for prop in &props {
        if !prop.original_name.starts_with("#t") {
            continue;
        }
        if let Some(name) = &prop.place.identifier.name {
            place_name_overrides
                .entry(prop.place.identifier.id)
                .or_insert_with(|| name.clone());
        }
    }
    let outlined_tag = generate_outlined_name(ctx);
    let instrs = emit_outlined_jsx(env, jsx, &props, &outlined_tag)?;
    let fn_outlined = emit_outlined_fn(env, jsx, &props, globals, &outlined_tag)?;

    Some(ProcessResult {
        instrs,
        place_name_overrides,
        outlined: OutlinedFunction {
            name: outlined_tag.clone(),
            func: fn_outlined,
        },
    })
}

fn collect_props(instructions: &[Instruction]) -> Option<Vec<OutlinedJsxAttribute>> {
    let mut suffix_id = 1u32;
    let mut seen: HashSet<String> = HashSet::new();
    let mut attributes: Vec<OutlinedJsxAttribute> = Vec::new();
    let mut first_place_by_attr: HashMap<String, Place> = HashMap::new();
    let mut first_named_place_by_attr: HashMap<String, Place> = HashMap::new();
    let jsx_ids: HashSet<IdentifierId> = instructions
        .iter()
        .map(|i| i.lvalue.identifier.id)
        .collect();

    let mut generate_name = |old_name: &str| -> String {
        let mut new_name = old_name.to_string();
        while seen.contains(&new_name) {
            new_name = format!("{old_name}{suffix_id}");
            suffix_id += 1;
        }
        seen.insert(new_name.clone());
        new_name
    };

    for instr in instructions {
        let InstructionValue::JsxExpression {
            props, children, ..
        } = &instr.value
        else {
            continue;
        };

        for attr in props {
            match attr {
                JsxAttribute::SpreadAttribute { .. } => {
                    return None;
                }
                JsxAttribute::Attribute { name, place } => {
                    let new_name = generate_name(name);
                    let place = if place.identifier.name.is_none() {
                        first_named_place_by_attr
                            .get(name)
                            .cloned()
                            .unwrap_or_else(|| place.clone())
                    } else if let Some(first_place) = first_place_by_attr.get(name) {
                        if is_temp_like_identifier_name(&place.identifier)
                            && !is_temp_like_identifier_name(&first_place.identifier)
                        {
                            first_place.clone()
                        } else {
                            place.clone()
                        }
                    } else {
                        first_named_place_by_attr
                            .entry(name.clone())
                            .or_insert_with(|| place.clone());
                        place.clone()
                    };
                    first_place_by_attr
                        .entry(name.clone())
                        .or_insert_with(|| place.clone());
                    attributes.push(OutlinedJsxAttribute {
                        original_name: name.clone(),
                        new_name,
                        place,
                    });
                }
            }
        }

        if let Some(children) = children {
            for child in children {
                if jsx_ids.contains(&child.identifier.id) {
                    continue;
                }
                let mut promoted_child = child.clone();
                promote_temporary(&mut promoted_child.identifier);
                let new_name = generate_name("t");
                attributes.push(OutlinedJsxAttribute {
                    original_name: identifier_display_name(&promoted_child.identifier),
                    new_name,
                    place: promoted_child,
                });
            }
        }
    }

    Some(attributes)
}

fn apply_identifier_name_overrides(
    instructions: &mut [Instruction],
    overrides: &HashMap<IdentifierId, IdentifierName>,
) {
    for instr in instructions {
        if let Some(name) = overrides.get(&instr.lvalue.identifier.id) {
            instr.lvalue.identifier.name = Some(name.clone());
        }
    }
}

fn emit_outlined_jsx(
    env: &Environment,
    instructions: &[Instruction],
    outlined_props: &[OutlinedJsxAttribute],
    outlined_tag: &str,
) -> Option<Vec<Instruction>> {
    let props: Vec<JsxAttribute> = outlined_props
        .iter()
        .map(|p| JsxAttribute::Attribute {
            name: p.new_name.clone(),
            place: p.place.clone(),
        })
        .collect();

    let mut jsx_tag = create_temporary_place(env, SourceLocation::Generated);
    promote_temporary_jsx_tag(&mut jsx_tag.identifier);

    let load_jsx = Instruction {
        id: make_instruction_id(0),
        loc: SourceLocation::Generated,
        lvalue: jsx_tag.clone(),
        value: InstructionValue::LoadGlobal {
            binding: NonLocalBinding::ModuleLocal {
                name: outlined_tag.to_string(),
            },
            loc: SourceLocation::Generated,
        },
        effects: None,
    };

    let lvalue = instructions.last().map(|i| i.lvalue.clone())?;
    let jsx_expr = Instruction {
        id: make_instruction_id(0),
        loc: SourceLocation::Generated,
        lvalue,
        value: InstructionValue::JsxExpression {
            tag: JsxTag::Component(jsx_tag),
            props,
            children: None,
            loc: SourceLocation::Generated,
        },
        effects: None,
    };

    Some(vec![load_jsx, jsx_expr])
}

fn emit_outlined_fn(
    env: &Environment,
    jsx: &[Instruction],
    old_props: &[OutlinedJsxAttribute],
    globals: &HashMap<IdentifierId, Instruction>,
    outlined_name: &str,
) -> Option<HIRFunction> {
    let old_to_new_props = create_old_to_new_props_mapping(env, old_props);

    let props_obj = create_temporary_place(env, SourceLocation::Generated);

    let mut instructions: Vec<Instruction> = Vec::new();
    instructions.push(emit_destructure_props(env, &props_obj, &old_to_new_props));

    let load_global_instrs = emit_load_globals(jsx, globals)?;
    instructions.extend(load_global_instrs);

    let updated_jsx = emit_updated_jsx(jsx, &old_to_new_props)?;
    instructions.extend(updated_jsx);

    let return_value = instructions.last().map(|i| i.lvalue.clone())?;

    let block = BasicBlock {
        kind: BlockKind::Block,
        id: make_block_id(0),
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

    Some(HIRFunction {
        env: env.clone(),
        id: Some(outlined_name.to_string()),
        fn_type: ReactFunctionType::Component,
        params: vec![Argument::Place(props_obj)],
        returns: create_temporary_place(env, SourceLocation::Generated),
        context: Vec::new(),
        body: HIR {
            entry: block.id,
            blocks: vec![(block.id, block)],
        },
        generator: false,
        async_: false,
        directives: Vec::new(),
        aliasing_effects: None,
    })
}

fn emit_load_globals(
    jsx: &[Instruction],
    globals: &HashMap<IdentifierId, Instruction>,
) -> Option<Vec<Instruction>> {
    let mut instructions = Vec::new();
    for instr in jsx {
        let InstructionValue::JsxExpression { tag, .. } = &instr.value else {
            continue;
        };
        if let JsxTag::Component(place) = tag {
            let load_global = globals.get(&place.identifier.id)?;
            instructions.push(load_global.clone());
        }
    }
    Some(instructions)
}

fn emit_updated_jsx(
    jsx: &[Instruction],
    old_to_new_props: &OldToNewProps,
) -> Option<Vec<Instruction>> {
    let mut new_instrs = Vec::new();
    let jsx_ids: HashSet<IdentifierId> = jsx.iter().map(|i| i.lvalue.identifier.id).collect();

    for instr in jsx {
        let InstructionValue::JsxExpression {
            props, children, ..
        } = &instr.value
        else {
            continue;
        };

        let mut new_props = Vec::new();
        for prop in props {
            match prop {
                JsxAttribute::SpreadAttribute { .. } => {
                    return None;
                }
                JsxAttribute::Attribute { name, place } => {
                    if name == "key" {
                        continue;
                    }
                    let new_prop = old_to_new_props.by_id.get(&place.identifier.id)?;
                    new_props.push(JsxAttribute::Attribute {
                        name: new_prop.original_name.clone(),
                        place: new_prop.place.clone(),
                    });
                }
            }
        }

        let mut new_children: Option<Vec<Place>> = None;
        if let Some(children) = children {
            let mut rewritten_children = Vec::new();
            for child in children {
                if jsx_ids.contains(&child.identifier.id) {
                    rewritten_children.push(child.clone());
                    continue;
                }
                let new_child = old_to_new_props.by_id.get(&child.identifier.id)?;
                rewritten_children.push(new_child.place.clone());
            }
            new_children = Some(rewritten_children);
        }

        let mut new_instr = instr.clone();
        if let InstructionValue::JsxExpression {
            props, children, ..
        } = &mut new_instr.value
        {
            *props = new_props;
            *children = new_children;
        }
        new_instrs.push(new_instr);
    }

    Some(new_instrs)
}

fn create_old_to_new_props_mapping(
    env: &Environment,
    old_props: &[OutlinedJsxAttribute],
) -> OldToNewProps {
    let mut out = OldToNewProps::default();
    for old_prop in old_props {
        // Key is used by React and should not be read inside the outlined component.
        if old_prop.original_name == "key" {
            continue;
        }

        let mut new_place = create_temporary_place(env, SourceLocation::Generated);
        new_place.identifier.name = Some(IdentifierName::Named(old_prop.new_name.clone()));

        let new_prop = OutlinedJsxAttribute {
            original_name: old_prop.original_name.clone(),
            new_name: old_prop.new_name.clone(),
            place: new_place,
        };
        out.by_id
            .insert(old_prop.place.identifier.id, new_prop.clone());
        out.ordered.push(new_prop);
    }
    out
}

fn emit_destructure_props(
    env: &Environment,
    props_obj: &Place,
    old_to_new_props: &OldToNewProps,
) -> Instruction {
    let properties: Vec<ObjectPropertyOrSpread> = old_to_new_props
        .ordered
        .iter()
        .map(|prop| {
            ObjectPropertyOrSpread::Property(ObjectProperty {
                key: ObjectPropertyKey::String(prop.new_name.clone()),
                type_: ObjectPropertyType::Property,
                place: prop.place.clone(),
            })
        })
        .collect();

    Instruction {
        id: make_instruction_id(0),
        lvalue: create_temporary_place(env, SourceLocation::Generated),
        loc: SourceLocation::Generated,
        value: InstructionValue::Destructure {
            lvalue: LValuePattern {
                pattern: Pattern::Object(ObjectPattern { properties }),
                kind: InstructionKind::Let,
            },
            loc: SourceLocation::Generated,
            value: props_obj.clone(),
        },
        effects: None,
    }
}

fn generate_outlined_name(ctx: &mut OutlineCtx) -> String {
    loop {
        let candidate = if ctx.name_counter == 0 {
            "_temp".to_string()
        } else {
            format!("_temp{}", ctx.name_counter)
        };
        ctx.name_counter += 1;
        if ctx.used_names.insert(candidate.clone()) {
            return candidate;
        }
    }
}

fn create_temporary_place(env: &Environment, loc: SourceLocation) -> Place {
    Place {
        identifier: env.make_temporary_identifier(loc),
        effect: Effect::Unknown,
        reactive: false,
        loc: SourceLocation::Generated,
    }
}

fn promote_temporary(identifier: &mut Identifier) {
    if identifier.name.is_none() {
        identifier.name = Some(IdentifierName::Promoted(format!(
            "#t{}",
            identifier.declaration_id.0
        )));
    }
}

fn promote_temporary_jsx_tag(identifier: &mut Identifier) {
    if identifier.name.is_none() {
        identifier.name = Some(IdentifierName::Promoted(format!(
            "#T{}",
            identifier.declaration_id.0
        )));
    }
}

fn identifier_display_name(identifier: &Identifier) -> String {
    match &identifier.name {
        Some(IdentifierName::Named(name)) | Some(IdentifierName::Promoted(name)) => name.clone(),
        None => format!("_t{}", identifier.id.0),
    }
}

fn is_temp_like_identifier_name(identifier: &Identifier) -> bool {
    let Some(name) = &identifier.name else {
        return true;
    };
    let raw = name.value();
    let stripped = raw.strip_prefix('#').unwrap_or(raw);
    if let Some(rest) = stripped.strip_prefix('t') {
        return !rest.is_empty() && rest.chars().all(|ch| ch.is_ascii_digit());
    }
    false
}

fn collect_used_names(func: &HIRFunction, used: &mut HashSet<String>) {
    if let Some(id) = &func.id {
        used.insert(id.clone());
    }
    for param in &func.params {
        match param {
            Argument::Place(place) | Argument::Spread(place) => {
                if let Some(name) = &place.identifier.name {
                    used.insert(name.value().to_string());
                }
            }
        }
    }
    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            if let Some(name) = &instr.lvalue.identifier.name {
                used.insert(name.value().to_string());
            }
            match &instr.value {
                InstructionValue::FunctionExpression {
                    name: Some(name),
                    lowered_func,
                    ..
                } => {
                    used.insert(name.clone());
                    collect_used_names(&lowered_func.func, used);
                }
                InstructionValue::FunctionExpression {
                    name: None,
                    lowered_func,
                    ..
                }
                | InstructionValue::ObjectMethod { lowered_func, .. } => {
                    collect_used_names(&lowered_func.func, used);
                }
                _ => {}
            }
        }
    }
}
