//! Global type registry and built-in shape registration.
//!
//! Port of `Globals.ts` from upstream React Compiler (babel-plugin-react-compiler).
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.
//!
//! This module registers all built-in object shapes (Array, Set, Map, etc.) and
//! global names (React hooks, Math, console, etc.) so the compiler can look up
//! function signatures and infer effects.

use std::collections::HashMap;

use crate::hir::types::{Effect, ValueKind, ValueReason};

use super::object_shape::*;

// ---------------------------------------------------------------------------
// Global type
// ---------------------------------------------------------------------------

/// Describes the type of a global binding.
#[derive(Debug, Clone)]
pub struct GlobalType {
    pub kind: GlobalKind,
}

/// The kind of a global binding.
#[derive(Debug, Clone)]
pub enum GlobalKind {
    /// A callable function with a known signature.
    Function(FunctionSignature),
    /// An object with a known shape.
    Object { shape_id: ShapeId },
    /// A React hook with a known signature.
    Hook(FunctionSignature),
    /// A primitive value (number, string, boolean, etc.).
    Primitive,
    /// Unknown / polymorphic type.
    Poly,
}

// ---------------------------------------------------------------------------
// Global registry
// ---------------------------------------------------------------------------

/// Registry of known globals and their shapes.
pub struct GlobalRegistry {
    pub shapes: ShapeRegistry,
    globals: HashMap<String, GlobalType>,
}

impl GlobalRegistry {
    pub fn new() -> Self {
        let mut registry = Self {
            shapes: ShapeRegistry::new(),
            globals: HashMap::new(),
        };
        registry.register_builtin_shapes();
        registry.register_globals();
        registry
    }

    pub fn get_global(&self, name: &str) -> Option<&GlobalType> {
        self.globals.get(name)
    }

    pub fn get_shape(&self, id: &str) -> Option<&ObjectShape> {
        self.shapes.get(id)
    }
}

impl Default for GlobalRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Helper constructors
// ---------------------------------------------------------------------------

/// Build a hook signature.
fn make_hook_sig(
    positional_params: Vec<Effect>,
    rest: Option<Effect>,
    return_type: ReturnType,
    return_kind: ValueKind,
    return_reason: Option<ValueReason>,
) -> FunctionSignature {
    FunctionSignature {
        positional_params,
        rest_param: rest,
        return_type,
        return_value_kind: return_kind,
        return_value_reason: return_reason,
        callee_effect: Effect::Read,
        ..Default::default()
    }
}

/// Build a pure function signature (reads callee, no impure flag).
fn make_pure_fn(
    positional_params: Vec<Effect>,
    rest: Option<Effect>,
    return_type: ReturnType,
    return_kind: ValueKind,
) -> FunctionSignature {
    FunctionSignature {
        positional_params,
        rest_param: rest,
        return_type,
        return_value_kind: return_kind,
        callee_effect: Effect::Read,
        ..Default::default()
    }
}

/// Build a pure function that reads all args and returns a primitive.
fn make_read_to_primitive() -> FunctionSignature {
    make_pure_fn(
        vec![],
        Some(Effect::Read),
        ReturnType::Primitive,
        ValueKind::Primitive,
    )
}

/// Build an impure function signature. Impure functions (e.g. Math.random,
/// Date.now) read their arguments but produce values that cannot be
/// deduplicated across renders.
fn make_impure_fn(canonical_name: &str) -> FunctionSignature {
    FunctionSignature {
        positional_params: vec![],
        rest_param: Some(Effect::Read),
        return_type: ReturnType::Poly,
        return_value_kind: ValueKind::Mutable,
        callee_effect: Effect::Read,
        impure: true,
        canonical_name: Some(canonical_name.to_string()),
        ..Default::default()
    }
}

/// Build a method signature with full control over every field.
fn make_method(
    positional_params: Vec<Effect>,
    rest: Option<Effect>,
    return_type: ReturnType,
    return_kind: ValueKind,
    callee_effect: Effect,
    no_alias: bool,
    mutable_only_if_operands_are_mutable: bool,
) -> FunctionSignature {
    FunctionSignature {
        positional_params,
        rest_param: rest,
        return_type,
        return_value_kind: return_kind,
        callee_effect,
        no_alias,
        mutable_only_if_operands_are_mutable,
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

impl GlobalRegistry {
    /// Register all built-in shapes (Array, Object, Set, Map, etc.) into
    /// `self.shapes`. These are the shapes referenced by BUILTIN_SHAPES in
    /// the upstream ObjectShape.ts.
    fn register_builtin_shapes(&mut self) {
        self.register_props_shape();
        self.register_array_shape();
        self.register_object_shape();
        self.register_set_shape();
        self.register_map_shape();
        self.register_weak_set_shape();
        self.register_weak_map_shape();
        self.register_use_state_shape();
        self.register_use_action_state_shape();
        self.register_use_reducer_shape();
        self.register_use_transition_shape();
        self.register_use_ref_shape();
        self.register_ref_value_shape();
        self.register_mixed_readonly_shape();
        self.register_effect_event_shape();
        self.register_misc_shapes();
        self.register_reanimated_shapes();
        self.register_test_module_shapes();
    }

    // -- Props --------------------------------------------------------------

    fn register_props_shape(&mut self) {
        let mut props = HashMap::new();
        props.insert(
            "ref".to_string(),
            PropertyType::Object {
                shape_id: BUILT_IN_USE_REF_ID,
            },
        );
        self.shapes.insert(
            BUILT_IN_PROPS_ID,
            ObjectShape {
                properties: props,
                function_type: None,
            },
        );
    }

    // -- Array instance methods ---------------------------------------------

    fn register_array_shape(&mut self) {
        let mut props = HashMap::new();

        // indexOf, includes: read args, read callee, return primitive
        for name in &["indexOf", "includes"] {
            props.insert(
                name.to_string(),
                PropertyType::Function(make_method(
                    vec![],
                    Some(Effect::Read),
                    ReturnType::Primitive,
                    ValueKind::Primitive,
                    Effect::Read,
                    false,
                    false,
                )),
            );
        }

        // pop: no args, store callee, return poly mutable
        props.insert(
            "pop".to_string(),
            PropertyType::Function(make_method(
                vec![],
                None,
                ReturnType::Poly,
                ValueKind::Mutable,
                Effect::Store,
                false,
                false,
            )),
        );

        // at: read arg, capture callee, return poly mutable
        props.insert(
            "at".to_string(),
            PropertyType::Function(make_method(
                vec![Effect::Read],
                None,
                ReturnType::Poly,
                ValueKind::Mutable,
                Effect::Capture,
                false,
                false,
            )),
        );

        // concat: capture args, capture callee, return BuiltInArray mutable
        props.insert(
            "concat".to_string(),
            PropertyType::Function(make_method(
                vec![],
                Some(Effect::Capture),
                ReturnType::Object {
                    shape_id: BUILT_IN_ARRAY_ID,
                },
                ValueKind::Mutable,
                Effect::Capture,
                false,
                false,
            )),
        );

        // length: primitive property
        props.insert("length".to_string(), PropertyType::Primitive);

        // push: capture args, store callee, return primitive
        props.insert(
            "push".to_string(),
            PropertyType::Function(make_method(
                vec![],
                Some(Effect::Capture),
                ReturnType::Primitive,
                ValueKind::Primitive,
                Effect::Store,
                false,
                false,
            )),
        );

        // slice: read args, capture callee, return BuiltInArray mutable
        props.insert(
            "slice".to_string(),
            PropertyType::Function(make_method(
                vec![],
                Some(Effect::Read),
                ReturnType::Object {
                    shape_id: BUILT_IN_ARRAY_ID,
                },
                ValueKind::Mutable,
                Effect::Capture,
                false,
                false,
            )),
        );

        // map, flatMap, filter: conditionally mutate args/callee, return BuiltInArray mutable,
        // noAlias=true, mutableOnlyIfOperandsAreMutable=true
        for name in &["map", "flatMap", "filter"] {
            props.insert(
                name.to_string(),
                PropertyType::Function(make_method(
                    vec![],
                    Some(Effect::ConditionallyMutate),
                    ReturnType::Object {
                        shape_id: BUILT_IN_ARRAY_ID,
                    },
                    ValueKind::Mutable,
                    Effect::ConditionallyMutate,
                    true,
                    true,
                )),
            );
        }

        // every, some, findIndex: conditionally mutate args/callee, return primitive,
        // noAlias=true, mutableOnlyIfOperandsAreMutable=true
        for name in &["every", "some", "findIndex"] {
            props.insert(
                name.to_string(),
                PropertyType::Function(make_method(
                    vec![],
                    Some(Effect::ConditionallyMutate),
                    ReturnType::Primitive,
                    ValueKind::Primitive,
                    Effect::ConditionallyMutate,
                    true,
                    true,
                )),
            );
        }

        // find: conditionally mutate args/callee, return poly mutable,
        // noAlias=true, mutableOnlyIfOperandsAreMutable=true
        props.insert(
            "find".to_string(),
            PropertyType::Function(make_method(
                vec![],
                Some(Effect::ConditionallyMutate),
                ReturnType::Poly,
                ValueKind::Mutable,
                Effect::ConditionallyMutate,
                true,
                true,
            )),
        );

        // join: read args, read callee, return primitive
        props.insert(
            "join".to_string(),
            PropertyType::Function(make_method(
                vec![],
                Some(Effect::Read),
                ReturnType::Primitive,
                ValueKind::Primitive,
                Effect::Read,
                false,
                false,
            )),
        );

        self.shapes.insert(
            BUILT_IN_ARRAY_ID,
            ObjectShape {
                properties: props,
                function_type: None,
            },
        );
    }

    // -- Object instance methods --------------------------------------------

    fn register_object_shape(&mut self) {
        let mut props = HashMap::new();

        // toString: read callee, return primitive
        props.insert(
            "toString".to_string(),
            PropertyType::Function(make_method(
                vec![],
                None,
                ReturnType::Primitive,
                ValueKind::Primitive,
                Effect::Read,
                false,
                false,
            )),
        );

        self.shapes.insert(
            BUILT_IN_OBJECT_ID,
            ObjectShape {
                properties: props,
                function_type: None,
            },
        );
    }

    // -- Set instance methods -----------------------------------------------

    fn register_set_shape(&mut self) {
        let mut props = HashMap::new();

        // add: capture arg, store callee, return BuiltInSet mutable
        props.insert(
            "add".to_string(),
            PropertyType::Function(make_method(
                vec![Effect::Capture],
                None,
                ReturnType::Object {
                    shape_id: BUILT_IN_SET_ID,
                },
                ValueKind::Mutable,
                Effect::Store,
                false,
                false,
            )),
        );

        // clear: store callee, return primitive
        props.insert(
            "clear".to_string(),
            PropertyType::Function(make_method(
                vec![],
                None,
                ReturnType::Primitive,
                ValueKind::Primitive,
                Effect::Store,
                false,
                false,
            )),
        );

        // delete: read arg, store callee, return primitive
        props.insert(
            "delete".to_string(),
            PropertyType::Function(make_method(
                vec![Effect::Read],
                None,
                ReturnType::Primitive,
                ValueKind::Primitive,
                Effect::Store,
                false,
                false,
            )),
        );

        // has: read arg, read callee, return primitive
        props.insert(
            "has".to_string(),
            PropertyType::Function(make_method(
                vec![Effect::Read],
                None,
                ReturnType::Primitive,
                ValueKind::Primitive,
                Effect::Read,
                false,
                false,
            )),
        );

        // size: primitive property
        props.insert("size".to_string(), PropertyType::Primitive);

        // difference, union, symmetricalDifference: capture arg, capture callee,
        // return BuiltInSet mutable
        for name in &["difference", "union", "symmetricalDifference"] {
            props.insert(
                name.to_string(),
                PropertyType::Function(make_method(
                    vec![Effect::Capture],
                    None,
                    ReturnType::Object {
                        shape_id: BUILT_IN_SET_ID,
                    },
                    ValueKind::Mutable,
                    Effect::Capture,
                    false,
                    false,
                )),
            );
        }

        // isSubsetOf, isSupersetOf: read arg, read callee, return primitive
        for name in &["isSubsetOf", "isSupersetOf"] {
            props.insert(
                name.to_string(),
                PropertyType::Function(make_method(
                    vec![Effect::Read],
                    None,
                    ReturnType::Primitive,
                    ValueKind::Primitive,
                    Effect::Read,
                    false,
                    false,
                )),
            );
        }

        // forEach: conditionally mutate, noAlias=true, mutableOnlyIfOperandsAreMutable=true
        props.insert(
            "forEach".to_string(),
            PropertyType::Function(make_method(
                vec![],
                Some(Effect::ConditionallyMutate),
                ReturnType::Primitive,
                ValueKind::Primitive,
                Effect::ConditionallyMutate,
                true,
                true,
            )),
        );

        // entries, keys, values: capture callee, return poly mutable
        for name in &["entries", "keys", "values"] {
            props.insert(
                name.to_string(),
                PropertyType::Function(make_method(
                    vec![],
                    None,
                    ReturnType::Poly,
                    ValueKind::Mutable,
                    Effect::Capture,
                    false,
                    false,
                )),
            );
        }

        self.shapes.insert(
            BUILT_IN_SET_ID,
            ObjectShape {
                properties: props,
                function_type: None,
            },
        );
    }

    // -- Map instance methods -----------------------------------------------

    fn register_map_shape(&mut self) {
        let mut props = HashMap::new();

        // clear: store callee, return primitive
        props.insert(
            "clear".to_string(),
            PropertyType::Function(make_method(
                vec![],
                None,
                ReturnType::Primitive,
                ValueKind::Primitive,
                Effect::Store,
                false,
                false,
            )),
        );

        // delete: read arg, store callee, return primitive
        props.insert(
            "delete".to_string(),
            PropertyType::Function(make_method(
                vec![Effect::Read],
                None,
                ReturnType::Primitive,
                ValueKind::Primitive,
                Effect::Store,
                false,
                false,
            )),
        );

        // get: read arg, capture callee, return poly mutable
        props.insert(
            "get".to_string(),
            PropertyType::Function(make_method(
                vec![Effect::Read],
                None,
                ReturnType::Poly,
                ValueKind::Mutable,
                Effect::Capture,
                false,
                false,
            )),
        );

        // has: read arg, read callee, return primitive
        props.insert(
            "has".to_string(),
            PropertyType::Function(make_method(
                vec![Effect::Read],
                None,
                ReturnType::Primitive,
                ValueKind::Primitive,
                Effect::Read,
                false,
                false,
            )),
        );

        // set: capture key + value, store callee, return BuiltInMap mutable
        props.insert(
            "set".to_string(),
            PropertyType::Function(make_method(
                vec![Effect::Capture, Effect::Capture],
                None,
                ReturnType::Object {
                    shape_id: BUILT_IN_MAP_ID,
                },
                ValueKind::Mutable,
                Effect::Store,
                false,
                false,
            )),
        );

        // size: primitive property
        props.insert("size".to_string(), PropertyType::Primitive);

        // forEach: conditionally mutate, noAlias=true, mutableOnlyIfOperandsAreMutable=true
        props.insert(
            "forEach".to_string(),
            PropertyType::Function(make_method(
                vec![],
                Some(Effect::ConditionallyMutate),
                ReturnType::Primitive,
                ValueKind::Primitive,
                Effect::ConditionallyMutate,
                true,
                true,
            )),
        );

        // entries, keys, values: capture callee, return poly mutable
        for name in &["entries", "keys", "values"] {
            props.insert(
                name.to_string(),
                PropertyType::Function(make_method(
                    vec![],
                    None,
                    ReturnType::Poly,
                    ValueKind::Mutable,
                    Effect::Capture,
                    false,
                    false,
                )),
            );
        }

        self.shapes.insert(
            BUILT_IN_MAP_ID,
            ObjectShape {
                properties: props,
                function_type: None,
            },
        );
    }

    // -- WeakSet instance methods -------------------------------------------

    fn register_weak_set_shape(&mut self) {
        let mut props = HashMap::new();

        // add: capture arg, store callee, return BuiltInWeakSet mutable
        props.insert(
            "add".to_string(),
            PropertyType::Function(make_method(
                vec![Effect::Capture],
                None,
                ReturnType::Object {
                    shape_id: BUILT_IN_WEAK_SET_ID,
                },
                ValueKind::Mutable,
                Effect::Store,
                false,
                false,
            )),
        );

        // delete: read arg, store callee, return primitive
        props.insert(
            "delete".to_string(),
            PropertyType::Function(make_method(
                vec![Effect::Read],
                None,
                ReturnType::Primitive,
                ValueKind::Primitive,
                Effect::Store,
                false,
                false,
            )),
        );

        // has: read arg, read callee, return primitive
        props.insert(
            "has".to_string(),
            PropertyType::Function(make_method(
                vec![Effect::Read],
                None,
                ReturnType::Primitive,
                ValueKind::Primitive,
                Effect::Read,
                false,
                false,
            )),
        );

        self.shapes.insert(
            BUILT_IN_WEAK_SET_ID,
            ObjectShape {
                properties: props,
                function_type: None,
            },
        );
    }

    // -- WeakMap instance methods -------------------------------------------

    fn register_weak_map_shape(&mut self) {
        let mut props = HashMap::new();

        // delete: read arg, store callee, return primitive
        props.insert(
            "delete".to_string(),
            PropertyType::Function(make_method(
                vec![Effect::Read],
                None,
                ReturnType::Primitive,
                ValueKind::Primitive,
                Effect::Store,
                false,
                false,
            )),
        );

        // get: read arg, capture callee, return poly mutable
        props.insert(
            "get".to_string(),
            PropertyType::Function(make_method(
                vec![Effect::Read],
                None,
                ReturnType::Poly,
                ValueKind::Mutable,
                Effect::Capture,
                false,
                false,
            )),
        );

        // has: read arg, read callee, return primitive
        props.insert(
            "has".to_string(),
            PropertyType::Function(make_method(
                vec![Effect::Read],
                None,
                ReturnType::Primitive,
                ValueKind::Primitive,
                Effect::Read,
                false,
                false,
            )),
        );

        // set: capture key + value, store callee, return BuiltInWeakMap mutable
        props.insert(
            "set".to_string(),
            PropertyType::Function(make_method(
                vec![Effect::Capture, Effect::Capture],
                None,
                ReturnType::Object {
                    shape_id: BUILT_IN_WEAK_MAP_ID,
                },
                ValueKind::Mutable,
                Effect::Store,
                false,
                false,
            )),
        );

        self.shapes.insert(
            BUILT_IN_WEAK_MAP_ID,
            ObjectShape {
                properties: props,
                function_type: None,
            },
        );
    }

    // -- useState return shape ----------------------------------------------

    fn register_use_state_shape(&mut self) {
        let mut props = HashMap::new();

        // "0" -> state value (Poly)
        props.insert("0".to_string(), PropertyType::Poly);

        // "1" -> setState function: freezes args, reads callee, returns primitive
        props.insert(
            "1".to_string(),
            PropertyType::Function(FunctionSignature {
                positional_params: vec![],
                rest_param: Some(Effect::Freeze),
                return_type: ReturnType::Primitive,
                return_value_kind: ValueKind::Primitive,
                callee_effect: Effect::Read,
                ..Default::default()
            }),
        );

        self.shapes.insert(
            BUILT_IN_USE_STATE_ID,
            ObjectShape {
                properties: props,
                function_type: None,
            },
        );

        // Also register the setState shape so it can be looked up by
        // BUILT_IN_SET_STATE_ID. The upstream creates a separate shape for
        // the setState function via addFunction with id=BuiltInSetStateId.
        self.shapes.insert(
            BUILT_IN_SET_STATE_ID,
            ObjectShape {
                properties: HashMap::new(),
                function_type: Some(FunctionSignature {
                    positional_params: vec![],
                    rest_param: Some(Effect::Freeze),
                    return_type: ReturnType::Primitive,
                    return_value_kind: ValueKind::Primitive,
                    callee_effect: Effect::Read,
                    ..Default::default()
                }),
            },
        );
    }

    // -- useActionState return shape ----------------------------------------

    fn register_use_action_state_shape(&mut self) {
        let mut props = HashMap::new();
        props.insert("0".to_string(), PropertyType::Poly);
        props.insert(
            "1".to_string(),
            PropertyType::Function(FunctionSignature {
                positional_params: vec![],
                rest_param: Some(Effect::Freeze),
                return_type: ReturnType::Primitive,
                return_value_kind: ValueKind::Primitive,
                callee_effect: Effect::Read,
                ..Default::default()
            }),
        );
        self.shapes.insert(
            BUILT_IN_USE_ACTION_STATE_ID,
            ObjectShape {
                properties: props,
                function_type: None,
            },
        );

        self.shapes.insert(
            BUILT_IN_SET_ACTION_STATE_ID,
            ObjectShape {
                properties: HashMap::new(),
                function_type: Some(FunctionSignature {
                    positional_params: vec![],
                    rest_param: Some(Effect::Freeze),
                    return_type: ReturnType::Primitive,
                    return_value_kind: ValueKind::Primitive,
                    callee_effect: Effect::Read,
                    ..Default::default()
                }),
            },
        );
    }

    // -- useReducer return shape --------------------------------------------

    fn register_use_reducer_shape(&mut self) {
        let mut props = HashMap::new();
        props.insert("0".to_string(), PropertyType::Poly);
        props.insert(
            "1".to_string(),
            PropertyType::Function(FunctionSignature {
                positional_params: vec![],
                rest_param: Some(Effect::Freeze),
                return_type: ReturnType::Primitive,
                return_value_kind: ValueKind::Primitive,
                callee_effect: Effect::Read,
                ..Default::default()
            }),
        );
        self.shapes.insert(
            BUILT_IN_USE_REDUCER_ID,
            ObjectShape {
                properties: props,
                function_type: None,
            },
        );

        self.shapes.insert(
            BUILT_IN_DISPATCH_ID,
            ObjectShape {
                properties: HashMap::new(),
                function_type: Some(FunctionSignature {
                    positional_params: vec![],
                    rest_param: Some(Effect::Freeze),
                    return_type: ReturnType::Primitive,
                    return_value_kind: ValueKind::Primitive,
                    callee_effect: Effect::Read,
                    ..Default::default()
                }),
            },
        );
    }

    // -- useTransition return shape -----------------------------------------

    fn register_use_transition_shape(&mut self) {
        let mut props = HashMap::new();
        // "0" -> isPending (Primitive)
        props.insert("0".to_string(), PropertyType::Primitive);
        // "1" -> startTransition function: reads callee, returns primitive
        props.insert(
            "1".to_string(),
            PropertyType::Function(FunctionSignature {
                positional_params: vec![],
                rest_param: None,
                return_type: ReturnType::Primitive,
                return_value_kind: ValueKind::Primitive,
                callee_effect: Effect::Read,
                ..Default::default()
            }),
        );
        self.shapes.insert(
            BUILT_IN_USE_TRANSITION_ID,
            ObjectShape {
                properties: props,
                function_type: None,
            },
        );

        self.shapes.insert(
            BUILT_IN_START_TRANSITION_ID,
            ObjectShape {
                properties: HashMap::new(),
                function_type: Some(FunctionSignature {
                    positional_params: vec![],
                    rest_param: None,
                    return_type: ReturnType::Primitive,
                    return_value_kind: ValueKind::Primitive,
                    callee_effect: Effect::Read,
                    ..Default::default()
                }),
            },
        );
    }

    // -- useRef return shape ------------------------------------------------

    fn register_use_ref_shape(&mut self) {
        let mut props = HashMap::new();
        props.insert(
            "current".to_string(),
            PropertyType::Object {
                shape_id: BUILT_IN_REF_VALUE_ID,
            },
        );
        self.shapes.insert(
            BUILT_IN_USE_REF_ID,
            ObjectShape {
                properties: props,
                function_type: None,
            },
        );
    }

    // -- Ref value shape (recursive wildcard) -------------------------------

    fn register_ref_value_shape(&mut self) {
        let mut props = HashMap::new();
        props.insert(
            "*".to_string(),
            PropertyType::Object {
                shape_id: BUILT_IN_REF_VALUE_ID,
            },
        );
        self.shapes.insert(
            BUILT_IN_REF_VALUE_ID,
            ObjectShape {
                properties: props,
                function_type: None,
            },
        );
    }

    // -- MixedReadonly shape ------------------------------------------------

    fn register_mixed_readonly_shape(&mut self) {
        let mut props = HashMap::new();

        // toString: read callee, return primitive
        props.insert(
            "toString".to_string(),
            PropertyType::Function(make_method(
                vec![],
                Some(Effect::Read),
                ReturnType::Primitive,
                ValueKind::Primitive,
                Effect::Read,
                false,
                false,
            )),
        );

        // indexOf, includes: read args, read callee, return primitive
        for name in &["indexOf", "includes"] {
            props.insert(
                name.to_string(),
                PropertyType::Function(make_method(
                    vec![],
                    Some(Effect::Read),
                    ReturnType::Primitive,
                    ValueKind::Primitive,
                    Effect::Read,
                    false,
                    false,
                )),
            );
        }

        // at: read arg, capture callee, return MixedReadonly frozen
        props.insert(
            "at".to_string(),
            PropertyType::Function(make_method(
                vec![Effect::Read],
                None,
                ReturnType::Object {
                    shape_id: BUILT_IN_MIXED_READONLY_ID,
                },
                ValueKind::Frozen,
                Effect::Capture,
                false,
                false,
            )),
        );

        // map, flatMap, filter: conditionally mutate, return BuiltInArray mutable, noAlias=true
        // Note: on MixedReadonly, mutableOnlyIfOperandsAreMutable is NOT set (unlike on Array)
        for name in &["map", "flatMap", "filter"] {
            props.insert(
                name.to_string(),
                PropertyType::Function(make_method(
                    vec![],
                    Some(Effect::ConditionallyMutate),
                    ReturnType::Object {
                        shape_id: BUILT_IN_ARRAY_ID,
                    },
                    ValueKind::Mutable,
                    Effect::ConditionallyMutate,
                    true,
                    false,
                )),
            );
        }

        // concat: capture args, capture callee, return BuiltInArray mutable
        props.insert(
            "concat".to_string(),
            PropertyType::Function(make_method(
                vec![],
                Some(Effect::Capture),
                ReturnType::Object {
                    shape_id: BUILT_IN_ARRAY_ID,
                },
                ValueKind::Mutable,
                Effect::Capture,
                false,
                false,
            )),
        );

        // slice: read args, capture callee, return BuiltInArray mutable
        props.insert(
            "slice".to_string(),
            PropertyType::Function(make_method(
                vec![],
                Some(Effect::Read),
                ReturnType::Object {
                    shape_id: BUILT_IN_ARRAY_ID,
                },
                ValueKind::Mutable,
                Effect::Capture,
                false,
                false,
            )),
        );

        // every, some: conditionally mutate, return primitive, noAlias=true,
        // mutableOnlyIfOperandsAreMutable=true
        for name in &["every", "some"] {
            props.insert(
                name.to_string(),
                PropertyType::Function(make_method(
                    vec![],
                    Some(Effect::ConditionallyMutate),
                    ReturnType::Primitive,
                    ValueKind::Primitive,
                    Effect::ConditionallyMutate,
                    true,
                    true,
                )),
            );
        }

        // find: conditionally mutate, return MixedReadonly frozen, noAlias=true,
        // mutableOnlyIfOperandsAreMutable=true
        props.insert(
            "find".to_string(),
            PropertyType::Function(make_method(
                vec![],
                Some(Effect::ConditionallyMutate),
                ReturnType::Object {
                    shape_id: BUILT_IN_MIXED_READONLY_ID,
                },
                ValueKind::Frozen,
                Effect::ConditionallyMutate,
                true,
                true,
            )),
        );

        // findIndex: conditionally mutate, return primitive, noAlias=true,
        // mutableOnlyIfOperandsAreMutable=true
        props.insert(
            "findIndex".to_string(),
            PropertyType::Function(make_method(
                vec![],
                Some(Effect::ConditionallyMutate),
                ReturnType::Primitive,
                ValueKind::Primitive,
                Effect::ConditionallyMutate,
                true,
                true,
            )),
        );

        // join: read args, read callee, return primitive
        props.insert(
            "join".to_string(),
            PropertyType::Function(make_method(
                vec![],
                Some(Effect::Read),
                ReturnType::Primitive,
                ValueKind::Primitive,
                Effect::Read,
                false,
                false,
            )),
        );

        // "*" wildcard: any other property returns MixedReadonly
        props.insert(
            "*".to_string(),
            PropertyType::Object {
                shape_id: BUILT_IN_MIXED_READONLY_ID,
            },
        );

        self.shapes.insert(
            BUILT_IN_MIXED_READONLY_ID,
            ObjectShape {
                properties: props,
                function_type: None,
            },
        );
    }

    // -- EffectEvent shape --------------------------------------------------

    fn register_effect_event_shape(&mut self) {
        self.shapes.insert(
            BUILT_IN_EFFECT_EVENT_ID,
            ObjectShape {
                properties: HashMap::new(),
                function_type: Some(FunctionSignature {
                    positional_params: vec![],
                    rest_param: Some(Effect::ConditionallyMutate),
                    return_type: ReturnType::Poly,
                    return_value_kind: ValueKind::Mutable,
                    callee_effect: Effect::ConditionallyMutate,
                    ..Default::default()
                }),
            },
        );
    }

    // -- Misc shapes (JSX, Function, Autodeps) ------------------------------

    fn register_misc_shapes(&mut self) {
        self.shapes.insert(
            BUILT_IN_JSX_ID,
            ObjectShape {
                properties: HashMap::new(),
                function_type: None,
            },
        );
        self.shapes.insert(
            BUILT_IN_FUNCTION_ID,
            ObjectShape {
                properties: HashMap::new(),
                function_type: None,
            },
        );
        self.shapes.insert(
            BUILT_IN_AUTODEPS_ID,
            ObjectShape {
                properties: HashMap::new(),
                function_type: None,
            },
        );
    }

    fn register_reanimated_shapes(&mut self) {
        // Reanimated shared values are ref-like mutable objects.
        self.shapes.insert(
            REANIMATED_SHARED_VALUE_ID,
            ObjectShape {
                properties: HashMap::new(),
                function_type: None,
            },
        );

        let frozen_hook_signature = FunctionSignature {
            positional_params: vec![],
            rest_param: Some(Effect::Freeze),
            return_type: ReturnType::Poly,
            return_value_kind: ValueKind::Frozen,
            callee_effect: Effect::Read,
            no_alias: true,
            ..Default::default()
        };
        self.shapes.insert(
            REANIMATED_FROZEN_HOOK_ID,
            ObjectShape {
                properties: HashMap::new(),
                function_type: Some(frozen_hook_signature.clone()),
            },
        );

        let mutable_hook_signature = FunctionSignature {
            positional_params: vec![],
            rest_param: Some(Effect::Freeze),
            return_type: ReturnType::Object {
                shape_id: REANIMATED_SHARED_VALUE_ID,
            },
            return_value_kind: ValueKind::Mutable,
            callee_effect: Effect::Read,
            no_alias: true,
            ..Default::default()
        };
        self.shapes.insert(
            REANIMATED_MUTABLE_HOOK_ID,
            ObjectShape {
                properties: HashMap::new(),
                function_type: Some(mutable_hook_signature.clone()),
            },
        );

        let mutable_function_signature = FunctionSignature {
            positional_params: vec![],
            rest_param: Some(Effect::Read),
            return_type: ReturnType::Poly,
            return_value_kind: ValueKind::Mutable,
            callee_effect: Effect::Read,
            no_alias: true,
            ..Default::default()
        };
        self.shapes.insert(
            REANIMATED_MUTABLE_FUNCTION_ID,
            ObjectShape {
                properties: HashMap::new(),
                function_type: Some(mutable_function_signature.clone()),
            },
        );

        let mut reanimated_module_props = HashMap::new();
        for hook in [
            "useFrameCallback",
            "useAnimatedStyle",
            "useAnimatedProps",
            "useAnimatedScrollHandler",
            "useAnimatedReaction",
            "useWorkletCallback",
        ] {
            reanimated_module_props.insert(
                hook.to_string(),
                PropertyType::Function(frozen_hook_signature.clone()),
            );
        }
        for hook in ["useSharedValue", "useDerivedValue"] {
            reanimated_module_props.insert(
                hook.to_string(),
                PropertyType::Function(mutable_hook_signature.clone()),
            );
        }
        for func in [
            "withTiming",
            "withSpring",
            "createAnimatedPropAdapter",
            "withDecay",
            "withRepeat",
            "runOnUI",
            "executeOnUIRuntimeSync",
        ] {
            reanimated_module_props.insert(
                func.to_string(),
                PropertyType::Function(mutable_function_signature.clone()),
            );
        }
        self.shapes.insert(
            REANIMATED_MODULE_ID,
            ObjectShape {
                properties: reanimated_module_props,
                function_type: None,
            },
        );
    }

    fn register_test_module_shapes(&mut self) {
        let known_incompatible_message = "useKnownIncompatible is known to be incompatible";
        let known_incompatible_indirect_message = "useKnownIncompatibleIndirect returns an incompatible() function that is known incompatible";

        let known_incompatible_hook = FunctionSignature {
            positional_params: vec![],
            rest_param: Some(Effect::Read),
            return_type: ReturnType::Poly,
            return_value_kind: ValueKind::Frozen,
            callee_effect: Effect::Read,
            known_incompatible: Some(known_incompatible_message.to_string()),
            ..Default::default()
        };
        self.shapes.insert(
            TEST_KNOWN_INCOMPATIBLE_HOOK_ID,
            ObjectShape {
                properties: HashMap::new(),
                function_type: Some(known_incompatible_hook.clone()),
            },
        );

        let known_incompatible_indirect_function = FunctionSignature {
            positional_params: vec![],
            rest_param: Some(Effect::Read),
            return_type: ReturnType::Poly,
            return_value_kind: ValueKind::Mutable,
            callee_effect: Effect::Read,
            known_incompatible: Some(known_incompatible_indirect_message.to_string()),
            ..Default::default()
        };
        self.shapes.insert(
            TEST_KNOWN_INCOMPATIBLE_INDIRECT_FUNCTION_ID,
            ObjectShape {
                properties: HashMap::new(),
                function_type: Some(known_incompatible_indirect_function.clone()),
            },
        );

        let mut known_incompatible_indirect_result_props = HashMap::new();
        known_incompatible_indirect_result_props.insert(
            "incompatible".to_string(),
            PropertyType::Function(known_incompatible_indirect_function),
        );
        self.shapes.insert(
            TEST_KNOWN_INCOMPATIBLE_INDIRECT_RESULT_ID,
            ObjectShape {
                properties: known_incompatible_indirect_result_props,
                function_type: None,
            },
        );

        let known_incompatible_indirect_hook = FunctionSignature {
            positional_params: vec![],
            rest_param: Some(Effect::Read),
            return_type: ReturnType::Object {
                shape_id: TEST_KNOWN_INCOMPATIBLE_INDIRECT_RESULT_ID,
            },
            return_value_kind: ValueKind::Frozen,
            callee_effect: Effect::Read,
            ..Default::default()
        };
        self.shapes.insert(
            TEST_KNOWN_INCOMPATIBLE_INDIRECT_HOOK_ID,
            ObjectShape {
                properties: HashMap::new(),
                function_type: Some(known_incompatible_indirect_hook.clone()),
            },
        );

        let known_incompatible_function = FunctionSignature {
            positional_params: vec![],
            rest_param: Some(Effect::Read),
            return_type: ReturnType::Poly,
            return_value_kind: ValueKind::Mutable,
            callee_effect: Effect::Read,
            known_incompatible: Some(known_incompatible_message.to_string()),
            ..Default::default()
        };
        self.shapes.insert(
            TEST_KNOWN_INCOMPATIBLE_FUNCTION_ID,
            ObjectShape {
                properties: HashMap::new(),
                function_type: Some(known_incompatible_function.clone()),
            },
        );

        let mut known_incompatible_module_props = HashMap::new();
        known_incompatible_module_props.insert(
            "useKnownIncompatible".to_string(),
            PropertyType::Function(known_incompatible_hook),
        );
        known_incompatible_module_props.insert(
            "useKnownIncompatibleIndirect".to_string(),
            PropertyType::Function(known_incompatible_indirect_hook),
        );
        known_incompatible_module_props.insert(
            "knownIncompatible".to_string(),
            PropertyType::Function(known_incompatible_function),
        );
        self.shapes.insert(
            TEST_KNOWN_INCOMPATIBLE_MODULE_ID,
            ObjectShape {
                properties: known_incompatible_module_props,
                function_type: None,
            },
        );

        let invalid_non_hook_typed_as_hook = FunctionSignature {
            positional_params: vec![],
            rest_param: Some(Effect::Freeze),
            return_type: ReturnType::Poly,
            return_value_kind: ValueKind::Frozen,
            callee_effect: Effect::Read,
            ..Default::default()
        };
        self.shapes.insert(
            TEST_INVALID_TYPE_PROVIDER_NON_HOOK_TYPED_AS_HOOK_ID,
            ObjectShape {
                properties: HashMap::new(),
                function_type: Some(invalid_non_hook_typed_as_hook.clone()),
            },
        );

        let mut invalid_type_provider_module_props = HashMap::new();
        invalid_type_provider_module_props
            .insert("useHookNotTypedAsHook".to_string(), PropertyType::Poly);
        invalid_type_provider_module_props.insert(
            "notAhookTypedAsHook".to_string(),
            PropertyType::Function(invalid_non_hook_typed_as_hook),
        );
        self.shapes.insert(
            TEST_INVALID_TYPE_PROVIDER_MODULE_ID,
            ObjectShape {
                properties: invalid_type_provider_module_props,
                function_type: None,
            },
        );

        // Shared runtime type provider used by upstream snap test harness.
        let graphql_function = FunctionSignature {
            positional_params: vec![],
            rest_param: Some(Effect::Read),
            return_type: ReturnType::Primitive,
            return_value_kind: ValueKind::Primitive,
            callee_effect: Effect::Read,
            ..Default::default()
        };
        let typed_array_push_function = FunctionSignature {
            positional_params: vec![Effect::Store, Effect::Capture],
            rest_param: Some(Effect::Capture),
            return_type: ReturnType::Primitive,
            return_value_kind: ValueKind::Primitive,
            callee_effect: Effect::Read,
            ..Default::default()
        };
        let typed_log_function = FunctionSignature {
            positional_params: vec![],
            rest_param: Some(Effect::Read),
            return_type: ReturnType::Primitive,
            return_value_kind: ValueKind::Primitive,
            callee_effect: Effect::Read,
            ..Default::default()
        };
        let typed_identity_function = FunctionSignature {
            positional_params: vec![Effect::Read],
            rest_param: None,
            return_type: ReturnType::Poly,
            return_value_kind: ValueKind::Mutable,
            callee_effect: Effect::Read,
            ..Default::default()
        };
        let typed_assign_function = FunctionSignature {
            positional_params: vec![Effect::Read],
            rest_param: None,
            return_type: ReturnType::Poly,
            return_value_kind: ValueKind::Mutable,
            callee_effect: Effect::Read,
            ..Default::default()
        };
        let typed_alias_function = FunctionSignature {
            positional_params: vec![Effect::Read],
            rest_param: None,
            return_type: ReturnType::Poly,
            return_value_kind: ValueKind::Mutable,
            callee_effect: Effect::Read,
            ..Default::default()
        };
        let typed_capture_function = FunctionSignature {
            positional_params: vec![Effect::Read],
            rest_param: None,
            return_type: ReturnType::Object {
                shape_id: BUILT_IN_ARRAY_ID,
            },
            return_value_kind: ValueKind::Mutable,
            callee_effect: Effect::Read,
            ..Default::default()
        };
        let typed_create_from_function = FunctionSignature {
            positional_params: vec![Effect::Read],
            rest_param: None,
            return_type: ReturnType::Poly,
            return_value_kind: ValueKind::Mutable,
            callee_effect: Effect::Read,
            ..Default::default()
        };
        let typed_mutate_function = FunctionSignature {
            positional_params: vec![Effect::Read, Effect::Capture],
            rest_param: None,
            return_type: ReturnType::Primitive,
            return_value_kind: ValueKind::Primitive,
            callee_effect: Effect::Store,
            ..Default::default()
        };
        self.shapes.insert(
            TEST_SHARED_RUNTIME_GRAPHQL_FN_ID,
            ObjectShape {
                properties: HashMap::new(),
                function_type: Some(graphql_function.clone()),
            },
        );
        self.shapes.insert(
            TEST_SHARED_RUNTIME_TYPED_ARRAY_PUSH_FN_ID,
            ObjectShape {
                properties: HashMap::new(),
                function_type: Some(typed_array_push_function.clone()),
            },
        );
        self.shapes.insert(
            TEST_SHARED_RUNTIME_TYPED_LOG_FN_ID,
            ObjectShape {
                properties: HashMap::new(),
                function_type: Some(typed_log_function.clone()),
            },
        );
        self.shapes.insert(
            TEST_SHARED_RUNTIME_TYPED_IDENTITY_FN_ID,
            ObjectShape {
                properties: HashMap::new(),
                function_type: Some(typed_identity_function.clone()),
            },
        );
        self.shapes.insert(
            TEST_SHARED_RUNTIME_TYPED_ASSIGN_FN_ID,
            ObjectShape {
                properties: HashMap::new(),
                function_type: Some(typed_assign_function.clone()),
            },
        );
        self.shapes.insert(
            TEST_SHARED_RUNTIME_TYPED_ALIAS_FN_ID,
            ObjectShape {
                properties: HashMap::new(),
                function_type: Some(typed_alias_function.clone()),
            },
        );
        self.shapes.insert(
            TEST_SHARED_RUNTIME_TYPED_CAPTURE_FN_ID,
            ObjectShape {
                properties: HashMap::new(),
                function_type: Some(typed_capture_function.clone()),
            },
        );
        self.shapes.insert(
            TEST_SHARED_RUNTIME_TYPED_CREATE_FROM_FN_ID,
            ObjectShape {
                properties: HashMap::new(),
                function_type: Some(typed_create_from_function.clone()),
            },
        );
        self.shapes.insert(
            TEST_SHARED_RUNTIME_TYPED_MUTATE_FN_ID,
            ObjectShape {
                properties: HashMap::new(),
                function_type: Some(typed_mutate_function.clone()),
            },
        );

        let use_freeze_hook = FunctionSignature {
            positional_params: vec![],
            rest_param: Some(Effect::Freeze),
            return_type: ReturnType::Poly,
            return_value_kind: ValueKind::Frozen,
            callee_effect: Effect::Read,
            ..Default::default()
        };
        self.shapes.insert(
            TEST_SHARED_RUNTIME_USE_FREEZE_HOOK_ID,
            ObjectShape {
                properties: HashMap::new(),
                function_type: Some(use_freeze_hook.clone()),
            },
        );

        let use_fragment_hook = FunctionSignature {
            positional_params: vec![],
            rest_param: Some(Effect::Freeze),
            return_type: ReturnType::Object {
                shape_id: BUILT_IN_MIXED_READONLY_ID,
            },
            return_value_kind: ValueKind::Frozen,
            callee_effect: Effect::Read,
            no_alias: true,
            ..Default::default()
        };
        self.shapes.insert(
            TEST_SHARED_RUNTIME_USE_FRAGMENT_HOOK_ID,
            ObjectShape {
                properties: HashMap::new(),
                function_type: Some(use_fragment_hook.clone()),
            },
        );

        let use_no_alias_hook = FunctionSignature {
            positional_params: vec![],
            rest_param: Some(Effect::Freeze),
            return_type: ReturnType::Poly,
            return_value_kind: ValueKind::Mutable,
            callee_effect: Effect::Read,
            no_alias: true,
            ..Default::default()
        };
        self.shapes.insert(
            TEST_SHARED_RUNTIME_USE_NO_ALIAS_HOOK_ID,
            ObjectShape {
                properties: HashMap::new(),
                function_type: Some(use_no_alias_hook.clone()),
            },
        );

        let mut shared_runtime_props = HashMap::new();
        shared_runtime_props.insert(
            "default".to_string(),
            PropertyType::Function(graphql_function.clone()),
        );
        shared_runtime_props.insert(
            "graphql".to_string(),
            PropertyType::Function(graphql_function),
        );
        shared_runtime_props.insert(
            "typedArrayPush".to_string(),
            PropertyType::Function(typed_array_push_function),
        );
        shared_runtime_props.insert(
            "typedLog".to_string(),
            PropertyType::Function(typed_log_function),
        );
        shared_runtime_props.insert(
            "typedIdentity".to_string(),
            PropertyType::Function(typed_identity_function),
        );
        shared_runtime_props.insert(
            "typedAssign".to_string(),
            PropertyType::Function(typed_assign_function),
        );
        shared_runtime_props.insert(
            "typedAlias".to_string(),
            PropertyType::Function(typed_alias_function),
        );
        shared_runtime_props.insert(
            "typedCapture".to_string(),
            PropertyType::Function(typed_capture_function),
        );
        shared_runtime_props.insert(
            "typedCreateFrom".to_string(),
            PropertyType::Function(typed_create_from_function),
        );
        shared_runtime_props.insert(
            "typedMutate".to_string(),
            PropertyType::Function(typed_mutate_function),
        );
        shared_runtime_props.insert(
            "useFreeze".to_string(),
            PropertyType::Function(use_freeze_hook),
        );
        shared_runtime_props.insert(
            "useFragment".to_string(),
            PropertyType::Function(use_fragment_hook),
        );
        shared_runtime_props.insert(
            "useNoAlias".to_string(),
            PropertyType::Function(use_no_alias_hook),
        );
        self.shapes.insert(
            TEST_SHARED_RUNTIME_MODULE_ID,
            ObjectShape {
                properties: shared_runtime_props,
                function_type: None,
            },
        );
    }

    // -----------------------------------------------------------------------
    // Global name registration
    // -----------------------------------------------------------------------

    fn register_globals(&mut self) {
        self.register_react_hooks();
        self.register_react_object();
        self.register_jsx_global();
        self.register_object_global();
        self.register_array_global();
        self.register_math_global();
        self.register_date_global();
        self.register_performance_global();
        self.register_console_global();
        self.register_primitive_globals();
        self.register_collection_constructors();
        self.register_constant_globals();
        self.register_untyped_globals();
        self.register_global_this();
    }

    // -- React hooks --------------------------------------------------------

    fn register_react_hooks(&mut self) {
        // useContext
        self.insert_global(
            "useContext",
            GlobalKind::Hook(make_hook_sig(
                vec![],
                Some(Effect::Read),
                ReturnType::Poly,
                ValueKind::Frozen,
                Some(ValueReason::Context),
            )),
        );

        // useState
        self.insert_global(
            "useState",
            GlobalKind::Hook(make_hook_sig(
                vec![],
                Some(Effect::Freeze),
                ReturnType::Object {
                    shape_id: BUILT_IN_USE_STATE_ID,
                },
                ValueKind::Frozen,
                Some(ValueReason::State),
            )),
        );

        // useActionState
        self.insert_global(
            "useActionState",
            GlobalKind::Hook(make_hook_sig(
                vec![],
                Some(Effect::Freeze),
                ReturnType::Object {
                    shape_id: BUILT_IN_USE_ACTION_STATE_ID,
                },
                ValueKind::Frozen,
                Some(ValueReason::State),
            )),
        );

        // useReducer
        self.insert_global(
            "useReducer",
            GlobalKind::Hook(make_hook_sig(
                vec![],
                Some(Effect::Freeze),
                ReturnType::Object {
                    shape_id: BUILT_IN_USE_REDUCER_ID,
                },
                ValueKind::Frozen,
                Some(ValueReason::ReducerState),
            )),
        );

        // useRef
        self.insert_global(
            "useRef",
            GlobalKind::Hook(make_hook_sig(
                vec![],
                Some(Effect::Capture),
                ReturnType::Object {
                    shape_id: BUILT_IN_USE_REF_ID,
                },
                ValueKind::Mutable,
                None,
            )),
        );

        // useImperativeHandle
        self.insert_global(
            "useImperativeHandle",
            GlobalKind::Hook(make_hook_sig(
                vec![],
                Some(Effect::Freeze),
                ReturnType::Primitive,
                ValueKind::Frozen,
                None,
            )),
        );

        // useMemo
        self.insert_global(
            "useMemo",
            GlobalKind::Hook(make_hook_sig(
                vec![],
                Some(Effect::Freeze),
                ReturnType::Poly,
                ValueKind::Frozen,
                None,
            )),
        );

        // useCallback
        self.insert_global(
            "useCallback",
            GlobalKind::Hook(make_hook_sig(
                vec![],
                Some(Effect::Freeze),
                ReturnType::Poly,
                ValueKind::Frozen,
                None,
            )),
        );

        // useEffect
        self.insert_global(
            "useEffect",
            GlobalKind::Hook(make_hook_sig(
                vec![],
                Some(Effect::Freeze),
                ReturnType::Primitive,
                ValueKind::Frozen,
                None,
            )),
        );

        // useLayoutEffect
        self.insert_global(
            "useLayoutEffect",
            GlobalKind::Hook(make_hook_sig(
                vec![],
                Some(Effect::Freeze),
                ReturnType::Poly,
                ValueKind::Frozen,
                None,
            )),
        );

        // useInsertionEffect
        self.insert_global(
            "useInsertionEffect",
            GlobalKind::Hook(make_hook_sig(
                vec![],
                Some(Effect::Freeze),
                ReturnType::Poly,
                ValueKind::Frozen,
                None,
            )),
        );

        // useTransition
        self.insert_global(
            "useTransition",
            GlobalKind::Hook(make_hook_sig(
                vec![],
                None,
                ReturnType::Object {
                    shape_id: BUILT_IN_USE_TRANSITION_ID,
                },
                ValueKind::Frozen,
                None,
            )),
        );

        // useEffectEvent
        self.insert_global(
            "useEffectEvent",
            GlobalKind::Hook(make_hook_sig(
                vec![],
                Some(Effect::Freeze),
                ReturnType::Function {
                    shape_id: Some(BUILT_IN_EFFECT_EVENT_ID),
                    return_type: Box::new(ReturnType::Poly),
                    is_constructor: false,
                },
                ValueKind::Frozen,
                None,
            )),
        );

        // `use` (not a hook, but a React function)
        self.insert_global(
            "use",
            GlobalKind::Function(FunctionSignature {
                positional_params: vec![],
                rest_param: Some(Effect::Freeze),
                return_type: ReturnType::Poly,
                return_value_kind: ValueKind::Frozen,
                callee_effect: Effect::Read,
                ..Default::default()
            }),
        );

        // fire
        self.insert_global(
            "fire",
            GlobalKind::Function(FunctionSignature {
                positional_params: vec![],
                rest_param: None,
                return_type: ReturnType::Function {
                    shape_id: Some(BUILT_IN_FIRE_FUNCTION_ID),
                    return_type: Box::new(ReturnType::Poly),
                    is_constructor: false,
                },
                return_value_kind: ValueKind::Frozen,
                callee_effect: Effect::Read,
                ..Default::default()
            }),
        );

        // AUTODEPS
        self.insert_global(
            "AUTODEPS",
            GlobalKind::Object {
                shape_id: BUILT_IN_AUTODEPS_ID,
            },
        );
    }

    // -- React object -------------------------------------------------------

    fn register_react_object(&mut self) {
        // We register "React" as an Object global. Its properties
        // (createElement, cloneElement, createRef, and all the hooks)
        // are accessed via PropertyLoad, so we register a shape for it.
        let mut react_props = HashMap::new();

        // All the hooks that are also direct globals get registered as
        // properties on the React object.
        let hook_names = [
            "useContext",
            "useState",
            "useActionState",
            "useReducer",
            "useRef",
            "useImperativeHandle",
            "useMemo",
            "useCallback",
            "useEffect",
            "useLayoutEffect",
            "useInsertionEffect",
            "useTransition",
            "useEffectEvent",
            "use",
            "fire",
        ];
        for name in &hook_names {
            if let Some(gt) = self.globals.get(*name) {
                let pt = match &gt.kind {
                    GlobalKind::Hook(sig) | GlobalKind::Function(sig) => {
                        PropertyType::Function(sig.clone())
                    }
                    GlobalKind::Object { shape_id } => PropertyType::Object { shape_id },
                    GlobalKind::Primitive => PropertyType::Primitive,
                    GlobalKind::Poly => PropertyType::Poly,
                };
                react_props.insert(name.to_string(), pt);
            }
        }

        // createElement: freeze args, return poly frozen
        react_props.insert(
            "createElement".to_string(),
            PropertyType::Function(FunctionSignature {
                positional_params: vec![],
                rest_param: Some(Effect::Freeze),
                return_type: ReturnType::Poly,
                return_value_kind: ValueKind::Frozen,
                callee_effect: Effect::Read,
                ..Default::default()
            }),
        );

        // cloneElement: freeze args, return poly frozen
        react_props.insert(
            "cloneElement".to_string(),
            PropertyType::Function(FunctionSignature {
                positional_params: vec![],
                rest_param: Some(Effect::Freeze),
                return_type: ReturnType::Poly,
                return_value_kind: ValueKind::Frozen,
                callee_effect: Effect::Read,
                ..Default::default()
            }),
        );

        // createRef: capture args, return useRef shape mutable
        react_props.insert(
            "createRef".to_string(),
            PropertyType::Function(FunctionSignature {
                positional_params: vec![],
                rest_param: Some(Effect::Capture),
                return_type: ReturnType::Object {
                    shape_id: BUILT_IN_USE_REF_ID,
                },
                return_value_kind: ValueKind::Mutable,
                callee_effect: Effect::Read,
                ..Default::default()
            }),
        );

        // AUTODEPS: upstream includes this in REACT_APIS which is spread into the
        // React object shape.  PropertyLoad from `React.AUTODEPS` must resolve to
        // the BuiltInAutoDepsId so that infer_effect_dependencies can detect it.
        react_props.insert(
            "AUTODEPS".to_string(),
            PropertyType::Object {
                shape_id: BUILT_IN_AUTODEPS_ID,
            },
        );

        // Register the React shape
        let react_shape_id: ShapeId = "React";
        self.shapes.insert(
            react_shape_id,
            ObjectShape {
                properties: react_props,
                function_type: None,
            },
        );
        self.insert_global(
            "React",
            GlobalKind::Object {
                shape_id: react_shape_id,
            },
        );
    }

    // -- _jsx ---------------------------------------------------------------

    fn register_jsx_global(&mut self) {
        self.insert_global(
            "_jsx",
            GlobalKind::Function(FunctionSignature {
                positional_params: vec![],
                rest_param: Some(Effect::Freeze),
                return_type: ReturnType::Poly,
                return_value_kind: ValueKind::Frozen,
                callee_effect: Effect::Read,
                ..Default::default()
            }),
        );
    }

    // -- Object statics -----------------------------------------------------

    fn register_object_global(&mut self) {
        let mut props = HashMap::new();

        // Object.keys: read arg, return BuiltInArray mutable
        props.insert(
            "keys".to_string(),
            PropertyType::Function(make_pure_fn(
                vec![Effect::Read],
                None,
                ReturnType::Object {
                    shape_id: BUILT_IN_ARRAY_ID,
                },
                ValueKind::Mutable,
            )),
        );

        // Object.values: capture arg, return BuiltInArray mutable
        props.insert(
            "values".to_string(),
            PropertyType::Function(make_pure_fn(
                vec![Effect::Capture],
                None,
                ReturnType::Object {
                    shape_id: BUILT_IN_ARRAY_ID,
                },
                ValueKind::Mutable,
            )),
        );

        // Object.entries: capture arg, return BuiltInArray mutable
        props.insert(
            "entries".to_string(),
            PropertyType::Function(make_pure_fn(
                vec![Effect::Capture],
                None,
                ReturnType::Object {
                    shape_id: BUILT_IN_ARRAY_ID,
                },
                ValueKind::Mutable,
            )),
        );

        // Object.fromEntries: conditionally mutate arg, return BuiltInObject mutable
        props.insert(
            "fromEntries".to_string(),
            PropertyType::Function(make_pure_fn(
                vec![Effect::ConditionallyMutate],
                None,
                ReturnType::Object {
                    shape_id: BUILT_IN_OBJECT_ID,
                },
                ValueKind::Mutable,
            )),
        );

        let object_shape_id: ShapeId = "Object";
        self.shapes.insert(
            object_shape_id,
            ObjectShape {
                properties: props,
                function_type: None,
            },
        );
        self.insert_global(
            "Object",
            GlobalKind::Object {
                shape_id: object_shape_id,
            },
        );
    }

    // -- Array statics ------------------------------------------------------

    fn register_array_global(&mut self) {
        let mut props = HashMap::new();

        // Array.isArray: read arg, return primitive
        props.insert(
            "isArray".to_string(),
            PropertyType::Function(make_pure_fn(
                vec![Effect::Read],
                None,
                ReturnType::Primitive,
                ValueKind::Primitive,
            )),
        );

        // Array.from: ConditionallyMutateIterator + ConditionallyMutate args, return BuiltInArray
        props.insert(
            "from".to_string(),
            PropertyType::Function(make_pure_fn(
                vec![
                    Effect::ConditionallyMutateIterator,
                    Effect::ConditionallyMutate,
                    Effect::ConditionallyMutate,
                ],
                Some(Effect::Read),
                ReturnType::Object {
                    shape_id: BUILT_IN_ARRAY_ID,
                },
                ValueKind::Mutable,
            )),
        );

        // Array.of: read args, return BuiltInArray mutable
        props.insert(
            "of".to_string(),
            PropertyType::Function(make_pure_fn(
                vec![],
                Some(Effect::Read),
                ReturnType::Object {
                    shape_id: BUILT_IN_ARRAY_ID,
                },
                ValueKind::Mutable,
            )),
        );

        let array_shape_id: ShapeId = "Array";
        self.shapes.insert(
            array_shape_id,
            ObjectShape {
                properties: props,
                function_type: None,
            },
        );
        self.insert_global(
            "Array",
            GlobalKind::Object {
                shape_id: array_shape_id,
            },
        );
    }

    // -- Math ---------------------------------------------------------------

    fn register_math_global(&mut self) {
        let mut props = HashMap::new();

        // PI: primitive
        props.insert("PI".to_string(), PropertyType::Primitive);

        // Pure math functions: max, min, pow, trunc, ceil, floor
        for name in &["max", "min", "pow", "trunc", "ceil", "floor"] {
            props.insert(
                name.to_string(),
                PropertyType::Function(make_read_to_primitive()),
            );
        }

        // random: impure
        props.insert(
            "random".to_string(),
            PropertyType::Function(make_impure_fn("Math.random")),
        );

        let math_shape_id: ShapeId = "Math";
        self.shapes.insert(
            math_shape_id,
            ObjectShape {
                properties: props,
                function_type: None,
            },
        );
        self.insert_global(
            "Math",
            GlobalKind::Object {
                shape_id: math_shape_id,
            },
        );
    }

    // -- Date ---------------------------------------------------------------

    fn register_date_global(&mut self) {
        let mut props = HashMap::new();
        props.insert(
            "now".to_string(),
            PropertyType::Function(make_impure_fn("Date.now")),
        );

        let date_shape_id: ShapeId = "Date";
        self.shapes.insert(
            date_shape_id,
            ObjectShape {
                properties: props,
                function_type: None,
            },
        );
        self.insert_global(
            "Date",
            GlobalKind::Object {
                shape_id: date_shape_id,
            },
        );
    }

    // -- performance --------------------------------------------------------

    fn register_performance_global(&mut self) {
        let mut props = HashMap::new();
        props.insert(
            "now".to_string(),
            PropertyType::Function(make_impure_fn("performance.now")),
        );

        let perf_shape_id: ShapeId = "performance";
        self.shapes.insert(
            perf_shape_id,
            ObjectShape {
                properties: props,
                function_type: None,
            },
        );
        self.insert_global(
            "performance",
            GlobalKind::Object {
                shape_id: perf_shape_id,
            },
        );
    }

    // -- console ------------------------------------------------------------

    fn register_console_global(&mut self) {
        let mut props = HashMap::new();

        for name in &["error", "info", "log", "table", "trace", "warn"] {
            props.insert(
                name.to_string(),
                PropertyType::Function(make_read_to_primitive()),
            );
        }

        let console_shape_id: ShapeId = "console";
        self.shapes.insert(
            console_shape_id,
            ObjectShape {
                properties: props,
                function_type: None,
            },
        );
        self.insert_global(
            "console",
            GlobalKind::Object {
                shape_id: console_shape_id,
            },
        );
    }

    // -- Primitive globals --------------------------------------------------

    fn register_primitive_globals(&mut self) {
        // Functions that read args and return primitives
        let primitive_fns = [
            "Boolean",
            "Number",
            "String",
            "parseInt",
            "parseFloat",
            "isNaN",
            "isFinite",
            "encodeURI",
            "encodeURIComponent",
            "decodeURI",
            "decodeURIComponent",
        ];
        for name in &primitive_fns {
            self.insert_global(name, GlobalKind::Function(make_read_to_primitive()));
        }
    }

    // -- Collection constructors --------------------------------------------

    fn register_collection_constructors(&mut self) {
        // Map constructor
        self.insert_global(
            "Map",
            GlobalKind::Function(FunctionSignature {
                positional_params: vec![Effect::ConditionallyMutateIterator],
                rest_param: None,
                return_type: ReturnType::Object {
                    shape_id: BUILT_IN_MAP_ID,
                },
                return_value_kind: ValueKind::Mutable,
                callee_effect: Effect::Read,
                ..Default::default()
            }),
        );

        // Set constructor
        self.insert_global(
            "Set",
            GlobalKind::Function(FunctionSignature {
                positional_params: vec![Effect::ConditionallyMutateIterator],
                rest_param: None,
                return_type: ReturnType::Object {
                    shape_id: BUILT_IN_SET_ID,
                },
                return_value_kind: ValueKind::Mutable,
                callee_effect: Effect::Read,
                ..Default::default()
            }),
        );

        // WeakMap constructor
        self.insert_global(
            "WeakMap",
            GlobalKind::Function(FunctionSignature {
                positional_params: vec![Effect::ConditionallyMutateIterator],
                rest_param: None,
                return_type: ReturnType::Object {
                    shape_id: BUILT_IN_WEAK_MAP_ID,
                },
                return_value_kind: ValueKind::Mutable,
                callee_effect: Effect::Read,
                ..Default::default()
            }),
        );

        // WeakSet constructor
        self.insert_global(
            "WeakSet",
            GlobalKind::Function(FunctionSignature {
                positional_params: vec![Effect::ConditionallyMutateIterator],
                rest_param: None,
                return_type: ReturnType::Object {
                    shape_id: BUILT_IN_WEAK_SET_ID,
                },
                return_value_kind: ValueKind::Mutable,
                callee_effect: Effect::Read,
                ..Default::default()
            }),
        );
    }

    // -- Constant globals ---------------------------------------------------

    fn register_constant_globals(&mut self) {
        self.insert_global("Infinity", GlobalKind::Primitive);
        self.insert_global("NaN", GlobalKind::Primitive);
    }

    // -- Untyped globals (Poly) ---------------------------------------------

    fn register_untyped_globals(&mut self) {
        let untyped = [
            "Function",
            "RegExp",
            "Error",
            "TypeError",
            "RangeError",
            "ReferenceError",
            "SyntaxError",
            "URIError",
            "EvalError",
            "DataView",
            "Float32Array",
            "Float64Array",
            "Int8Array",
            "Int16Array",
            "Int32Array",
            "Uint8Array",
            "Uint8ClampedArray",
            "Uint16Array",
            "Uint32Array",
            "ArrayBuffer",
            "JSON",
            "eval",
        ];
        for name in &untyped {
            // Only insert if not already registered (e.g. Object, console, Date
            // are in UNTYPED_GLOBALS upstream but also in TYPED_GLOBALS --
            // TYPED_GLOBALS wins).
            if !self.globals.contains_key(*name) {
                self.insert_global(name, GlobalKind::Poly);
            }
        }
    }

    // -- global / globalThis --------------------------------------------------

    /// Register `global` and `globalThis` as objects whose shape contains all
    /// the typed globals (console, Math, Object, Array, etc.). This mirrors
    /// upstream's recursive global types:
    ///   DEFAULT_GLOBALS.set('global',     addObject(DEFAULT_SHAPES, 'global',     TYPED_GLOBALS));
    ///   DEFAULT_GLOBALS.set('globalThis', addObject(DEFAULT_SHAPES, 'globalThis', TYPED_GLOBALS));
    fn register_global_this(&mut self) {
        // Build properties from all currently-registered typed globals.
        let mut props = HashMap::new();
        for (name, global_type) in &self.globals {
            let prop = match &global_type.kind {
                GlobalKind::Object { shape_id } => PropertyType::Object { shape_id },
                GlobalKind::Function(sig) => PropertyType::Function(sig.clone()),
                GlobalKind::Hook(sig) => PropertyType::Function(sig.clone()),
                GlobalKind::Primitive => PropertyType::Primitive,
                GlobalKind::Poly => PropertyType::Poly,
            };
            props.insert(name.clone(), prop);
        }

        // Register the `global` shape and global.
        let global_shape_id: ShapeId = "global";
        self.shapes.insert(
            global_shape_id,
            ObjectShape {
                properties: props.clone(),
                function_type: None,
            },
        );
        self.insert_global(
            "global",
            GlobalKind::Object {
                shape_id: global_shape_id,
            },
        );

        // Register the `globalThis` shape and global.
        let global_this_shape_id: ShapeId = "globalThis";
        self.shapes.insert(
            global_this_shape_id,
            ObjectShape {
                properties: props,
                function_type: None,
            },
        );
        self.insert_global(
            "globalThis",
            GlobalKind::Object {
                shape_id: global_this_shape_id,
            },
        );
    }

    // -- Helper to insert a global ------------------------------------------

    fn insert_global(&mut self, name: &str, kind: GlobalKind) {
        self.globals.insert(name.to_string(), GlobalType { kind });
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_creates_without_panic() {
        let registry = GlobalRegistry::new();
        // Spot-check a few shapes exist
        assert!(registry.shapes.get(BUILT_IN_ARRAY_ID).is_some());
        assert!(registry.shapes.get(BUILT_IN_OBJECT_ID).is_some());
        assert!(registry.shapes.get(BUILT_IN_SET_ID).is_some());
        assert!(registry.shapes.get(BUILT_IN_MAP_ID).is_some());
        assert!(registry.shapes.get(BUILT_IN_WEAK_SET_ID).is_some());
        assert!(registry.shapes.get(BUILT_IN_WEAK_MAP_ID).is_some());
        assert!(registry.shapes.get(BUILT_IN_USE_STATE_ID).is_some());
        assert!(registry.shapes.get(BUILT_IN_USE_REDUCER_ID).is_some());
        assert!(registry.shapes.get(BUILT_IN_USE_TRANSITION_ID).is_some());
        assert!(registry.shapes.get(BUILT_IN_USE_REF_ID).is_some());
        assert!(registry.shapes.get(BUILT_IN_REF_VALUE_ID).is_some());
        assert!(registry.shapes.get(BUILT_IN_MIXED_READONLY_ID).is_some());
        assert!(registry.shapes.get(BUILT_IN_JSX_ID).is_some());
        assert!(registry.shapes.get(BUILT_IN_FUNCTION_ID).is_some());
    }

    #[test]
    fn array_shape_has_map_method() {
        let registry = GlobalRegistry::new();
        let prop = registry.shapes.get_property(BUILT_IN_ARRAY_ID, "map");
        assert!(prop.is_some());
        match prop.unwrap() {
            PropertyType::Function(sig) => {
                assert!(sig.no_alias);
                assert!(sig.mutable_only_if_operands_are_mutable);
                assert_eq!(sig.callee_effect, Effect::ConditionallyMutate);
            }
            _ => panic!("Expected Function property type for Array.map"),
        }
    }

    #[test]
    fn globals_have_react_hooks() {
        let registry = GlobalRegistry::new();
        assert!(registry.get_global("useState").is_some());
        assert!(registry.get_global("useEffect").is_some());
        assert!(registry.get_global("useContext").is_some());
        assert!(registry.get_global("useRef").is_some());
        assert!(registry.get_global("useMemo").is_some());
        assert!(registry.get_global("useCallback").is_some());
    }

    #[test]
    fn globals_have_math() {
        let registry = GlobalRegistry::new();
        let math = registry.get_global("Math");
        assert!(math.is_some());
        match &math.unwrap().kind {
            GlobalKind::Object { shape_id } => {
                let random = registry.shapes.get_property(shape_id, "random");
                assert!(random.is_some());
                match random.unwrap() {
                    PropertyType::Function(sig) => {
                        assert!(sig.impure);
                        assert_eq!(sig.canonical_name.as_deref(), Some("Math.random"));
                    }
                    _ => panic!("Expected Function for Math.random"),
                }
            }
            _ => panic!("Expected Object kind for Math global"),
        }
    }

    #[test]
    fn globals_have_collection_constructors() {
        let registry = GlobalRegistry::new();
        for name in &["Map", "Set", "WeakMap", "WeakSet"] {
            let g = registry.get_global(name);
            assert!(g.is_some(), "Missing global: {}", name);
            match &g.unwrap().kind {
                GlobalKind::Function(sig) => {
                    assert_eq!(sig.return_value_kind, ValueKind::Mutable);
                }
                _ => panic!("Expected Function kind for {} constructor", name),
            }
        }
    }

    #[test]
    fn use_state_shape_has_setter() {
        let registry = GlobalRegistry::new();
        let prop = registry.shapes.get_property(BUILT_IN_USE_STATE_ID, "1");
        assert!(prop.is_some());
        match prop.unwrap() {
            PropertyType::Function(sig) => {
                assert_eq!(sig.rest_param, Some(Effect::Freeze));
                assert_eq!(sig.return_value_kind, ValueKind::Primitive);
            }
            _ => panic!("Expected Function for useState[1]"),
        }
    }

    #[test]
    fn mixed_readonly_wildcard() {
        let registry = GlobalRegistry::new();
        // Accessing an unknown property on MixedReadonly should fall back to "*"
        let prop = registry
            .shapes
            .get_property(BUILT_IN_MIXED_READONLY_ID, "unknownProp");
        assert!(prop.is_some());
        match prop.unwrap() {
            PropertyType::Object { shape_id } => {
                assert_eq!(*shape_id, BUILT_IN_MIXED_READONLY_ID);
            }
            _ => panic!("Expected Object type for wildcard fallback"),
        }
    }

    #[test]
    fn react_object_has_create_element() {
        let registry = GlobalRegistry::new();
        let react = registry.get_global("React");
        assert!(react.is_some());
        match &react.unwrap().kind {
            GlobalKind::Object { shape_id } => {
                let prop = registry.shapes.get_property(shape_id, "createElement");
                assert!(prop.is_some());
            }
            _ => panic!("Expected Object kind for React global"),
        }
    }
}
