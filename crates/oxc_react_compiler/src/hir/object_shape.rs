//! Object shape types and shape registry.
//!
//! Port of `ObjectShape.ts` from upstream React Compiler (babel-plugin-react-compiler).
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.
//!
//! This module defines function signatures and object shapes used for type and
//! effect inference. Shapes describe the properties and call signatures of
//! built-in objects (Array, Set, Map, etc.) and React hooks so that the compiler
//! can reason about which values are read, mutated, captured, or frozen.

use std::collections::HashMap;

use crate::hir::types::{Effect, ValueKind, ValueReason};

// ---------------------------------------------------------------------------
// Shape ID
// ---------------------------------------------------------------------------

/// Identifies a shape in the registry.
pub type ShapeId = &'static str;

// Built-in shape IDs -- must match the upstream string constants exactly.
pub const BUILT_IN_ARRAY_ID: ShapeId = "BuiltInArray";
pub const BUILT_IN_OBJECT_ID: ShapeId = "BuiltInObject";
pub const BUILT_IN_SET_ID: ShapeId = "BuiltInSet";
pub const BUILT_IN_MAP_ID: ShapeId = "BuiltInMap";
pub const BUILT_IN_WEAK_SET_ID: ShapeId = "BuiltInWeakSet";
pub const BUILT_IN_WEAK_MAP_ID: ShapeId = "BuiltInWeakMap";
pub const BUILT_IN_FUNCTION_ID: ShapeId = "BuiltInFunction";
pub const BUILT_IN_JSX_ID: ShapeId = "BuiltInJsx";
pub const BUILT_IN_PROPS_ID: ShapeId = "BuiltInProps";
pub const BUILT_IN_USE_STATE_ID: ShapeId = "BuiltInUseState";
pub const BUILT_IN_SET_STATE_ID: ShapeId = "BuiltInSetState";
pub const BUILT_IN_USE_ACTION_STATE_ID: ShapeId = "BuiltInUseActionState";
pub const BUILT_IN_SET_ACTION_STATE_ID: ShapeId = "BuiltInSetActionState";
pub const BUILT_IN_USE_REF_ID: ShapeId = "BuiltInUseRefId";
pub const BUILT_IN_REF_VALUE_ID: ShapeId = "BuiltInRefValue";
pub const BUILT_IN_MIXED_READONLY_ID: ShapeId = "BuiltInMixedReadonly";
pub const BUILT_IN_USE_REDUCER_ID: ShapeId = "BuiltInUseReducer";
pub const BUILT_IN_DISPATCH_ID: ShapeId = "BuiltInDispatch";
pub const BUILT_IN_USE_TRANSITION_ID: ShapeId = "BuiltInUseTransition";
pub const BUILT_IN_START_TRANSITION_ID: ShapeId = "BuiltInStartTransition";
pub const BUILT_IN_FIRE_FUNCTION_ID: ShapeId = "BuiltInFireFunction";
pub const BUILT_IN_USE_EFFECT_EVENT_ID: ShapeId = "BuiltInUseEffectEvent";
pub const BUILT_IN_EFFECT_EVENT_ID: ShapeId = "BuiltInEffectEventFunction";
pub const BUILT_IN_AUTODEPS_ID: ShapeId = "BuiltInAutoDepsId";
pub const REANIMATED_SHARED_VALUE_ID: ShapeId = "ReanimatedSharedValueId";
pub const REANIMATED_MODULE_ID: ShapeId = "ReanimatedModuleId";
pub const REANIMATED_FROZEN_HOOK_ID: ShapeId = "ReanimatedFrozenHookId";
pub const REANIMATED_MUTABLE_HOOK_ID: ShapeId = "ReanimatedMutableHookId";
pub const REANIMATED_MUTABLE_FUNCTION_ID: ShapeId = "ReanimatedMutableFunctionId";
pub const TEST_KNOWN_INCOMPATIBLE_MODULE_ID: ShapeId = "ReactCompilerKnownIncompatibleTestModule";
pub const TEST_KNOWN_INCOMPATIBLE_HOOK_ID: ShapeId = "ReactCompilerKnownIncompatibleHook";
pub const TEST_KNOWN_INCOMPATIBLE_INDIRECT_HOOK_ID: ShapeId =
    "ReactCompilerKnownIncompatibleIndirectHook";
pub const TEST_KNOWN_INCOMPATIBLE_INDIRECT_RESULT_ID: ShapeId =
    "ReactCompilerKnownIncompatibleIndirectResult";
pub const TEST_KNOWN_INCOMPATIBLE_FUNCTION_ID: ShapeId = "ReactCompilerKnownIncompatibleFunction";
pub const TEST_KNOWN_INCOMPATIBLE_INDIRECT_FUNCTION_ID: ShapeId =
    "ReactCompilerKnownIncompatibleIndirectFunction";
pub const TEST_INVALID_TYPE_PROVIDER_MODULE_ID: ShapeId = "ReactCompilerTestModule";
pub const TEST_INVALID_TYPE_PROVIDER_NON_HOOK_TYPED_AS_HOOK_ID: ShapeId =
    "ReactCompilerTestNotAHookTypedAsHook";
pub const TEST_SHARED_RUNTIME_MODULE_ID: ShapeId = "SharedRuntimeTestModule";
pub const TEST_SHARED_RUNTIME_USE_FRAGMENT_HOOK_ID: ShapeId = "SharedRuntimeUseFragmentHook";
pub const TEST_SHARED_RUNTIME_USE_NO_ALIAS_HOOK_ID: ShapeId = "SharedRuntimeUseNoAliasHook";
pub const TEST_SHARED_RUNTIME_USE_FREEZE_HOOK_ID: ShapeId = "SharedRuntimeUseFreezeHook";
pub const TEST_SHARED_RUNTIME_GRAPHQL_FN_ID: ShapeId = "SharedRuntimeGraphqlFn";
pub const TEST_SHARED_RUNTIME_TYPED_ARRAY_PUSH_FN_ID: ShapeId = "SharedRuntimeTypedArrayPushFn";
pub const TEST_SHARED_RUNTIME_TYPED_LOG_FN_ID: ShapeId = "SharedRuntimeTypedLogFn";
pub const TEST_SHARED_RUNTIME_TYPED_IDENTITY_FN_ID: ShapeId = "SharedRuntimeTypedIdentityFn";
pub const TEST_SHARED_RUNTIME_TYPED_ASSIGN_FN_ID: ShapeId = "SharedRuntimeTypedAssignFn";
pub const TEST_SHARED_RUNTIME_TYPED_ALIAS_FN_ID: ShapeId = "SharedRuntimeTypedAliasFn";
pub const TEST_SHARED_RUNTIME_TYPED_CAPTURE_FN_ID: ShapeId = "SharedRuntimeTypedCaptureFn";
pub const TEST_SHARED_RUNTIME_TYPED_CREATE_FROM_FN_ID: ShapeId = "SharedRuntimeTypedCreateFromFn";
pub const TEST_SHARED_RUNTIME_TYPED_MUTATE_FN_ID: ShapeId = "SharedRuntimeTypedMutateFn";

// ---------------------------------------------------------------------------
// Return type
// ---------------------------------------------------------------------------

/// The return type of a function, mirroring upstream `BuiltInType | PolyType`.
#[derive(Debug, Clone)]
pub enum ReturnType {
    Primitive,
    Object {
        shape_id: ShapeId,
    },
    Function {
        shape_id: Option<ShapeId>,
        return_type: Box<ReturnType>,
        is_constructor: bool,
    },
    Poly,
}

// ---------------------------------------------------------------------------
// Function signature
// ---------------------------------------------------------------------------

/// Call signature of a function, used for type and effect inference.
///
/// Mirrors upstream `FunctionSignature` from ObjectShape.ts.
#[derive(Debug, Clone)]
pub struct FunctionSignature {
    pub positional_params: Vec<Effect>,
    pub rest_param: Option<Effect>,
    pub return_type: ReturnType,
    pub return_value_kind: ValueKind,
    pub return_value_reason: Option<ValueReason>,
    pub callee_effect: Effect,
    /// When true, parameters are guaranteed not to alias each other or the
    /// return value. The compiler may skip memoizing arguments that do not
    /// otherwise escape.
    pub no_alias: bool,
    /// When true (methods only), the method can only modify its receiver if
    /// any of the arguments are mutable or are function expressions which
    /// mutate their arguments.
    pub mutable_only_if_operands_are_mutable: bool,
    /// Marks a function as impure (e.g. `Math.random`, `Date.now`).
    pub impure: bool,
    /// Canonical name for diagnostics (e.g. "Math.random").
    pub canonical_name: Option<String>,
    /// Marks a function/hook as known incompatible with inferred memoization.
    pub known_incompatible: Option<String>,
}

impl Default for FunctionSignature {
    fn default() -> Self {
        Self {
            positional_params: Vec::new(),
            rest_param: None,
            return_type: ReturnType::Poly,
            return_value_kind: ValueKind::Mutable,
            return_value_reason: None,
            callee_effect: Effect::Read,
            no_alias: false,
            mutable_only_if_operands_are_mutable: false,
            impure: false,
            canonical_name: None,
            known_incompatible: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Property type
// ---------------------------------------------------------------------------

/// Type of a property on an object shape. A property is either a primitive
/// value, an object with a known shape, a callable function, or an unknown
/// poly type.
#[derive(Debug, Clone)]
pub enum PropertyType {
    Primitive,
    Object { shape_id: ShapeId },
    Function(FunctionSignature),
    Poly,
}

// ---------------------------------------------------------------------------
// Object shape
// ---------------------------------------------------------------------------

/// An object shape describes the known properties and optional call signature
/// of a built-in type. For callable objects (constructors, hooks, plain
/// functions) `function_type` holds the call signature.
#[derive(Debug, Clone, Default)]
pub struct ObjectShape {
    pub properties: HashMap<String, PropertyType>,
    pub function_type: Option<FunctionSignature>,
}

// ---------------------------------------------------------------------------
// Shape registry
// ---------------------------------------------------------------------------

/// Registry of all known shapes keyed by shape ID.
pub struct ShapeRegistry {
    shapes: HashMap<&'static str, ObjectShape>,
}

impl ShapeRegistry {
    pub fn new() -> Self {
        Self {
            shapes: HashMap::new(),
        }
    }

    pub fn get(&self, id: &str) -> Option<&ObjectShape> {
        self.shapes.get(id)
    }

    /// Look up a property on a shape. Falls back to the wildcard `"*"`
    /// property if the exact name is not found.
    pub fn get_property(&self, shape_id: &str, property: &str) -> Option<&PropertyType> {
        self.shapes
            .get(shape_id)
            .and_then(|s| s.properties.get(property).or_else(|| s.properties.get("*")))
    }

    pub fn insert(&mut self, id: &'static str, shape: ObjectShape) {
        self.shapes.insert(id, shape);
    }
}

impl Default for ShapeRegistry {
    fn default() -> Self {
        Self::new()
    }
}
