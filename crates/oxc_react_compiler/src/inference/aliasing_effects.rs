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
        /// Whether any lowered parameter has a non-trivial mutable range.
        /// This matches upstream's non-mutating callback check for frozen lambdas.
        mutates_inputs: bool,
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
