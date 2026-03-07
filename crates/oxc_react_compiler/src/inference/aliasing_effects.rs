//! Aliasing effect types.
//!
//! Port of `AliasingEffects.ts` from upstream React Compiler (babel-plugin-react-compiler).
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.
//!
//! This module defines the `AliasingEffect` type which describes effects that
//! instructions and terminals have on values, used for inference of value kinds
//! and mutation/aliasing analysis.

use crate::error::CompilerDiagnostic;
use crate::hir::types::{IdentifierId, Place, SourceLocation, ValueKind, ValueReason};

// ---------------------------------------------------------------------------
// AliasingEffect
// ---------------------------------------------------------------------------

/// Describes effects that an instruction or terminal has on values.
///
/// Each variant represents a different kind of data flow or mutation effect
/// that the compiler tracks during inference.
#[derive(Debug, Clone)]
pub enum AliasingEffect {
    /// Marks the value (and its direct aliases) as frozen (immutable).
    Freeze { value: Place, reason: ValueReason },

    /// Mutates the value and its direct aliases.
    Mutate {
        value: Place,
        reason: Option<MutationReason>,
    },

    /// Mutates the value only if it is known to be mutable.
    MutateConditionally { value: Place },

    /// Mutates the value, its aliases, and all transitive captures.
    MutateTransitive { value: Place },

    /// Mutates mutable values among direct and transitive captures.
    MutateTransitiveConditionally { value: Place },

    /// Information flow where local mutation of `into` does NOT mutate `from`.
    Capture { from: Place, into: Place },

    /// Information flow where local mutation of `into` DOES mutate `from`.
    Alias { from: Place, into: Place },

    /// Potential information flow (used for unknown function signatures).
    MaybeAlias { from: Place, into: Place },

    /// Direct assignment: `into = from`.
    Assign { from: Place, into: Place },

    /// Create a value of the given kind at a place.
    Create {
        into: Place,
        value: ValueKind,
        reason: ValueReason,
    },

    /// Create a new value with the same kind as the source value.
    CreateFrom { from: Place, into: Place },

    /// Immutable data flow for escape analysis.
    ImmutableCapture { from: Place, into: Place },

    /// Call a function with arguments, capturing/aliasing the result.
    Apply {
        receiver: Place,
        function: Place,
        mutates_function: bool,
        args: Vec<ApplyArg>,
        into: Box<Place>,
        signature: Option<crate::hir::object_shape::FunctionSignature>,
        loc: SourceLocation,
    },

    /// Construct a function value from a set of captured places.
    CreateFunction {
        captures: Vec<Place>,
        into: Place,
        /// Signature derived from the locally known function expression/object method.
        /// Used to substitute `Apply` effects at callsites.
        signature: Option<AliasingSignature>,
        /// Context places referenced by the lowered function's aliasing effects.
        context: Vec<Place>,
    },

    /// Error: mutation of an immutable (frozen) value.
    MutateFrozen {
        place: Place,
        error: CompilerDiagnostic,
    },

    /// Error: mutation of a global value.
    MutateGlobal {
        place: Place,
        error: CompilerDiagnostic,
    },

    /// Side-effect that is not safe during render.
    Impure {
        place: Place,
        error: CompilerDiagnostic,
    },

    /// Value accessed during render.
    Render { place: Place },
}

// ---------------------------------------------------------------------------
// MutationReason
// ---------------------------------------------------------------------------

/// Reason for a mutation effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MutationReason {
    /// The mutation is an assignment to a property of the current value.
    AssignCurrentProperty,
}

// ---------------------------------------------------------------------------
// ApplyArg
// ---------------------------------------------------------------------------

/// An argument to an `Apply` effect.
#[derive(Debug, Clone)]
pub enum ApplyArg {
    /// A regular positional argument.
    Place(Place),
    /// A spread argument.
    Spread(Place),
    /// A hole (elided argument).
    Hole,
}

// ---------------------------------------------------------------------------
// FunctionSignature
// ---------------------------------------------------------------------------

/// Describes the signature of a function for aliasing analysis.
///
/// This is a placeholder that will be expanded when the Globals/ObjectShape
/// system is ported.
#[derive(Debug, Clone)]
pub struct FunctionSignature {
    /// Effects for each positional parameter.
    pub positional_params: Vec<SignatureEffect>,
    /// Effect for the rest parameter, if any.
    pub rest_param: Option<SignatureEffect>,
    /// The return type category.
    pub return_type: SignatureReturnType,
    /// How the function is called (method, static, constructor).
    pub call_kind: CallKind,
}

/// Effect that a function has on a parameter.
#[derive(Debug, Clone)]
pub enum SignatureEffect {
    /// The parameter is frozen after the call.
    Freeze,
    /// The parameter is only read.
    Read,
    /// The parameter is captured into the specified target.
    Capture { into: SignatureTarget },
    /// The parameter is mutated.
    Mutate,
}

/// Target of a capture effect in a function signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureTarget {
    /// Captured into the return value.
    Return,
    /// Captured into another parameter (by index).
    Param(usize),
}

/// Category of a function's return type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureReturnType {
    /// Returns an object (mutable).
    Object,
    /// Returns a primitive (immutable).
    Primitive,
    /// Polymorphic return type.
    Poly,
}

/// How a function is called.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallKind {
    /// Called as a method on a receiver (e.g., `obj.method()`).
    Method,
    /// Called as a static/free function (e.g., `fn()`).
    StaticFunction,
    /// Called as a constructor (e.g., `new Foo()`).
    Constructor,
}

// ---------------------------------------------------------------------------
// AliasingSignature
// ---------------------------------------------------------------------------

/// Aliasing signature for a function call.
///
/// Describes the aliasing effects of calling a function, including the
/// receiver, parameters, return value, and any temporary values created.
#[derive(Debug, Clone)]
pub struct AliasingSignature {
    /// The receiver (`this` context) identifier.
    pub receiver: IdentifierId,
    /// Positional parameter identifiers.
    pub params: Vec<IdentifierId>,
    /// Rest parameter identifier, if any.
    pub rest: Option<IdentifierId>,
    /// Return value identifier.
    pub returns: IdentifierId,
    /// Effects of the function call.
    pub effects: Vec<AliasingEffect>,
    /// Temporary values created during the call.
    pub temporaries: Vec<Place>,
}
