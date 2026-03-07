//! Minimal type inference — port of upstream InferTypes.ts.
//!
//! This implements a simplified version of the React Compiler's type inference.
//! The key goal is to identify which instruction results are **definitely primitive**
//! so that `may_allocate()` in InferReactiveScopeVariables can return false for them,
//! preventing over-merging of reactive scopes.
//!
//! Full upstream uses unification with TypeVars; we use a simpler forward dataflow
//! approach that propagates known types through instructions.
//!
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.

use std::collections::{HashMap, HashSet};

use crate::hir::globals::{GlobalKind, GlobalRegistry};
use crate::hir::object_shape::{
    BUILT_IN_ARRAY_ID, BUILT_IN_FUNCTION_ID, BUILT_IN_JSX_ID, BUILT_IN_MIXED_READONLY_ID,
    BUILT_IN_OBJECT_ID, BUILT_IN_USE_REF_ID, PropertyType, REANIMATED_FROZEN_HOOK_ID,
    REANIMATED_MODULE_ID, REANIMATED_MUTABLE_FUNCTION_ID, REANIMATED_MUTABLE_HOOK_ID,
    REANIMATED_SHARED_VALUE_ID, ReturnType, TEST_INVALID_TYPE_PROVIDER_MODULE_ID,
    TEST_INVALID_TYPE_PROVIDER_NON_HOOK_TYPED_AS_HOOK_ID, TEST_KNOWN_INCOMPATIBLE_FUNCTION_ID,
    TEST_KNOWN_INCOMPATIBLE_HOOK_ID, TEST_KNOWN_INCOMPATIBLE_INDIRECT_FUNCTION_ID,
    TEST_KNOWN_INCOMPATIBLE_INDIRECT_HOOK_ID, TEST_KNOWN_INCOMPATIBLE_INDIRECT_RESULT_ID,
    TEST_KNOWN_INCOMPATIBLE_MODULE_ID, TEST_SHARED_RUNTIME_GRAPHQL_FN_ID,
    TEST_SHARED_RUNTIME_MODULE_ID, TEST_SHARED_RUNTIME_TYPED_ALIAS_FN_ID,
    TEST_SHARED_RUNTIME_TYPED_ARRAY_PUSH_FN_ID, TEST_SHARED_RUNTIME_TYPED_ASSIGN_FN_ID,
    TEST_SHARED_RUNTIME_TYPED_CAPTURE_FN_ID, TEST_SHARED_RUNTIME_TYPED_CREATE_FROM_FN_ID,
    TEST_SHARED_RUNTIME_TYPED_IDENTITY_FN_ID, TEST_SHARED_RUNTIME_TYPED_LOG_FN_ID,
    TEST_SHARED_RUNTIME_TYPED_MUTATE_FN_ID, TEST_SHARED_RUNTIME_USE_FRAGMENT_HOOK_ID,
    TEST_SHARED_RUNTIME_USE_FREEZE_HOOK_ID, TEST_SHARED_RUNTIME_USE_NO_ALIAS_HOOK_ID,
};
use crate::hir::types::*;
use crate::hir::visitors;
use crate::inference::infer_mutation_aliasing_effects::encode_method_signature_shape_id;

/// Run type inference on a function, setting `identifier.type_` for all identifiers.
///
/// After this pass, identifiers whose values are known to be primitive will have
/// `Type::Primitive`, enabling better reactive scope analysis.
pub fn infer_types(func: &mut HIRFunction) {
    let debug_types = std::env::var("DEBUG_TYPE_INFERENCE").is_ok();
    if debug_types {
        for ctx in &func.context {
            eprintln!(
                "[TYPE_INFER_CTX] fn={} id={} decl={} ty={:?}",
                func.id.as_deref().unwrap_or("<anonymous>"),
                ctx.identifier.id.0,
                ctx.identifier.declaration_id.0,
                ctx.identifier.type_
            );
        }
    }

    // Upstream infers nested lowered functions when generating equations.
    // Mirror that by recursively inferring types for all nested function bodies first.
    for (_bid, block) in &mut func.body.blocks {
        for instr in &mut block.instructions {
            match &mut instr.value {
                InstructionValue::FunctionExpression { lowered_func, .. }
                | InstructionValue::ObjectMethod { lowered_func, .. } => {
                    infer_types(&mut lowered_func.func);
                }
                _ => {}
            }
        }
    }

    // Phase 1: Build type equations by forward analysis
    let mut id_types: HashMap<IdentifierId, Type> = HashMap::new();
    let mut id_names: HashMap<IdentifierId, String> = HashMap::new();
    let mut declaration_ids: HashMap<DeclarationId, Vec<IdentifierId>> = HashMap::new();
    // Upstream BuildHIR represents captured mutable locals as context variables.
    // Our lowering can still keep them as local loads/stores; treat those
    // declarations as context-like for type equations.
    let mut context_like_declaration_ids = collect_captured_context_declarations(func);

    // Seed with already-known concrete types (e.g. context captures in lowered
    // nested functions). Falling back to Poly loses this information.
    let seed_identifier = |id_types: &mut HashMap<IdentifierId, Type>,
                           id_names: &mut HashMap<IdentifierId, String>,
                           declaration_ids: &mut HashMap<DeclarationId, Vec<IdentifierId>>,
                           ident: &Identifier| {
        declaration_ids
            .entry(ident.declaration_id)
            .or_default()
            .push(ident.id);
        if let Some(name) = get_name(ident) {
            id_names.entry(ident.id).or_insert_with(|| name.to_string());
        }
        if !matches!(ident.type_, Type::Poly | Type::TypeVar { .. }) {
            id_types
                .entry(ident.id)
                .or_insert_with(|| ident.type_.clone());
        }
    };

    for param in &func.params {
        match param {
            Argument::Place(place) | Argument::Spread(place) => {
                seed_identifier(
                    &mut id_types,
                    &mut id_names,
                    &mut declaration_ids,
                    &place.identifier,
                );
            }
        }
    }
    for place in &func.context {
        seed_identifier(
            &mut id_types,
            &mut id_names,
            &mut declaration_ids,
            &place.identifier,
        );
    }
    seed_identifier(
        &mut id_types,
        &mut id_names,
        &mut declaration_ids,
        &func.returns.identifier,
    );

    // Upstream InferTypes seeds the second Component param (`ref`) to BuiltInUseRefId.
    if func.fn_type == ReactFunctionType::Component
        && let Some(Argument::Place(ref_param)) = func.params.get(1)
    {
        assign_type_to_declaration(
            ref_param.identifier.declaration_id,
            Type::Object {
                shape_id: Some(BUILT_IN_USE_REF_ID.to_string()),
            },
            &declaration_ids,
            &mut id_types,
        );
    }

    for (_bid, block) in &func.body.blocks {
        for phi in &block.phis {
            seed_identifier(
                &mut id_types,
                &mut id_names,
                &mut declaration_ids,
                &phi.place.identifier,
            );
            for op in phi.operands.values() {
                seed_identifier(
                    &mut id_types,
                    &mut id_names,
                    &mut declaration_ids,
                    &op.identifier,
                );
            }
        }
        for instr in &block.instructions {
            match &instr.value {
                InstructionValue::DeclareContext { lvalue, .. }
                | InstructionValue::StoreContext { lvalue, .. } => {
                    context_like_declaration_ids.insert(lvalue.place.identifier.declaration_id);
                }
                _ => {}
            }
            visitors::for_each_instruction_lvalue(instr, |place| {
                seed_identifier(
                    &mut id_types,
                    &mut id_names,
                    &mut declaration_ids,
                    &place.identifier,
                );
            });
            visitors::for_each_instruction_operand(instr, |place| {
                seed_identifier(
                    &mut id_types,
                    &mut id_names,
                    &mut declaration_ids,
                    &place.identifier,
                );
            });
        }
        visitors::for_each_terminal_operand(&block.terminal, |place| {
            seed_identifier(
                &mut id_types,
                &mut id_names,
                &mut declaration_ids,
                &place.identifier,
            );
        });
    }

    // Track string literal values for temporaries created from Primitive::String.
    // This is needed to resolve method property names (e.g., `React.useState` lowers
    // to a MethodCall whose property is a temporary backed by Primitive::String("useState")).
    let mut id_string_values: HashMap<IdentifierId, String> = HashMap::new();
    let mut store_local_decl_writes: HashMap<DeclarationId, usize> = HashMap::new();
    for (_bid, block) in &func.body.blocks {
        for instr in &block.instructions {
            if let InstructionValue::StoreLocal { lvalue, .. } = &instr.value {
                *store_local_decl_writes
                    .entry(lvalue.place.identifier.declaration_id)
                    .or_insert(0) += 1;
            }
        }
    }

    // Multiple passes to propagate through phi nodes and loads
    for _pass in 0..3 {
        for (_bid, block) in &func.body.blocks {
            // Process phis conservatively. Upstream keeps per-identifier type
            // variables and only resolves a phi when all operands provide
            // enough evidence; skipping unknown operands here over-constrains
            // mixed branches (for example `ref.current` vs an unknown object).
            for phi in &block.phis {
                let mut merged_type: Option<Type> = None;
                let mut all_operands_known = true;
                for op in phi.operands.values() {
                    let op_type = id_types
                        .get(&op.identifier.id)
                        .cloned()
                        .unwrap_or(op.identifier.type_.clone());
                    if matches!(op_type, Type::Poly | Type::TypeVar { .. }) {
                        all_operands_known = false;
                        break;
                    }
                    merged_type = Some(match merged_type.take() {
                        None => op_type,
                        Some(existing) => merge_known_types(Some(existing), Some(op_type)),
                    });
                }
                if all_operands_known
                    && let Some(ty) = merged_type
                    && !matches!(ty, Type::Poly | Type::TypeVar { .. })
                {
                    id_types.insert(phi.place.identifier.id, ty);
                }
            }

            for instr in &block.instructions {
                let lv_id = instr.lvalue.identifier.id;
                match &instr.value {
                    InstructionValue::LoadLocal { place, .. } => {
                        if let Some(name) = identifier_name(place, &id_names) {
                            id_names.insert(lv_id, name);
                        }

                        // Upstream BuildHIR lowers context-box reads as LoadContext.
                        // Our lowering can still produce LoadLocal for context-backed
                        // declarations; treat them like LoadContext in type inference.
                        if context_like_declaration_ids.contains(&place.identifier.declaration_id) {
                            continue;
                        }

                        // Load instructions unify the specific source/destination
                        // identifiers. For multi-write declarations, propagating a
                        // concrete type across every SSA version over-constrains mixed
                        // control-flow compared to upstream's per-identifier unifier.
                        let source_ty = get_type(place.identifier.id, &id_types);
                        if !matches!(source_ty, Type::Poly | Type::TypeVar { .. }) {
                            assign_type_to_identifier_or_single_write_declaration(
                                &instr.lvalue.identifier,
                                source_ty,
                                &declaration_ids,
                                &store_local_decl_writes,
                                &mut id_types,
                            );
                        }
                        let dest_ty = get_type(lv_id, &id_types);
                        if !matches!(dest_ty, Type::Poly | Type::TypeVar { .. }) {
                            assign_type_to_identifier_or_single_write_declaration(
                                &place.identifier,
                                dest_ty,
                                &declaration_ids,
                                &store_local_decl_writes,
                                &mut id_types,
                            );
                        }
                    }
                    // Upstream InferTypes intentionally does not infer from LoadContext.
                    // Context variables are only constrained by StoreContext const and
                    // other equations, not direct load propagation.
                    InstructionValue::LoadContext { .. } => {}
                    InstructionValue::LoadGlobal { binding, .. } => {
                        id_names.insert(lv_id, binding.name().to_string());

                        // Inner lowered functions can reference captured outer variables
                        // via `LoadGlobal { name }`. Mirror LoadLocal-like unification
                        // against the context capture with the same name.
                        if let NonLocalBinding::Global { name } = binding
                            && let Some(context_place) = func.context.iter().find(|p| {
                                get_name(&p.identifier).is_some_and(|n| n == name.as_str())
                            })
                        {
                            let source_ty = get_type(context_place.identifier.id, &id_types);
                            if !matches!(source_ty, Type::Poly | Type::TypeVar { .. }) {
                                assign_type_to_identifier_or_single_write_declaration(
                                    &instr.lvalue.identifier,
                                    source_ty,
                                    &declaration_ids,
                                    &store_local_decl_writes,
                                    &mut id_types,
                                );
                            }
                            let dest_ty = get_type(lv_id, &id_types);
                            if !matches!(dest_ty, Type::Poly | Type::TypeVar { .. }) {
                                assign_type_to_identifier_or_single_write_declaration(
                                    &context_place.identifier,
                                    dest_ty,
                                    &declaration_ids,
                                    &store_local_decl_writes,
                                    &mut id_types,
                                );
                            }
                            if debug_types {
                                eprintln!(
                                    "[TYPE_INFER_LOAD_GLOBAL_CTX] fn={} instr#{} name={} lvalue_id={} ctx_id={} lvalue_ty={:?} ctx_ty={:?}",
                                    func.id.as_deref().unwrap_or("<anonymous>"),
                                    instr.id.0,
                                    name,
                                    lv_id.0,
                                    context_place.identifier.id.0,
                                    get_type(lv_id, &id_types),
                                    get_type(context_place.identifier.id, &id_types)
                                );
                            }
                        }
                    }
                    InstructionValue::PropertyLoad {
                        property: PropertyLiteral::String(prop_name),
                        ..
                    } => {
                        id_names.insert(lv_id, prop_name.clone());
                    }
                    _ => {}
                }

                // Upstream `enableTreatSetIdentifiersAsStateSetters`: treat called
                // `set*` identifiers as state-setter functions.
                if func
                    .env
                    .config()
                    .enable_treat_set_identifiers_as_state_setters
                    && let InstructionValue::CallExpression { callee, .. } = &instr.value
                    && let Some(callee_name) = identifier_name(callee, &id_names)
                    && callee_name.starts_with("set")
                {
                    assign_type_to_declaration(
                        callee.identifier.declaration_id,
                        Type::Function {
                            shape_id: Some("BuiltInSetState".to_string()),
                            return_type: Box::new(Type::Poly),
                            is_constructor: false,
                        },
                        &declaration_ids,
                        &mut id_types,
                    );
                }

                // Upstream `enableTreatRefLikeIdentifiersAsRefs`: infer ref-like
                // objects from `<maybeRef>.current` property access.
                if func.env.config().enable_treat_ref_like_identifiers_as_refs
                    && let InstructionValue::PropertyLoad {
                        object, property, ..
                    } = &instr.value
                    && matches!(property, PropertyLiteral::String(prop) if prop == "current")
                {
                    let object_name = identifier_name(object, &id_names);
                    if debug_types {
                        eprintln!(
                            "[TYPE_INFER_REFLIKE_LOAD] fn={} instr#{} object_id={} object_decl={} object_ident_name={} resolved_name={:?}",
                            func.id.as_deref().unwrap_or("<anonymous>"),
                            instr.id.0,
                            object.identifier.id.0,
                            object.identifier.declaration_id.0,
                            identifier_debug_name(&object.identifier),
                            object_name
                        );
                    }
                    if let Some(object_name) = object_name
                        && is_ref_like_name(&object_name)
                    {
                        assign_type_to_declaration(
                            object.identifier.declaration_id,
                            Type::Object {
                                shape_id: Some("BuiltInUseRefId".to_string()),
                            },
                            &declaration_ids,
                            &mut id_types,
                        );
                    }
                }
                // Upstream also infers ref-like objects from `<maybeRef>.current`
                // property stores (InferTypes.ts PropertyStore + Property unification).
                if func.env.config().enable_treat_ref_like_identifiers_as_refs
                    && let InstructionValue::PropertyStore {
                        object, property, ..
                    } = &instr.value
                    && matches!(property, PropertyLiteral::String(prop) if prop == "current")
                {
                    let object_name = identifier_name(object, &id_names);
                    if debug_types {
                        eprintln!(
                            "[TYPE_INFER_REFLIKE_STORE] fn={} instr#{} object_id={} object_decl={} object_ident_name={} resolved_name={:?}",
                            func.id.as_deref().unwrap_or("<anonymous>"),
                            instr.id.0,
                            object.identifier.id.0,
                            object.identifier.declaration_id.0,
                            identifier_debug_name(&object.identifier),
                            object_name
                        );
                    }
                    if let Some(object_name) = object_name
                        && is_ref_like_name(&object_name)
                    {
                        assign_type_to_declaration(
                            object.identifier.declaration_id,
                            Type::Object {
                                shape_id: Some("BuiltInUseRefId".to_string()),
                            },
                            &declaration_ids,
                            &mut id_types,
                        );
                    }
                }

                // Upstream InferTypes constrains JSX `ref` attributes to BuiltInUseRefId
                // when `enableTreatRefLikeIdentifiersAsRefs` is enabled.
                if func.env.config().enable_treat_ref_like_identifiers_as_refs
                    && let InstructionValue::JsxExpression { props, .. } = &instr.value
                {
                    for prop in props {
                        if let JsxAttribute::Attribute { name, place } = prop
                            && name == "ref"
                        {
                            assign_type_to_declaration(
                                place.identifier.declaration_id,
                                Type::Object {
                                    shape_id: Some(BUILT_IN_USE_REF_ID.to_string()),
                                },
                                &declaration_ids,
                                &mut id_types,
                            );
                        }
                    }
                }

                if debug_types {
                    eprintln!(
                        "[TYPE_INFER_INSTR] fn={} pass={} bb{} instr#{} kind={} lvalue_id={} lvalue_name={} lvalue_ty={:?}",
                        func.id.as_deref().unwrap_or("<anonymous>"),
                        _pass,
                        block.id.0,
                        instr.id.0,
                        instruction_value_kind(&instr.value),
                        lv_id.0,
                        identifier_debug_name(&instr.lvalue.identifier),
                        instr.lvalue.identifier.type_
                    );
                    match &instr.value {
                        InstructionValue::LoadLocal { place, .. }
                        | InstructionValue::LoadContext { place, .. } => {
                            eprintln!(
                                "[TYPE_INFER_LOAD] src_id={} src_name={} src_ty={:?}",
                                place.identifier.id.0,
                                identifier_debug_name(&place.identifier),
                                get_type(place.identifier.id, &id_types)
                            );
                        }
                        InstructionValue::StoreLocal { lvalue, value, .. }
                        | InstructionValue::StoreContext { lvalue, value, .. } => {
                            eprintln!(
                                "[TYPE_INFER_STORE] target_id={} target_name={} target_ty={:?} value_id={} value_name={} value_ty={:?}",
                                lvalue.place.identifier.id.0,
                                identifier_debug_name(&lvalue.place.identifier),
                                get_type(lvalue.place.identifier.id, &id_types),
                                value.identifier.id.0,
                                identifier_debug_name(&value.identifier),
                                get_type(value.identifier.id, &id_types)
                            );
                        }
                        InstructionValue::PropertyLoad {
                            object, property, ..
                        } => {
                            eprintln!(
                                "[TYPE_INFER_PROP_LOAD] obj_id={} obj_name={} obj_ty={:?} prop={:?}",
                                object.identifier.id.0,
                                identifier_debug_name(&object.identifier),
                                get_type(object.identifier.id, &id_types),
                                property
                            );
                        }
                        InstructionValue::CallExpression { callee, .. } => {
                            eprintln!(
                                "[TYPE_INFER_CALL] callee_id={} callee_name={} callee_ty={:?}",
                                callee.identifier.id.0,
                                identifier_debug_name(&callee.identifier),
                                get_type(callee.identifier.id, &id_types)
                            );
                        }
                        InstructionValue::MethodCall {
                            receiver, property, ..
                        } => {
                            eprintln!(
                                "[TYPE_INFER_METHOD] recv_id={} recv_name={} recv_ty={:?} prop_id={} prop_name={} prop_ty={:?}",
                                receiver.identifier.id.0,
                                identifier_debug_name(&receiver.identifier),
                                get_type(receiver.identifier.id, &id_types),
                                property.identifier.id.0,
                                identifier_debug_name(&property.identifier),
                                get_type(property.identifier.id, &id_types)
                            );
                        }
                        _ => {}
                    }
                }
                // Track string literal values for method property resolution
                if let InstructionValue::Primitive {
                    value: PrimitiveValue::String(s),
                    ..
                } = &instr.value
                {
                    id_string_values.insert(lv_id, s.clone());
                }

                // Upstream's unifier can infer array receiver types from method usage
                // (e.g. `data.items.map(...)` implies `data.items` is an Array).
                // Mirror that in this simplified inference by constraining unresolved
                // receiver declarations for array-specific method names.
                if let InstructionValue::MethodCall {
                    receiver, property, ..
                } = &instr.value
                {
                    let method_name = get_name(&property.identifier)
                        .or_else(|| id_names.get(&property.identifier.id).map(String::as_str))
                        .or_else(|| {
                            id_string_values
                                .get(&property.identifier.id)
                                .map(String::as_str)
                        });
                    if let Some(method_name) = method_name
                        && method_name_implies_array_receiver(method_name)
                    {
                        let receiver_ty = get_type(receiver.identifier.id, &id_types);
                        if matches!(receiver_ty, Type::Poly | Type::TypeVar { .. }) {
                            assign_type_to_declaration(
                                receiver.identifier.declaration_id,
                                Type::Object {
                                    shape_id: Some(BUILT_IN_ARRAY_ID.to_string()),
                                },
                                &declaration_ids,
                                &mut id_types,
                            );
                        }
                    }
                }

                // Upstream InferTypes adds equations that constrain binary/update
                // operands to primitive types. Apply the same constraints to improve
                // side-effect analysis and dead-code elimination parity.
                match &instr.value {
                    InstructionValue::BinaryExpression {
                        operator,
                        left,
                        right,
                        ..
                    } => {
                        if is_primitive_binary_op(*operator) {
                            assign_type_to_declaration(
                                left.identifier.declaration_id,
                                Type::Primitive,
                                &declaration_ids,
                                &mut id_types,
                            );
                            assign_type_to_declaration(
                                right.identifier.declaration_id,
                                Type::Primitive,
                                &declaration_ids,
                                &mut id_types,
                            );
                        }
                    }
                    InstructionValue::PrefixUpdate { value, lvalue, .. }
                    | InstructionValue::PostfixUpdate { value, lvalue, .. } => {
                        assign_type_to_declaration(
                            value.identifier.declaration_id,
                            Type::Primitive,
                            &declaration_ids,
                            &mut id_types,
                        );
                        assign_type_to_declaration(
                            lvalue.identifier.declaration_id,
                            Type::Primitive,
                            &declaration_ids,
                            &mut id_types,
                        );
                    }
                    InstructionValue::TypeCastExpression { value, type_, .. } => {
                        if func.env.config().enable_use_type_annotations
                            && !matches!(type_, Type::Poly | Type::TypeVar { .. })
                        {
                            assign_type_to_declaration(
                                value.identifier.declaration_id,
                                type_.clone(),
                                &declaration_ids,
                                &mut id_types,
                            );
                        }
                    }
                    InstructionValue::ObjectExpression { properties, .. } => {
                        // Mirror upstream TypeInference/InferTypes: computed object keys
                        // are constrained to Primitive.
                        for property in properties {
                            if let ObjectPropertyOrSpread::Property(property) = property
                                && let ObjectPropertyKey::Computed(place) = &property.key
                            {
                                assign_type_to_declaration(
                                    place.identifier.declaration_id,
                                    Type::Primitive,
                                    &declaration_ids,
                                    &mut id_types,
                                );
                            }
                        }
                    }
                    _ => {}
                }

                let ty = infer_instruction_type(
                    instr,
                    &id_types,
                    &id_string_values,
                    &id_names,
                    &context_like_declaration_ids,
                    func.env.config(),
                );
                if !matches!(ty, Type::Poly) {
                    if debug_types {
                        eprintln!(
                            "[TYPE_INFER] fn={} pass={} bb{} instr#{} lvalue_id={} inferred={:?}",
                            func.id.as_deref().unwrap_or("<anonymous>"),
                            _pass,
                            block.id.0,
                            instr.id.0,
                            lv_id.0,
                            ty
                        );
                    }
                    id_types.insert(lv_id, ty);
                }
                // StoreLocal unifies target/source with a type-variable unifier.
                // Mirror upstream's per-identifier equations for multi-write
                // declarations so one branch-local concrete type does not smear
                // across sibling SSA versions before a phi resolves.
                if let InstructionValue::StoreLocal { lvalue, value, .. } = &instr.value {
                    let value_ty = get_type(value.identifier.id, &id_types);
                    if !matches!(value_ty, Type::Poly | Type::TypeVar { .. }) {
                        assign_type_to_identifier_or_single_write_declaration(
                            &lvalue.place.identifier,
                            value_ty,
                            &declaration_ids,
                            &store_local_decl_writes,
                            &mut id_types,
                        );
                    }
                    let write_count = store_local_decl_writes
                        .get(&lvalue.place.identifier.declaration_id)
                        .copied()
                        .unwrap_or(0);
                    if write_count <= 1 {
                        let lvalue_ty = get_type(lvalue.place.identifier.id, &id_types);
                        if !matches!(lvalue_ty, Type::Poly | Type::TypeVar { .. }) {
                            assign_type_to_identifier_or_single_write_declaration(
                                &value.identifier,
                                lvalue_ty,
                                &declaration_ids,
                                &store_local_decl_writes,
                                &mut id_types,
                            );
                        }
                    }
                }
                // Upstream only propagates StoreContext for const lvalues.
                if let InstructionValue::StoreContext { lvalue, value, .. } = &instr.value
                    && lvalue.kind == InstructionKind::Const
                {
                    let value_ty = get_type(value.identifier.id, &id_types);
                    if !matches!(value_ty, Type::Poly | Type::TypeVar { .. }) {
                        assign_type_to_declaration(
                            lvalue.place.identifier.declaration_id,
                            value_ty,
                            &declaration_ids,
                            &mut id_types,
                        );
                    }
                }
                // For Destructure of known hook results, assign types to pattern elements.
                if let InstructionValue::Destructure { lvalue, value, .. } = &instr.value {
                    // Mirror upstream InferTypes: array spread elements in a destructuring
                    // pattern always produce arrays, independent of source object shape.
                    if let Pattern::Array(arr) = &lvalue.pattern {
                        for item in &arr.items {
                            if let ArrayElement::Spread(place) = item {
                                assign_type_to_declaration(
                                    place.identifier.declaration_id,
                                    Type::Object {
                                        shape_id: Some(BUILT_IN_ARRAY_ID.to_string()),
                                    },
                                    &declaration_ids,
                                    &mut id_types,
                                );
                            }
                        }
                    }

                    let source_type = get_type(value.identifier.id, &id_types);
                    if let Type::Object {
                        shape_id: Some(ref sid),
                    } = source_type
                    {
                        infer_destructure_pattern_types(sid, &lvalue.pattern, &mut id_types);
                    }
                }
            }
        }

        // Mirror upstream InferTypes return equations for the most stable case:
        // single return: returns = value type.
        //
        // Multi-return functions in this simplified inference can over-constrain
        // recursive / exceptional control-flow compared to upstream unification,
        // so we keep those unconstrained here.
        let mut return_types: Vec<Type> = Vec::new();
        for (_bid, block) in &func.body.blocks {
            if let Terminal::Return { value, .. } = &block.terminal {
                let ty = get_type(value.identifier.id, &id_types);
                if matches!(ty, Type::Poly) {
                    return_types.push(value.identifier.type_.clone());
                } else {
                    return_types.push(ty);
                }
            }
        }
        if return_types.len() == 1
            && let Some(ty) = return_types.pop()
            && !matches!(ty, Type::Poly | Type::TypeVar { .. })
        {
            assign_type_to_declaration(
                func.returns.identifier.declaration_id,
                ty.clone(),
                &declaration_ids,
                &mut id_types,
            );
            if debug_types {
                eprintln!(
                    "[TYPE_INFER_RET] fn={} pass={} single_return={:?}",
                    func.id.as_deref().unwrap_or("<anonymous>"),
                    _pass,
                    ty
                );
            }
        }
    }

    // Phase 2: Apply inferred types to identifiers
    for param in &mut func.params {
        let place = match param {
            Argument::Place(place) | Argument::Spread(place) => place,
        };
        if let Some(ty) = id_types.get(&place.identifier.id) {
            place.identifier.type_ = ty.clone();
        }
    }
    for place in &mut func.context {
        if let Some(ty) = id_types.get(&place.identifier.id) {
            place.identifier.type_ = ty.clone();
        }
    }
    if let Some(ty) = id_types.get(&func.returns.identifier.id) {
        func.returns.identifier.type_ = ty.clone();
    }

    for (_bid, block) in &mut func.body.blocks {
        let apply = |place: &mut Place| {
            if let Some(ty) = id_types.get(&place.identifier.id) {
                place.identifier.type_ = ty.clone();
            }
        };

        for phi in &mut block.phis {
            if let Some(ty) = id_types.get(&phi.place.identifier.id) {
                phi.place.identifier.type_ = ty.clone();
            }
            for op in phi.operands.values_mut() {
                if let Some(ty) = id_types.get(&op.identifier.id) {
                    op.identifier.type_ = ty.clone();
                }
            }
        }
        for instr in &mut block.instructions {
            visitors::map_instruction_lvalues(instr, |place| apply(place));
            visitors::map_instruction_operands(instr, |place| apply(place));
        }
        visitors::map_terminal_operands(&mut block.terminal, |place| apply(place));
    }

    // Re-run nested function inference after this function's captures/locals have
    // been typed, so inner LoadContext/LoadLocal uses see updated capture types.
    for (_bid, block) in &mut func.body.blocks {
        for instr in &mut block.instructions {
            match &mut instr.value {
                InstructionValue::FunctionExpression { lowered_func, .. }
                | InstructionValue::ObjectMethod { lowered_func, .. } => {
                    infer_types(&mut lowered_func.func);
                }
                _ => {}
            }
        }
    }
}

fn collect_captured_context_declarations(func: &HIRFunction) -> HashSet<DeclarationId> {
    fn walk(func: &HIRFunction, out: &mut HashSet<DeclarationId>) {
        for (_bid, block) in &func.body.blocks {
            for instr in &block.instructions {
                match &instr.value {
                    InstructionValue::FunctionExpression { lowered_func, .. }
                    | InstructionValue::ObjectMethod { lowered_func, .. } => {
                        for place in &lowered_func.func.context {
                            out.insert(place.identifier.declaration_id);
                        }
                        walk(&lowered_func.func, out);
                    }
                    _ => {}
                }
            }
        }
    }

    let mut out = HashSet::new();
    walk(func, &mut out);
    out
}

/// Infer the type of a single instruction's result.
fn infer_instruction_type(
    instr: &Instruction,
    known_types: &HashMap<IdentifierId, Type>,
    id_string_values: &HashMap<IdentifierId, String>,
    id_names: &HashMap<IdentifierId, String>,
    context_declaration_ids: &HashSet<DeclarationId>,
    env_config: &crate::options::EnvironmentConfig,
) -> Type {
    match &instr.value {
        // Primitives are always primitive
        InstructionValue::Primitive { .. } => Type::Primitive,

        // Template literals produce strings (primitive)
        InstructionValue::TemplateLiteral { .. } => Type::Primitive,

        // Binary operators: arithmetic, comparison, bitwise all produce primitives
        InstructionValue::BinaryExpression { .. } => Type::Primitive,

        // Unary operators: !, -, +, ~, typeof, void all produce primitives
        InstructionValue::UnaryExpression { .. } => Type::Primitive,

        // Prefix/Postfix update (++/--) produce numbers (primitive)
        InstructionValue::PrefixUpdate { .. } | InstructionValue::PostfixUpdate { .. } => {
            Type::Primitive
        }

        // JSXText produces a string (primitive)
        InstructionValue::JSXText { .. } => Type::Primitive,

        // MetaProperty (import.meta, new.target) — conservatively Poly
        InstructionValue::MetaProperty { .. } => Type::Poly,

        // LoadLocal: propagate from source
        InstructionValue::LoadLocal { place, .. } => {
            if context_declaration_ids.contains(&place.identifier.declaration_id) {
                return Type::Poly;
            }
            if env_config.enable_treat_set_identifiers_as_state_setters
                && let Some(name) = identifier_name(place, id_names)
                && name.starts_with("set")
            {
                return Type::Function {
                    shape_id: Some("BuiltInSetState".to_string()),
                    return_type: Box::new(Type::Poly),
                    is_constructor: false,
                };
            }
            let ty = get_type(place.identifier.id, known_types);
            if matches!(ty, Type::Poly) {
                place.identifier.type_.clone()
            } else {
                ty
            }
        }
        // Upstream context-box modeling primarily relies on StoreContext equations.
        // Our lowering can materialize captured outer values as LoadContext with
        // source identifiers already carrying precise built-in object types
        // (e.g. BuiltInUseRefId). Preserve that precision for the loaded temp.
        InstructionValue::LoadContext { place, .. } => {
            let ty = get_type(place.identifier.id, known_types);
            if matches!(ty, Type::Poly) {
                place.identifier.type_.clone()
            } else {
                ty
            }
        }

        // StoreLocal: propagate from value
        InstructionValue::StoreLocal { value, .. }
        | InstructionValue::StoreContext { value, .. } => {
            let ty = get_type(value.identifier.id, known_types);
            if matches!(ty, Type::Poly) {
                value.identifier.type_.clone()
            } else {
                ty
            }
        }

        // Object/Array/JSX creation -> Object type with built-in shape ids.
        // Upstream uses these shape ids for downstream scope-merging and effect logic.
        InstructionValue::ObjectExpression { .. } => Type::Object {
            shape_id: Some(BUILT_IN_OBJECT_ID.to_string()),
        },
        InstructionValue::ArrayExpression { .. } => Type::Object {
            shape_id: Some(BUILT_IN_ARRAY_ID.to_string()),
        },
        InstructionValue::JsxExpression { .. } | InstructionValue::JsxFragment { .. } => {
            Type::Object {
                shape_id: Some(BUILT_IN_JSX_ID.to_string()),
            }
        }
        InstructionValue::NewExpression { callee, .. } => {
            let callee_type = get_type(callee.identifier.id, known_types);
            if let Type::Function { return_type, .. } = &callee_type
                && !matches!(**return_type, Type::Poly)
            {
                return (**return_type).clone();
            }
            Type::Object { shape_id: None }
        }
        InstructionValue::RegExpLiteral { .. } => Type::Object { shape_id: None },

        // Function expressions -> Function type
        InstructionValue::FunctionExpression { lowered_func, .. } => Type::Function {
            shape_id: Some(BUILT_IN_FUNCTION_ID.to_string()),
            return_type: Box::new(lowered_func.func.returns.identifier.type_.clone()),
            is_constructor: false,
        },
        InstructionValue::ObjectMethod { .. } => Type::ObjectMethod,

        // PropertyLoad: if loading known primitive properties, result is primitive
        InstructionValue::PropertyLoad {
            object, property, ..
        } => {
            if let Some(ty) =
                infer_property_load_type(&get_type(object.identifier.id, known_types), property)
            {
                return ty;
            }

            // Check for ref.current access
            if let PropertyLiteral::String(s) = property {
                if s == "current" {
                    let obj_type = get_type(object.identifier.id, known_types);
                    if matches!(&obj_type, Type::Object { shape_id: Some(sid) } if sid == "BuiltInUseRefId")
                    {
                        return Type::Object {
                            shape_id: Some("BuiltInRefValue".to_string()),
                        };
                    }
                    if env_config.enable_treat_ref_like_identifiers_as_refs
                        && let Some(name) = identifier_name(object, id_names)
                        && is_ref_like_name(&name)
                    {
                        return Type::Object {
                            shape_id: Some("BuiltInRefValue".to_string()),
                        };
                    }
                }
                if s == "length" {
                    return Type::Primitive;
                }
            }
            Type::Poly
        }

        // ComputedLoad: use shape wildcard typing when available.
        InstructionValue::ComputedLoad { object, .. } => {
            infer_computed_load_type(&get_type(object.identifier.id, known_types))
                .unwrap_or(Type::Poly)
        }

        // PropertyStore/ComputedStore/Delete: no meaningful result type
        InstructionValue::PropertyStore { .. }
        | InstructionValue::ComputedStore { .. }
        | InstructionValue::PropertyDelete { .. }
        | InstructionValue::ComputedDelete { .. } => Type::Poly,

        // StoreGlobal: no meaningful result
        InstructionValue::StoreGlobal { .. } => Type::Poly,

        // TypeCast: with `enableUseTypeAnnotations`, cast type constrains result
        // (and through the loop above, also the source declaration).
        InstructionValue::TypeCastExpression { value, type_, .. } => {
            if env_config.enable_use_type_annotations
                && !matches!(type_, Type::Poly | Type::TypeVar { .. })
            {
                return type_.clone();
            }
            get_type(value.identifier.id, known_types)
        }

        // CallExpression: propagate return type from callee's Function type
        InstructionValue::CallExpression { callee, .. } => {
            let callee_type = get_type(callee.identifier.id, known_types);
            if let Type::Function { return_type, .. } = &callee_type {
                // Propagate known return types (Primitive, Object with shape, etc.)
                if !matches!(**return_type, Type::Poly) {
                    return (**return_type).clone();
                }
                // If callee is already known to be a function with unknown return,
                // do not override it with name-based built-in hook fallbacks.
                // This avoids treating shadowed/local hook-like names (e.g. local
                // `useState`) as React built-ins.
                return Type::Poly;
            }
            // Fallback: check callee name for known hook/builtin return types
            if let Some(name) = identifier_name(callee, id_names) {
                if is_primitive_returning_global(&name) {
                    return Type::Primitive;
                }
                if let Some(ty) = hook_return_type_for_name(&name) {
                    return ty;
                }
            }
            Type::Poly
        }

        // MethodCall: check for known hook calls via method syntax.
        // For `React.useState(...)` etc., the property is a temporary backed by
        // `Primitive::String("useState")`. We resolve the string value via
        // `id_string_values` to detect hook method calls.
        //
        // NOTE: We intentionally limit `id_string_values` lookup to hook
        // detection only. Using it for `is_primitive_returning_method` would
        // change scope merging for cases like `x.toFixed()` where upstream
        // does NOT infer primitive return types in this simplified inference.
        InstructionValue::MethodCall {
            receiver, property, ..
        } => {
            // Upstream InferTypes constrains MethodCall by the property function type:
            //   property: Function<return=R>, lvalue: R
            // Mirror that first so return-shape information from PropertyLoad
            // (e.g. React.useRef -> BuiltInUseRefId) propagates through calls.
            let property_type = get_type(property.identifier.id, known_types);
            if let Type::Function { return_type, .. } = &property_type
                && !matches!(**return_type, Type::Poly)
            {
                return (**return_type).clone();
            }

            if let Some(method_name) = get_name(&property.identifier).or_else(|| {
                id_string_values
                    .get(&property.identifier.id)
                    .map(String::as_str)
            }) && let Some(return_ty) = infer_method_return_type(
                &get_type(receiver.identifier.id, known_types),
                method_name,
            ) {
                return return_ty;
            }

            // Named property check for primitive-returning methods (rare path)
            if let Some(name) = get_name(&property.identifier) {
                if is_primitive_returning_method(name) {
                    return Type::Primitive;
                }
                if let Some(ty) = hook_return_type_for_name(name) {
                    return ty;
                }
            }
            // Resolve lowered string temporaries for hook detection only
            if let Some(prop_str) = id_string_values.get(&property.identifier.id)
                && let Some(ty) = hook_return_type_for_name(prop_str)
            {
                return ty;
            }
            Type::Poly
        }

        // Destructure: result depends on source, conservatively Poly.
        // Pattern element types are inferred separately in the main loop.
        InstructionValue::Destructure { .. } => Type::Poly,

        // LoadGlobal: check if it's a known type
        InstructionValue::LoadGlobal { binding, .. } => {
            if let Some(ty) = infer_module_import_type(binding, env_config) {
                return ty;
            }

            let react_import_hook_fallback = match binding {
                NonLocalBinding::ImportSpecifier {
                    module,
                    imported,
                    name,
                } if module == "react" => is_hook_like_name(imported) || is_hook_like_name(name),
                _ => false,
            };

            if let Some(name) = known_global_name_for_binding(binding) {
                let globals = GlobalRegistry::new();
                if let Some(global) = globals.get_global(name) {
                    match &global.kind {
                        GlobalKind::Primitive => return Type::Primitive,
                        GlobalKind::Object { shape_id } => {
                            return Type::Object {
                                shape_id: Some((*shape_id).to_string()),
                            };
                        }
                        GlobalKind::Function(signature) => {
                            // Preserve constructor and return-shape information from the
                            // global registry (e.g. Map -> BuiltInMap), which feeds
                            // downstream aliasing/mutation signatures.
                            let is_constructor = matches!(
                                name,
                                "Array"
                                    | "Object"
                                    | "Map"
                                    | "Set"
                                    | "WeakMap"
                                    | "WeakSet"
                                    | "Promise"
                                    | "Error"
                                    | "RegExp"
                                    | "Date"
                            );
                            return Type::Function {
                                shape_id: None,
                                return_type: Box::new(return_type_to_hir_type(
                                    &signature.return_type,
                                )),
                                is_constructor,
                            };
                        }
                        GlobalKind::Hook(_) | GlobalKind::Poly => {}
                    }
                }

                let ty = match name {
                    // Known primitive-valued globals
                    "undefined" | "NaN" | "Infinity" => Type::Primitive,
                    // Known constructor functions
                    "Array" | "Object" | "Map" | "Set" | "WeakMap" | "WeakSet" | "Promise"
                    | "Error" | "RegExp" | "Date" => Type::Function {
                        shape_id: None,
                        return_type: Box::new(Type::Object { shape_id: None }),
                        is_constructor: true,
                    },
                    // Known primitive-returning functions
                    "Number" | "String" | "Boolean" | "parseInt" | "parseFloat" | "isNaN"
                    | "isFinite" => Type::Function {
                        shape_id: None,
                        return_type: Box::new(Type::Primitive),
                        is_constructor: false,
                    },
                    // Hooks that return ref objects
                    "useRef" | "createRef" => Type::Function {
                        shape_id: Some("BuiltInUseRefHookId".to_string()),
                        return_type: Box::new(Type::Object {
                            shape_id: Some("BuiltInUseRefId".to_string()),
                        }),
                        is_constructor: false,
                    },
                    // Hooks that return frozen state
                    "useState" => Type::Function {
                        shape_id: Some("BuiltInUseStateHookId".to_string()),
                        return_type: Box::new(Type::Object {
                            shape_id: Some("BuiltInUseStateHookResult".to_string()),
                        }),
                        is_constructor: false,
                    },
                    "useReducer" => Type::Function {
                        shape_id: Some("BuiltInUseReducerHookId".to_string()),
                        return_type: Box::new(Type::Object {
                            shape_id: Some("BuiltInUseReducerHookResult".to_string()),
                        }),
                        is_constructor: false,
                    },
                    "useContext" => Type::Function {
                        shape_id: Some("BuiltInUseContextHookId".to_string()),
                        return_type: Box::new(Type::Object { shape_id: None }),
                        is_constructor: false,
                    },
                    "useMemo" => Type::Function {
                        shape_id: Some("BuiltInUseMemoHookId".to_string()),
                        return_type: Box::new(Type::Poly),
                        is_constructor: false,
                    },
                    "useCallback" => Type::Function {
                        shape_id: Some("BuiltInUseCallbackHookId".to_string()),
                        return_type: Box::new(Type::Poly),
                        is_constructor: false,
                    },
                    "useEffect" | "useLayoutEffect" | "useInsertionEffect" => Type::Function {
                        shape_id: Some("BuiltInUseEffectHookId".to_string()),
                        return_type: Box::new(Type::Poly),
                        is_constructor: false,
                    },
                    "fire" => Type::Function {
                        shape_id: Some("BuiltInFire".to_string()),
                        return_type: Box::new(Type::Function {
                            shape_id: Some("BuiltInFireFunction".to_string()),
                            return_type: Box::new(Type::Poly),
                            is_constructor: false,
                        }),
                        is_constructor: false,
                    },
                    "useTransition" => Type::Function {
                        shape_id: Some("BuiltInUseTransitionHookId".to_string()),
                        return_type: Box::new(Type::Object {
                            shape_id: Some("BuiltInUseTransitionHookResult".to_string()),
                        }),
                        is_constructor: false,
                    },
                    "useImperativeHandle" => Type::Function {
                        shape_id: Some("BuiltInUseImperativeHandleHookId".to_string()),
                        return_type: Box::new(Type::Poly),
                        is_constructor: false,
                    },
                    "useActionState" => Type::Function {
                        shape_id: Some("BuiltInUseActionStateHookId".to_string()),
                        return_type: Box::new(Type::Object {
                            shape_id: Some("BuiltInUseActionStateHookResult".to_string()),
                        }),
                        is_constructor: false,
                    },
                    // Shared-runtime test hooks/functions used in upstream fixture harness.
                    "useFragment" => Type::Function {
                        shape_id: Some(TEST_SHARED_RUNTIME_USE_FRAGMENT_HOOK_ID.to_string()),
                        return_type: Box::new(Type::Object {
                            shape_id: Some(BUILT_IN_MIXED_READONLY_ID.to_string()),
                        }),
                        is_constructor: false,
                    },
                    "useNoAlias" => Type::Function {
                        shape_id: Some(TEST_SHARED_RUNTIME_USE_NO_ALIAS_HOOK_ID.to_string()),
                        return_type: Box::new(Type::Poly),
                        is_constructor: false,
                    },
                    "useFreeze" => Type::Function {
                        shape_id: Some(TEST_SHARED_RUNTIME_USE_FREEZE_HOOK_ID.to_string()),
                        return_type: Box::new(Type::Poly),
                        is_constructor: false,
                    },
                    // Any unknown hook (use* pattern) -- default custom hook shape
                    // depends on `enableAssumeHooksFollowRulesOfReact`.
                    name if is_hook_like_name(name) => default_hook_type(env_config),
                    _ => Type::Poly,
                };
                // Upstream Environment.getGlobalDeclaration behavior for
                // `import {foo as useBar} from 'react'`: when `foo` is unknown
                // but either imported/local name is hook-like, infer custom hook.
                if matches!(ty, Type::Poly) && react_import_hook_fallback {
                    return default_hook_type(env_config);
                }
                return ty;
            }

            // Module-local and non-react imports should not map to built-in globals,
            // but use* names are still treated as custom hooks.

            if is_hook_like_name(binding.name()) {
                default_hook_type(env_config)
            } else {
                Type::Poly
            }
        }

        // Tagged template: propagate return type from tag function when known.
        InstructionValue::TaggedTemplateExpression { tag, .. } => {
            let tag_type = get_type(tag.identifier.id, known_types);
            if let Type::Function { return_type, .. } = &tag_type {
                if !matches!(**return_type, Type::Poly) {
                    return (**return_type).clone();
                }
                return Type::Poly;
            }
            if let Some(name) = identifier_name(tag, id_names) {
                if is_primitive_returning_global(&name) {
                    return Type::Primitive;
                }
                if let Some(ty) = hook_return_type_for_name(&name) {
                    return ty;
                }
            }
            Type::Poly
        }

        // Declare: no value, Poly
        InstructionValue::DeclareLocal { .. } | InstructionValue::DeclareContext { .. } => {
            Type::Poly
        }

        // Iterator operations
        InstructionValue::GetIterator { .. } => Type::Object { shape_id: None },
        InstructionValue::IteratorNext { .. } => Type::Poly,
        InstructionValue::NextPropertyOf { .. } => Type::Primitive,

        // Await: result type unknown
        InstructionValue::Await { .. } => Type::Poly,

        // Debugger: no meaningful result
        InstructionValue::Debugger { .. } => Type::Poly,

        // StartMemoize/FinishMemoize: no meaningful type
        InstructionValue::StartMemoize { .. } | InstructionValue::FinishMemoize { .. } => {
            Type::Poly
        }

        // Ternary: if both branches are primitive, result is primitive
        InstructionValue::Ternary {
            consequent,
            alternate,
            ..
        } => {
            let c_type = get_type(consequent.identifier.id, known_types);
            let a_type = get_type(alternate.identifier.id, known_types);
            if matches!(c_type, Type::Primitive) && matches!(a_type, Type::Primitive) {
                Type::Primitive
            } else {
                Type::Poly
            }
        }

        // Logical: if both sides are primitive, result is primitive
        InstructionValue::LogicalExpression { left, right, .. } => {
            let l_type = get_type(left.identifier.id, known_types);
            let r_type = get_type(right.identifier.id, known_types);
            if matches!(l_type, Type::Primitive) && matches!(r_type, Type::Primitive) {
                Type::Primitive
            } else {
                Type::Poly
            }
        }
        InstructionValue::ReactiveSequenceExpression { .. }
        | InstructionValue::ReactiveOptionalExpression { .. }
        | InstructionValue::ReactiveLogicalExpression { .. }
        | InstructionValue::ReactiveConditionalExpression { .. } => Type::Poly,
    }
}

/// Infer types for pattern elements in Destructure of known hook results.
///
/// This mirrors upstream's tuple-property equation walk behavior: when an
/// unsupported array element (e.g. elision/hole) appears, later elements are
/// not inferred.
fn infer_destructure_pattern_types(
    source_shape_id: &str,
    pattern: &Pattern,
    id_types: &mut HashMap<IdentifierId, Type>,
) {
    match source_shape_id {
        "BuiltInUseState"
        | "BuiltInUseReducer"
        | "BuiltInUseStateHookResult"
        | "BuiltInUseReducerHookResult" => {
            if let Pattern::Array(arr) = pattern
                && matches!(arr.items.first(), Some(ArrayElement::Place(_)))
                && let Some(ArrayElement::Place(p)) = arr.items.get(1)
            {
                id_types.insert(
                    p.identifier.id,
                    Type::Function {
                        shape_id: Some("BuiltInSetState".to_string()),
                        return_type: Box::new(Type::Poly),
                        is_constructor: false,
                    },
                );
            }
        }
        "BuiltInUseActionState" | "BuiltInUseActionStateHookResult" => {
            if let Pattern::Array(arr) = pattern {
                if matches!(arr.items.first(), Some(ArrayElement::Place(_)))
                    && let Some(ArrayElement::Place(p)) = arr.items.get(1)
                {
                    id_types.insert(
                        p.identifier.id,
                        Type::Function {
                            shape_id: Some("BuiltInSetActionState".to_string()),
                            return_type: Box::new(Type::Poly),
                            is_constructor: false,
                        },
                    );
                }
                if matches!(arr.items.first(), Some(ArrayElement::Place(_)))
                    && matches!(arr.items.get(1), Some(ArrayElement::Place(_)))
                    && let Some(ArrayElement::Place(p)) = arr.items.get(2)
                {
                    id_types.insert(p.identifier.id, Type::Primitive);
                }
            }
        }
        "BuiltInUseTransition" | "BuiltInUseTransitionHookResult" => {
            if let Pattern::Array(arr) = pattern
                && let Some(ArrayElement::Place(first)) = arr.items.first()
            {
                id_types.insert(first.identifier.id, Type::Primitive);
                if let Some(ArrayElement::Place(p)) = arr.items.get(1) {
                    id_types.insert(
                        p.identifier.id,
                        Type::Function {
                            shape_id: Some("BuiltInStartTransition".to_string()),
                            return_type: Box::new(Type::Poly),
                            is_constructor: false,
                        },
                    );
                }
            }
        }
        _ => {
            if let Pattern::Object(obj) = pattern {
                for prop in &obj.properties {
                    if let ObjectPropertyOrSpread::Property(p) = prop {
                        let key = match &p.key {
                            ObjectPropertyKey::String(s) | ObjectPropertyKey::Identifier(s) => {
                                Some(s.as_str())
                            }
                            ObjectPropertyKey::Number(_) | ObjectPropertyKey::Computed(_) => None,
                        };
                        if let Some(key) = key
                            && let Some(property_ty) =
                                infer_property_type_for_shape(source_shape_id, key)
                        {
                            id_types.insert(p.place.identifier.id, property_ty);
                        }
                    }
                }
            }
        }
    }
}

/// Get the known type for an identifier, falling back to Poly.
fn get_type(id: IdentifierId, known: &HashMap<IdentifierId, Type>) -> Type {
    known.get(&id).cloned().unwrap_or(Type::Poly)
}

/// Get the name of an identifier if it has one.
fn get_name(ident: &Identifier) -> Option<&str> {
    match &ident.name {
        Some(IdentifierName::Named(name)) => Some(name.as_str()),
        _ => None,
    }
}

fn identifier_name(place: &Place, id_names: &HashMap<IdentifierId, String>) -> Option<String> {
    id_names
        .get(&place.identifier.id)
        .cloned()
        .or_else(|| get_name(&place.identifier).map(|name| name.to_string()))
}

fn assign_type_to_identifier_or_single_write_declaration(
    identifier: &Identifier,
    ty: Type,
    declaration_ids: &HashMap<DeclarationId, Vec<IdentifierId>>,
    store_local_decl_writes: &HashMap<DeclarationId, usize>,
    id_types: &mut HashMap<IdentifierId, Type>,
) {
    if store_local_decl_writes
        .get(&identifier.declaration_id)
        .copied()
        .unwrap_or(0)
        <= 1
    {
        assign_type_to_declaration(identifier.declaration_id, ty, declaration_ids, id_types);
        return;
    }

    assign_type_to_identifier(identifier.id, ty, id_types);
}

fn assign_type_to_identifier(
    identifier_id: IdentifierId,
    ty: Type,
    id_types: &mut HashMap<IdentifierId, Type>,
) {
    let merged = merge_known_types(
        id_types
            .get(&identifier_id)
            .cloned()
            .and_then(normalize_known_type),
        normalize_known_type(ty),
    );
    if !matches!(merged, Type::Poly | Type::TypeVar { .. }) {
        id_types.insert(identifier_id, merged);
    }
}

fn assign_type_to_declaration(
    declaration_id: DeclarationId,
    ty: Type,
    declaration_ids: &HashMap<DeclarationId, Vec<IdentifierId>>,
    id_types: &mut HashMap<IdentifierId, Type>,
) {
    if let Some(ids) = declaration_ids.get(&declaration_id) {
        let mut merged = normalize_known_type(ty);
        for id in ids {
            merged = Some(merge_known_types(merged, id_types.get(id).cloned()));
        }
        let merged = merged.unwrap_or(Type::Poly);
        for id in ids {
            id_types.insert(*id, merged.clone());
        }
    }
}

fn normalize_known_type(ty: Type) -> Option<Type> {
    match ty {
        Type::Poly | Type::TypeVar { .. } => None,
        Type::Phi { operands } => {
            let mut flattened = Vec::new();
            for operand in operands {
                if let Some(operand) = normalize_known_type(operand) {
                    push_unique_type(&mut flattened, operand);
                }
            }
            match flattened.len() {
                0 => None,
                1 => flattened.into_iter().next(),
                _ => Some(Type::Phi {
                    operands: flattened,
                }),
            }
        }
        other => Some(other),
    }
}

fn merge_known_types(current: Option<Type>, incoming: Option<Type>) -> Type {
    match (current, incoming) {
        (Some(current), None) | (None, Some(current)) => current,
        (None, None) => Type::Poly,
        (Some(current), Some(incoming)) => {
            if current == incoming {
                return current;
            }

            if let Some(union) = try_union_types(&current, &incoming) {
                return union;
            }

            let mut operands = Vec::new();
            append_type_operands(&mut operands, current);
            append_type_operands(&mut operands, incoming);
            match operands.len() {
                0 => Type::Poly,
                1 => operands.into_iter().next().unwrap_or(Type::Poly),
                _ => Type::Phi { operands },
            }
        }
    }
}

fn append_type_operands(operands: &mut Vec<Type>, ty: Type) {
    match ty {
        Type::Phi { operands: nested } => {
            for operand in nested {
                if let Some(operand) = normalize_known_type(operand) {
                    push_unique_type(operands, operand);
                }
            }
        }
        other => push_unique_type(operands, other),
    }
}

fn push_unique_type(operands: &mut Vec<Type>, ty: Type) {
    if !operands.contains(&ty) {
        operands.push(ty);
    }
}

fn try_union_types(ty1: &Type, ty2: &Type) -> Option<Type> {
    let (readonly_type, other_type) = match (ty1, ty2) {
        (
            Type::Object {
                shape_id: Some(shape_id),
            },
            other,
        ) if shape_id == BUILT_IN_MIXED_READONLY_ID => (ty1, other),
        (
            other,
            Type::Object {
                shape_id: Some(shape_id),
            },
        ) if shape_id == BUILT_IN_MIXED_READONLY_ID => (ty2, other),
        _ => return None,
    };

    match other_type {
        Type::Primitive => Some(readonly_type.clone()),
        Type::Object {
            shape_id: Some(shape_id),
        } if shape_id == BUILT_IN_ARRAY_ID => Some(other_type.clone()),
        _ => None,
    }
}

fn is_ref_like_name(name: &str) -> bool {
    name == "ref" || name.ends_with("Ref")
}

fn identifier_debug_name(ident: &Identifier) -> String {
    get_name(ident).unwrap_or("<anon>").to_string()
}

fn instruction_value_kind(value: &InstructionValue) -> &'static str {
    match value {
        InstructionValue::Primitive { .. } => "Primitive",
        InstructionValue::TemplateLiteral { .. } => "TemplateLiteral",
        InstructionValue::BinaryExpression { .. } => "BinaryExpression",
        InstructionValue::UnaryExpression { .. } => "UnaryExpression",
        InstructionValue::PrefixUpdate { .. } => "PrefixUpdate",
        InstructionValue::PostfixUpdate { .. } => "PostfixUpdate",
        InstructionValue::JSXText { .. } => "JSXText",
        InstructionValue::MetaProperty { .. } => "MetaProperty",
        InstructionValue::LoadLocal { .. } => "LoadLocal",
        InstructionValue::LoadContext { .. } => "LoadContext",
        InstructionValue::StoreLocal { .. } => "StoreLocal",
        InstructionValue::StoreContext { .. } => "StoreContext",
        InstructionValue::ObjectExpression { .. } => "ObjectExpression",
        InstructionValue::ArrayExpression { .. } => "ArrayExpression",
        InstructionValue::JsxExpression { .. } => "JsxExpression",
        InstructionValue::JsxFragment { .. } => "JsxFragment",
        InstructionValue::NewExpression { .. } => "NewExpression",
        InstructionValue::RegExpLiteral { .. } => "RegExpLiteral",
        InstructionValue::FunctionExpression { .. } => "FunctionExpression",
        InstructionValue::ObjectMethod { .. } => "ObjectMethod",
        InstructionValue::PropertyLoad { .. } => "PropertyLoad",
        InstructionValue::ComputedLoad { .. } => "ComputedLoad",
        InstructionValue::PropertyStore { .. } => "PropertyStore",
        InstructionValue::ComputedStore { .. } => "ComputedStore",
        InstructionValue::PropertyDelete { .. } => "PropertyDelete",
        InstructionValue::ComputedDelete { .. } => "ComputedDelete",
        InstructionValue::StoreGlobal { .. } => "StoreGlobal",
        InstructionValue::TypeCastExpression { .. } => "TypeCastExpression",
        InstructionValue::CallExpression { .. } => "CallExpression",
        InstructionValue::MethodCall { .. } => "MethodCall",
        InstructionValue::Destructure { .. } => "Destructure",
        InstructionValue::LoadGlobal { .. } => "LoadGlobal",
        InstructionValue::TaggedTemplateExpression { .. } => "TaggedTemplateExpression",
        InstructionValue::DeclareLocal { .. } => "DeclareLocal",
        InstructionValue::DeclareContext { .. } => "DeclareContext",
        InstructionValue::GetIterator { .. } => "GetIterator",
        InstructionValue::IteratorNext { .. } => "IteratorNext",
        InstructionValue::NextPropertyOf { .. } => "NextPropertyOf",
        InstructionValue::Await { .. } => "Await",
        InstructionValue::Debugger { .. } => "Debugger",
        InstructionValue::StartMemoize { .. } => "StartMemoize",
        InstructionValue::FinishMemoize { .. } => "FinishMemoize",
        InstructionValue::Ternary { .. } => "Ternary",
        InstructionValue::LogicalExpression { .. } => "LogicalExpression",
        InstructionValue::ReactiveSequenceExpression { .. } => "ReactiveSequenceExpression",
        InstructionValue::ReactiveOptionalExpression { .. } => "ReactiveOptionalExpression",
        InstructionValue::ReactiveLogicalExpression { .. } => "ReactiveLogicalExpression",
        InstructionValue::ReactiveConditionalExpression { .. } => "ReactiveConditionalExpression",
    }
}

fn known_global_name_for_binding(binding: &NonLocalBinding) -> Option<&str> {
    match binding {
        NonLocalBinding::Global { name } => Some(name.as_str()),
        NonLocalBinding::ImportSpecifier {
            module, imported, ..
        } => {
            if module == "react" {
                Some(imported.as_str())
            } else {
                None
            }
        }
        NonLocalBinding::ImportDefault { module, .. }
        | NonLocalBinding::ImportNamespace { module, .. } => {
            if module == "react" {
                Some("React")
            } else {
                None
            }
        }
        NonLocalBinding::ModuleLocal { .. } => None,
    }
}

fn infer_module_import_type(
    binding: &NonLocalBinding,
    env_config: &crate::options::EnvironmentConfig,
) -> Option<Type> {
    if env_config.enable_custom_type_definition_for_reanimated {
        match binding {
            NonLocalBinding::ImportSpecifier {
                module, imported, ..
            } if module == "react-native-reanimated" => {
                if let Some(ty) = infer_reanimated_import_type(imported) {
                    return Some(ty);
                }
            }
            NonLocalBinding::ImportNamespace { module, .. }
                if module == "react-native-reanimated" =>
            {
                return Some(Type::Object {
                    shape_id: Some(REANIMATED_MODULE_ID.to_string()),
                });
            }
            _ => {}
        }
    }

    match binding {
        NonLocalBinding::ImportSpecifier {
            module, imported, ..
        } if module == "shared-runtime" => match imported.as_str() {
            "graphql" => Some(Type::Function {
                shape_id: Some(TEST_SHARED_RUNTIME_GRAPHQL_FN_ID.to_string()),
                return_type: Box::new(Type::Primitive),
                is_constructor: false,
            }),
            "typedArrayPush" => Some(Type::Function {
                shape_id: Some(TEST_SHARED_RUNTIME_TYPED_ARRAY_PUSH_FN_ID.to_string()),
                return_type: Box::new(Type::Primitive),
                is_constructor: false,
            }),
            "typedLog" => Some(Type::Function {
                shape_id: Some(TEST_SHARED_RUNTIME_TYPED_LOG_FN_ID.to_string()),
                return_type: Box::new(Type::Primitive),
                is_constructor: false,
            }),
            "typedIdentity" => Some(Type::Function {
                shape_id: Some(TEST_SHARED_RUNTIME_TYPED_IDENTITY_FN_ID.to_string()),
                return_type: Box::new(Type::Poly),
                is_constructor: false,
            }),
            "typedAssign" => Some(Type::Function {
                shape_id: Some(TEST_SHARED_RUNTIME_TYPED_ASSIGN_FN_ID.to_string()),
                return_type: Box::new(Type::Poly),
                is_constructor: false,
            }),
            "typedAlias" => Some(Type::Function {
                shape_id: Some(TEST_SHARED_RUNTIME_TYPED_ALIAS_FN_ID.to_string()),
                return_type: Box::new(Type::Poly),
                is_constructor: false,
            }),
            "typedCapture" => Some(Type::Function {
                shape_id: Some(TEST_SHARED_RUNTIME_TYPED_CAPTURE_FN_ID.to_string()),
                return_type: Box::new(Type::Object {
                    shape_id: Some(BUILT_IN_ARRAY_ID.to_string()),
                }),
                is_constructor: false,
            }),
            "typedCreateFrom" => Some(Type::Function {
                shape_id: Some(TEST_SHARED_RUNTIME_TYPED_CREATE_FROM_FN_ID.to_string()),
                return_type: Box::new(Type::Poly),
                is_constructor: false,
            }),
            "typedMutate" => Some(Type::Function {
                shape_id: Some(TEST_SHARED_RUNTIME_TYPED_MUTATE_FN_ID.to_string()),
                return_type: Box::new(Type::Primitive),
                is_constructor: false,
            }),
            "useFragment" => Some(Type::Function {
                shape_id: Some(TEST_SHARED_RUNTIME_USE_FRAGMENT_HOOK_ID.to_string()),
                return_type: Box::new(Type::Object {
                    shape_id: Some(BUILT_IN_MIXED_READONLY_ID.to_string()),
                }),
                is_constructor: false,
            }),
            "useNoAlias" => Some(Type::Function {
                shape_id: Some(TEST_SHARED_RUNTIME_USE_NO_ALIAS_HOOK_ID.to_string()),
                return_type: Box::new(Type::Poly),
                is_constructor: false,
            }),
            "useFreeze" => Some(Type::Function {
                shape_id: Some(TEST_SHARED_RUNTIME_USE_FREEZE_HOOK_ID.to_string()),
                return_type: Box::new(Type::Poly),
                is_constructor: false,
            }),
            _ => None,
        },
        NonLocalBinding::ImportSpecifier {
            module, imported, ..
        } if module == "ReactCompilerKnownIncompatibleTest" => match imported.as_str() {
            "useKnownIncompatible" => Some(Type::Function {
                shape_id: Some(TEST_KNOWN_INCOMPATIBLE_HOOK_ID.to_string()),
                return_type: Box::new(Type::Poly),
                is_constructor: false,
            }),
            "useKnownIncompatibleIndirect" => Some(Type::Function {
                shape_id: Some(TEST_KNOWN_INCOMPATIBLE_INDIRECT_HOOK_ID.to_string()),
                return_type: Box::new(Type::Object {
                    shape_id: Some(TEST_KNOWN_INCOMPATIBLE_INDIRECT_RESULT_ID.to_string()),
                }),
                is_constructor: false,
            }),
            "knownIncompatible" => Some(Type::Function {
                shape_id: Some(TEST_KNOWN_INCOMPATIBLE_FUNCTION_ID.to_string()),
                return_type: Box::new(Type::Poly),
                is_constructor: false,
            }),
            _ => None,
        },
        NonLocalBinding::ImportSpecifier {
            module, imported, ..
        } if module == "ReactCompilerTest" => match imported.as_str() {
            // Intentionally invalid type-provider mapping used by upstream tests.
            "useHookNotTypedAsHook" => Some(Type::Poly),
            "notAhookTypedAsHook" => Some(Type::Function {
                shape_id: Some(TEST_INVALID_TYPE_PROVIDER_NON_HOOK_TYPED_AS_HOOK_ID.to_string()),
                return_type: Box::new(Type::Poly),
                is_constructor: false,
            }),
            _ => None,
        },
        NonLocalBinding::ImportSpecifier {
            module, imported, ..
        } if module == "react-compiler-runtime" => match imported.as_str() {
            // LowerContextAccess helper is semantically a useContext hook call.
            "useContext_withSelector" => Some(Type::Function {
                shape_id: Some("BuiltInUseContextHookId".to_string()),
                return_type: Box::new(Type::Object { shape_id: None }),
                is_constructor: false,
            }),
            _ => None,
        },
        NonLocalBinding::ImportDefault { module, .. } if module == "shared-runtime" => {
            Some(Type::Function {
                shape_id: Some(TEST_SHARED_RUNTIME_TYPED_LOG_FN_ID.to_string()),
                return_type: Box::new(Type::Primitive),
                is_constructor: false,
            })
        }
        NonLocalBinding::ImportNamespace { module, .. } if module == "shared-runtime" => {
            Some(Type::Object {
                shape_id: Some(TEST_SHARED_RUNTIME_MODULE_ID.to_string()),
            })
        }
        NonLocalBinding::ImportDefault { module, .. }
            if module == "useDefaultExportNotTypedAsHook" =>
        {
            // Intentionally invalid type-provider mapping used by upstream tests.
            Some(Type::Poly)
        }
        NonLocalBinding::ImportNamespace { module, .. } if module == "ReactCompilerTest" => {
            Some(Type::Object {
                shape_id: Some(TEST_INVALID_TYPE_PROVIDER_MODULE_ID.to_string()),
            })
        }
        NonLocalBinding::ImportNamespace { module, .. }
            if module == "ReactCompilerKnownIncompatibleTest" =>
        {
            Some(Type::Object {
                shape_id: Some(TEST_KNOWN_INCOMPATIBLE_MODULE_ID.to_string()),
            })
        }
        _ => None,
    }
}

fn infer_reanimated_import_type(imported: &str) -> Option<Type> {
    let function = |shape_id: &str, return_type: Type| Type::Function {
        shape_id: Some(shape_id.to_string()),
        return_type: Box::new(return_type),
        is_constructor: false,
    };

    if [
        "useFrameCallback",
        "useAnimatedStyle",
        "useAnimatedProps",
        "useAnimatedScrollHandler",
        "useAnimatedReaction",
        "useWorkletCallback",
    ]
    .contains(&imported)
    {
        return Some(function(REANIMATED_FROZEN_HOOK_ID, Type::Poly));
    }

    if ["useSharedValue", "useDerivedValue"].contains(&imported) {
        return Some(function(
            REANIMATED_MUTABLE_HOOK_ID,
            Type::Object {
                shape_id: Some(REANIMATED_SHARED_VALUE_ID.to_string()),
            },
        ));
    }

    if [
        "withTiming",
        "withSpring",
        "createAnimatedPropAdapter",
        "withDecay",
        "withRepeat",
        "runOnUI",
        "executeOnUIRuntimeSync",
    ]
    .contains(&imported)
    {
        return Some(function(REANIMATED_MUTABLE_FUNCTION_ID, Type::Poly));
    }

    None
}

fn infer_property_load_type(object_type: &Type, property: &PropertyLiteral) -> Option<Type> {
    let PropertyLiteral::String(property_name) = property else {
        return None;
    };

    let shape_id = match object_type {
        Type::Object {
            shape_id: Some(shape_id),
        } => shape_id.as_str(),
        Type::Function {
            shape_id: Some(shape_id),
            ..
        } => shape_id.as_str(),
        _ => return None,
    };

    infer_property_type_for_shape(shape_id, property_name)
}

fn infer_computed_load_type(object_type: &Type) -> Option<Type> {
    let shape_id = match object_type {
        Type::Object {
            shape_id: Some(shape_id),
        } => shape_id.as_str(),
        Type::Function {
            shape_id: Some(shape_id),
            ..
        } => shape_id.as_str(),
        _ => return None,
    };

    let globals = GlobalRegistry::new();
    let property_type = globals.shapes.get_property(shape_id, "*")?;
    match property_type {
        PropertyType::Primitive => Some(Type::Primitive),
        PropertyType::Object { shape_id } => Some(Type::Object {
            shape_id: Some((*shape_id).to_string()),
        }),
        PropertyType::Function(signature) => Some(Type::Function {
            shape_id: None,
            return_type: Box::new(return_type_to_hir_type(&signature.return_type)),
            is_constructor: false,
        }),
        PropertyType::Poly => Some(Type::Poly),
    }
}

fn infer_property_type_for_shape(shape_id: &str, property_name: &str) -> Option<Type> {
    match (shape_id, property_name) {
        (TEST_SHARED_RUNTIME_MODULE_ID, "useFragment") => {
            return Some(Type::Function {
                shape_id: Some(TEST_SHARED_RUNTIME_USE_FRAGMENT_HOOK_ID.to_string()),
                return_type: Box::new(Type::Object {
                    shape_id: Some(BUILT_IN_MIXED_READONLY_ID.to_string()),
                }),
                is_constructor: false,
            });
        }
        (TEST_SHARED_RUNTIME_MODULE_ID, "useNoAlias") => {
            return Some(Type::Function {
                shape_id: Some(TEST_SHARED_RUNTIME_USE_NO_ALIAS_HOOK_ID.to_string()),
                return_type: Box::new(Type::Poly),
                is_constructor: false,
            });
        }
        (TEST_SHARED_RUNTIME_MODULE_ID, "useFreeze") => {
            return Some(Type::Function {
                shape_id: Some(TEST_SHARED_RUNTIME_USE_FREEZE_HOOK_ID.to_string()),
                return_type: Box::new(Type::Poly),
                is_constructor: false,
            });
        }
        (TEST_SHARED_RUNTIME_MODULE_ID, "graphql") | (TEST_SHARED_RUNTIME_MODULE_ID, "default") => {
            return Some(Type::Function {
                shape_id: Some(TEST_SHARED_RUNTIME_GRAPHQL_FN_ID.to_string()),
                return_type: Box::new(Type::Primitive),
                is_constructor: false,
            });
        }
        (TEST_KNOWN_INCOMPATIBLE_MODULE_ID, "useKnownIncompatible") => {
            return Some(Type::Function {
                shape_id: Some(TEST_KNOWN_INCOMPATIBLE_HOOK_ID.to_string()),
                return_type: Box::new(Type::Poly),
                is_constructor: false,
            });
        }
        (TEST_KNOWN_INCOMPATIBLE_MODULE_ID, "useKnownIncompatibleIndirect") => {
            return Some(Type::Function {
                shape_id: Some(TEST_KNOWN_INCOMPATIBLE_INDIRECT_HOOK_ID.to_string()),
                return_type: Box::new(Type::Object {
                    shape_id: Some(TEST_KNOWN_INCOMPATIBLE_INDIRECT_RESULT_ID.to_string()),
                }),
                is_constructor: false,
            });
        }
        (TEST_KNOWN_INCOMPATIBLE_MODULE_ID, "knownIncompatible") => {
            return Some(Type::Function {
                shape_id: Some(TEST_KNOWN_INCOMPATIBLE_FUNCTION_ID.to_string()),
                return_type: Box::new(Type::Poly),
                is_constructor: false,
            });
        }
        (TEST_KNOWN_INCOMPATIBLE_INDIRECT_RESULT_ID, "incompatible") => {
            return Some(Type::Function {
                shape_id: Some(TEST_KNOWN_INCOMPATIBLE_INDIRECT_FUNCTION_ID.to_string()),
                return_type: Box::new(Type::Poly),
                is_constructor: false,
            });
        }
        (TEST_INVALID_TYPE_PROVIDER_MODULE_ID, "useHookNotTypedAsHook") => {
            return Some(Type::Poly);
        }
        (TEST_INVALID_TYPE_PROVIDER_MODULE_ID, "notAhookTypedAsHook") => {
            return Some(Type::Function {
                shape_id: Some(TEST_INVALID_TYPE_PROVIDER_NON_HOOK_TYPED_AS_HOOK_ID.to_string()),
                return_type: Box::new(Type::Poly),
                is_constructor: false,
            });
        }
        _ => {}
    }

    let globals = GlobalRegistry::new();
    let property_type = globals.shapes.get_property(shape_id, property_name)?;
    match property_type {
        PropertyType::Primitive => Some(Type::Primitive),
        PropertyType::Object { shape_id } => Some(Type::Object {
            shape_id: Some((*shape_id).to_string()),
        }),
        PropertyType::Function(signature) => Some(Type::Function {
            shape_id: Some(encode_method_signature_shape_id(shape_id, property_name)),
            return_type: Box::new(return_type_to_hir_type(&signature.return_type)),
            is_constructor: false,
        }),
        PropertyType::Poly => Some(Type::Poly),
    }
}

fn return_type_to_hir_type(return_type: &ReturnType) -> Type {
    match return_type {
        ReturnType::Primitive => Type::Primitive,
        ReturnType::Object { shape_id } => Type::Object {
            shape_id: Some((*shape_id).to_string()),
        },
        ReturnType::Function {
            shape_id,
            return_type,
            is_constructor,
        } => Type::Function {
            shape_id: shape_id.map(|sid| sid.to_string()),
            return_type: Box::new(return_type_to_hir_type(return_type)),
            is_constructor: *is_constructor,
        },
        ReturnType::Poly => Type::Poly,
    }
}

fn infer_method_return_type(receiver_type: &Type, method_name: &str) -> Option<Type> {
    let shape_id = match receiver_type {
        Type::Object {
            shape_id: Some(shape_id),
        } => Some(shape_id.as_str()),
        Type::Function {
            shape_id: Some(shape_id),
            ..
        } => Some(shape_id.as_str()),
        _ => None,
    }?;

    let globals = GlobalRegistry::new();
    let property = globals.shapes.get_property(shape_id, method_name)?;
    match property {
        PropertyType::Primitive => Some(Type::Primitive),
        PropertyType::Object { shape_id } => Some(Type::Object {
            shape_id: Some((*shape_id).to_string()),
        }),
        PropertyType::Function(signature) => Some(return_type_to_hir_type(&signature.return_type)),
        PropertyType::Poly => Some(Type::Poly),
    }
}

fn method_name_implies_array_receiver(method_name: &str) -> bool {
    method_name == "flatMap"
}

fn is_hook_like_name(name: &str) -> bool {
    let candidate = normalize_hook_name(name);
    candidate.starts_with("use")
        && candidate.len() > 3
        && candidate.chars().nth(3).is_some_and(|c| c.is_uppercase())
}

fn default_hook_type(env_config: &crate::options::EnvironmentConfig) -> Type {
    if env_config.enable_assume_hooks_follow_rules_of_react {
        default_nonmutating_hook_type()
    } else {
        default_mutating_hook_type()
    }
}

fn default_nonmutating_hook_type() -> Type {
    Type::Function {
        shape_id: Some("BuiltInDefaultNonmutatingHookId".to_string()),
        return_type: Box::new(Type::Poly),
        is_constructor: false,
    }
}

fn default_mutating_hook_type() -> Type {
    Type::Function {
        shape_id: Some("BuiltInDefaultMutatingHookId".to_string()),
        return_type: Box::new(Type::Poly),
        is_constructor: false,
    }
}

fn is_primitive_binary_op(op: BinaryOperator) -> bool {
    matches!(
        op,
        BinaryOperator::Add
            | BinaryOperator::Sub
            | BinaryOperator::Div
            | BinaryOperator::Mod
            | BinaryOperator::Mul
            | BinaryOperator::Exp
            | BinaryOperator::BitAnd
            | BinaryOperator::BitOr
            | BinaryOperator::RShift
            | BinaryOperator::LShift
            | BinaryOperator::BitXor
            | BinaryOperator::Gt
            | BinaryOperator::Lt
            | BinaryOperator::GtEq
            | BinaryOperator::LtEq
    )
}

/// Check if a global function name is known to return a primitive.
fn is_primitive_returning_global(name: &str) -> bool {
    matches!(
        name,
        "Number" | "String" | "Boolean" | "parseInt" | "parseFloat" | "isNaN" | "isFinite"
    )
}

/// Check if a method name is known to return a primitive.
fn is_primitive_returning_method(name: &str) -> bool {
    matches!(
        name,
        // String methods returning primitives
        "indexOf" | "lastIndexOf" | "charCodeAt" | "codePointAt"
        | "includes" | "startsWith" | "endsWith"
        | "localeCompare"
        // Number/Math methods
        | "toFixed" | "toPrecision" | "toExponential"
        | "toString" | "valueOf"
        // Object methods returning primitives
        | "hasOwnProperty" | "isPrototypeOf" | "propertyIsEnumerable"
        // Array methods returning booleans/numbers
        | "every" | "some"
    )
}

/// Return the known return type for a hook name (e.g. "useState", "useRef").
///
/// This is used both for direct calls (`useState(0)`) and method calls
/// (`React.useState(0)`), where the method property is a lowered
/// `Primitive::String("useState")` temporary.
fn hook_return_type_for_name(name: &str) -> Option<Type> {
    match normalize_hook_name(name) {
        "useRef" | "createRef" => Some(Type::Object {
            shape_id: Some("BuiltInUseRefId".to_string()),
        }),
        "useState" => Some(Type::Object {
            shape_id: Some("BuiltInUseStateHookResult".to_string()),
        }),
        "useReducer" => Some(Type::Object {
            shape_id: Some("BuiltInUseReducerHookResult".to_string()),
        }),
        "useTransition" => Some(Type::Object {
            shape_id: Some("BuiltInUseTransitionHookResult".to_string()),
        }),
        "useActionState" => Some(Type::Object {
            shape_id: Some("BuiltInUseActionStateHookResult".to_string()),
        }),
        _ => None,
    }
}

fn normalize_hook_name(name: &str) -> &str {
    let tail = name.rsplit_once('.').map_or(name, |(_, tail)| tail);
    tail.rsplit_once('$').map_or(tail, |(_, tail)| tail)
}
