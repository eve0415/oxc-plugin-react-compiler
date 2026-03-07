//! Name anonymous function expressions for debug-friendly codegen output.
//!
//! Port of `Transform/NameAnonymousFunctions.ts` from upstream React Compiler.
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.

use std::collections::HashMap;

use crate::hir::types::*;

#[derive(Debug)]
struct Node {
    block_index: usize,
    instruction_index: usize,
    function_name: Option<String>,
    generated_name: Option<String>,
    inner: Vec<Node>,
}

/// Assign generated names to anonymous function expressions.
///
/// Upstream stores generated names in `FunctionExpression.nameHint`; Rust HIR
/// currently has no dedicated `name_hint` field on function-expression
/// instructions, so we store the generated hint on `lowered_func.func.id` and
/// read it during reactive codegen.
pub fn name_anonymous_functions(func: &mut HIRFunction) {
    let Some(parent_name) = func.id.clone() else {
        return;
    };

    let debug = std::env::var("DEBUG_NAME_ANON_FUNCTIONS").is_ok();
    let nodes = name_anonymous_functions_impl(func);
    let prefix = format!("{parent_name}[");
    for node in &nodes {
        visit_named_node(func, node, &prefix, debug);
    }
}

fn name_anonymous_functions_impl(func: &mut HIRFunction) -> Vec<Node> {
    // Functions that we track to generate names for.
    let mut functions: HashMap<IdentifierId, usize> = HashMap::new();
    // Tracks temporaries that read from variables/globals/properties.
    let mut names: HashMap<IdentifierId, String> = HashMap::new();
    // Tracks all function nodes to bubble up for later renaming.
    let mut nodes: Vec<Node> = Vec::new();

    for block_index in 0..func.body.blocks.len() {
        let instructions_len = func.body.blocks[block_index].1.instructions.len();
        for instruction_index in 0..instructions_len {
            let instr = &mut func.body.blocks[block_index].1.instructions[instruction_index];
            let lvalue_id = instr.lvalue.identifier.id;

            match &mut instr.value {
                InstructionValue::LoadGlobal { binding, .. } => {
                    names.insert(lvalue_id, binding.name().to_string());
                }
                InstructionValue::LoadContext { place, .. }
                | InstructionValue::LoadLocal { place, .. } => {
                    if let Some(name) = named_identifier_name(&place.identifier.name) {
                        names.insert(lvalue_id, name.to_string());
                    }
                    if let Some(&node_idx) = functions.get(&place.identifier.id) {
                        functions.insert(lvalue_id, node_idx);
                    }
                }
                InstructionValue::PropertyLoad {
                    object, property, ..
                } => {
                    if let Some(object_name) = names.get(&object.identifier.id) {
                        names.insert(
                            lvalue_id,
                            format!("{object_name}.{}", property_literal_to_string(property)),
                        );
                    }
                }
                InstructionValue::FunctionExpression {
                    name, lowered_func, ..
                } => {
                    let inner = name_anonymous_functions_impl(&mut lowered_func.func);
                    let node_idx = nodes.len();
                    nodes.push(Node {
                        block_index,
                        instruction_index,
                        function_name: name.clone(),
                        generated_name: None,
                        inner,
                    });
                    if name.is_none() {
                        functions.insert(lvalue_id, node_idx);
                    }
                }
                InstructionValue::StoreContext { lvalue, value, .. }
                | InstructionValue::StoreLocal { lvalue, value, .. } => {
                    let Some(&node_idx) = functions.get(&value.identifier.id) else {
                        continue;
                    };
                    let Some(variable_name) = named_identifier_name(&lvalue.place.identifier.name)
                    else {
                        continue;
                    };
                    if nodes[node_idx].generated_name.is_none() {
                        nodes[node_idx].generated_name = Some(variable_name.to_string());
                        functions.remove(&value.identifier.id);
                    }
                }
                InstructionValue::CallExpression { callee, args, .. } => {
                    assign_generated_names_for_call(
                        callee,
                        args,
                        &mut functions,
                        &mut nodes,
                        &names,
                    );
                }
                InstructionValue::MethodCall { property, args, .. } => {
                    assign_generated_names_for_call(
                        property,
                        args,
                        &mut functions,
                        &mut nodes,
                        &names,
                    );
                }
                InstructionValue::JsxExpression { tag, props, .. } => {
                    for attr in props {
                        let JsxAttribute::Attribute { name, place } = attr else {
                            continue;
                        };
                        let Some(&node_idx) = functions.get(&place.identifier.id) else {
                            continue;
                        };
                        if nodes[node_idx].generated_name.is_some() {
                            continue;
                        }

                        let element_name = match tag {
                            JsxTag::BuiltinTag(name) => Some(name.clone()),
                            JsxTag::Component(place) => names.get(&place.identifier.id).cloned(),
                            JsxTag::Fragment => None,
                        };
                        let prop_name = match element_name {
                            Some(element_name) => format!("<{element_name}>.{name}"),
                            None => name.clone(),
                        };
                        nodes[node_idx].generated_name = Some(prop_name);
                        functions.remove(&place.identifier.id);
                    }
                }
                _ => {}
            }
        }
    }

    nodes
}

fn visit_named_node(func: &mut HIRFunction, node: &Node, prefix: &str, debug: bool) {
    let Some((_, block)) = func.body.blocks.get_mut(node.block_index) else {
        return;
    };
    let Some(instr) = block.instructions.get_mut(node.instruction_index) else {
        return;
    };
    let InstructionValue::FunctionExpression {
        name, lowered_func, ..
    } = &mut instr.value
    else {
        return;
    };

    if let Some(generated_name) = &node.generated_name
        && name.is_none()
        && lowered_func.func.id.is_none()
    {
        let full_name = format!("{prefix}{generated_name}]");
        if debug {
            eprintln!(
                "[NAME_ANON] set hint {} at bb{} instr#{}",
                full_name, block.id.0, instr.id.0
            );
        }
        lowered_func.func.id = Some(full_name);
    }

    let next_segment = node
        .generated_name
        .as_deref()
        .or(node.function_name.as_deref())
        .unwrap_or("<anonymous>");
    let next_prefix = format!("{prefix}{next_segment} > ");
    for inner in &node.inner {
        visit_named_node(&mut lowered_func.func, inner, &next_prefix, debug);
    }
}

fn assign_generated_names_for_call(
    callee: &Place,
    args: &[Argument],
    functions: &mut HashMap<IdentifierId, usize>,
    nodes: &mut [Node],
    names: &HashMap<IdentifierId, String>,
) {
    let callee_name = hook_kind_name_for_identifier(&callee.identifier)
        .map(ToString::to_string)
        .or_else(|| names.get(&callee.identifier.id).cloned())
        .unwrap_or_else(|| "(anonymous)".to_string());

    let fn_arg_count = args
        .iter()
        .filter_map(|arg| match arg {
            Argument::Place(place) if functions.contains_key(&place.identifier.id) => Some(()),
            Argument::Place(_) | Argument::Spread(_) => None,
        })
        .count();

    for (index, arg) in args.iter().enumerate() {
        let Argument::Place(place) = arg else {
            continue;
        };
        let Some(&node_idx) = functions.get(&place.identifier.id) else {
            continue;
        };
        if nodes[node_idx].generated_name.is_some() {
            continue;
        }

        let generated_name = if fn_arg_count > 1 {
            format!("{callee_name}(arg{index})")
        } else {
            format!("{callee_name}()")
        };
        nodes[node_idx].generated_name = Some(generated_name);
        functions.remove(&place.identifier.id);
    }
}

fn named_identifier_name(name: &Option<IdentifierName>) -> Option<&str> {
    match name {
        Some(IdentifierName::Named(name)) => Some(name),
        Some(IdentifierName::Promoted(_)) | None => None,
    }
}

fn property_literal_to_string(property: &PropertyLiteral) -> String {
    match property {
        PropertyLiteral::String(value) => value.clone(),
        PropertyLiteral::Number(value) => {
            if value.fract() == 0.0 {
                format!("{value:.0}")
            } else {
                value.to_string()
            }
        }
    }
}

fn hook_kind_name_for_identifier(identifier: &Identifier) -> Option<&'static str> {
    let Type::Function {
        shape_id: Some(shape_id),
        ..
    } = &identifier.type_
    else {
        return None;
    };

    match shape_id.as_str() {
        "BuiltInUseStateHookId" => Some("useState"),
        "BuiltInUseReducerHookId" => Some("useReducer"),
        "BuiltInUseContextHookId" => Some("useContext"),
        "BuiltInUseRefHookId" => Some("useRef"),
        "BuiltInUseMemoHookId" => Some("useMemo"),
        "BuiltInUseCallbackHookId" => Some("useCallback"),
        "BuiltInUseEffectHookId" => Some("useEffect"),
        "BuiltInUseLayoutEffectHookId" => Some("useLayoutEffect"),
        "BuiltInUseInsertionEffectHookId" => Some("useInsertionEffect"),
        "BuiltInUseTransitionHookId" => Some("useTransition"),
        "BuiltInUseImperativeHandleHookId" => Some("useImperativeHandle"),
        "BuiltInUseActionStateHookId" => Some("useActionState"),
        "BuiltInUseEffectEventHookId" => Some("useEffectEvent"),
        _ => None,
    }
}
