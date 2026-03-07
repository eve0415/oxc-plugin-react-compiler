//! HIR type definitions.
//!
//! Port of `HIR.ts` from upstream React Compiler (babel-plugin-react-compiler).
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.
//!
//! This module defines the core types for the compiler's intermediate representation.
//! The HIR is a control-flow graph of basic blocks, each containing a sequence of
//! instructions and a terminal.

use std::collections::{HashMap, HashSet};

use indexmap::IndexMap;

// ---------------------------------------------------------------------------
// Opaque ID types
// ---------------------------------------------------------------------------

macro_rules! define_id {
    ($name:ident) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
        pub struct $name(pub u32);

        impl $name {
            pub fn new(id: u32) -> Self {
                Self(id)
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}", self.0)
            }
        }
    };
}

define_id!(BlockId);
define_id!(InstructionId);
define_id!(IdentifierId);
define_id!(DeclarationId);
define_id!(ScopeId);
define_id!(TypeId);

// ---------------------------------------------------------------------------
// Source location
// ---------------------------------------------------------------------------

/// Represents a location in source code. `Generated` means the code was
/// synthesized by the compiler and has no single source location.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub enum SourceLocation {
    Source(SourceRange),
    Generated,
}

impl Default for SourceLocation {
    fn default() -> Self {
        Self::Generated
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct SourceRange {
    pub start: SourcePosition,
    pub end: SourcePosition,
}

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct SourcePosition {
    pub line: u32,
    pub column: u32,
}

// ---------------------------------------------------------------------------
// Types (port of Types.ts)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum Type {
    Primitive,
    Function {
        shape_id: Option<String>,
        return_type: Box<Type>,
        is_constructor: bool,
    },
    Object {
        shape_id: Option<String>,
    },
    TypeVar {
        id: TypeId,
    },
    Poly,
    Phi {
        operands: Vec<Type>,
    },
    Property {
        object_type: Box<Type>,
        object_name: String,
        property_name: PropertyName,
    },
    ObjectMethod,
}

impl Default for Type {
    fn default() -> Self {
        Self::Poly
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum PropertyName {
    Literal(PropertyLiteral),
    Computed(Box<Type>),
}

// ---------------------------------------------------------------------------
// Effect
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    Unknown,
    Freeze,
    Read,
    Capture,
    ConditionallyMutateIterator,
    ConditionallyMutate,
    Mutate,
    Store,
}

impl Default for Effect {
    fn default() -> Self {
        Self::Unknown
    }
}

// ---------------------------------------------------------------------------
// Value kinds
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ValueKind {
    MaybeFrozen,
    Frozen,
    Primitive,
    Global,
    Mutable,
    Context,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ValueReason {
    Global,
    JsxCaptured,
    HookCaptured,
    HookReturn,
    Effect,
    KnownReturnSignature,
    Context,
    State,
    ReducerState,
    ReactiveFunctionArgument,
    Other,
}

// ---------------------------------------------------------------------------
// Place & Identifier
// ---------------------------------------------------------------------------

/// A place where data may be read from / written to.
#[derive(Debug, Clone)]
pub struct Place {
    pub identifier: Identifier,
    pub effect: Effect,
    pub reactive: bool,
    pub loc: SourceLocation,
}

/// An identifier in SSA form.
#[derive(Debug, Clone)]
pub struct Identifier {
    /// After EnterSSA, uniquely identifies an SSA instance of a variable.
    pub id: IdentifierId,
    /// Uniquely identifies a variable declaration in the original program.
    pub declaration_id: DeclarationId,
    /// None for temporaries.
    pub name: Option<IdentifierName>,
    /// Range of instructions where this value is mutable.
    pub mutable_range: MutableRange,
    /// The reactive scope that computes this value.
    pub scope: Option<Box<ReactiveScope>>,
    pub type_: Type,
    pub loc: SourceLocation,
}

#[derive(Debug, Clone)]
pub enum IdentifierName {
    Named(String),
    Promoted(String),
}

impl IdentifierName {
    pub fn value(&self) -> &str {
        match self {
            Self::Named(v) | Self::Promoted(v) => v,
        }
    }
}

/// Range in which an identifier is mutable.
/// Start is inclusive, end is exclusive.
#[derive(Debug, Clone, Default)]
pub struct MutableRange {
    pub start: InstructionId,
    pub end: InstructionId,
}

// ---------------------------------------------------------------------------
// Instructions
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstructionKind {
    Const,
    Let,
    Reassign,
    Catch,
    HoistedConst,
    HoistedLet,
    HoistedFunction,
    Function,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeAnnotationKind {
    Cast,
    As,
    Satisfies,
}

/// An instruction is a single operation within a basic block.
#[derive(Debug, Clone)]
pub struct Instruction {
    pub id: InstructionId,
    pub lvalue: Place,
    pub value: InstructionValue,
    pub loc: SourceLocation,
    /// Aliasing effects computed by inferMutationAliasingEffects. None until that pass runs.
    pub effects: Option<Vec<crate::inference::aliasing_effects::AliasingEffect>>,
}

/// The value produced by an instruction.
/// Port of the `InstructionValue` discriminated union from HIR.ts.
#[derive(Debug, Clone)]
pub enum InstructionValue {
    // Variable operations
    LoadLocal {
        place: Place,
        loc: SourceLocation,
    },
    LoadContext {
        place: Place,
        loc: SourceLocation,
    },
    DeclareLocal {
        lvalue: LValue,
        loc: SourceLocation,
    },
    DeclareContext {
        lvalue: LValue,
        loc: SourceLocation,
    },
    StoreLocal {
        lvalue: LValue,
        value: Place,
        loc: SourceLocation,
    },
    StoreContext {
        lvalue: LValue,
        value: Place,
        loc: SourceLocation,
    },
    Destructure {
        lvalue: LValuePattern,
        value: Place,
        loc: SourceLocation,
    },

    // Literals
    Primitive {
        value: PrimitiveValue,
        loc: SourceLocation,
    },
    JSXText {
        value: String,
        loc: SourceLocation,
    },
    RegExpLiteral {
        pattern: String,
        flags: String,
        loc: SourceLocation,
    },
    MetaProperty {
        meta: String,
        property: String,
        loc: SourceLocation,
    },

    // Expressions
    BinaryExpression {
        operator: BinaryOperator,
        left: Place,
        right: Place,
        loc: SourceLocation,
    },
    UnaryExpression {
        operator: UnaryOperator,
        value: Place,
        loc: SourceLocation,
    },
    CallExpression {
        callee: Place,
        args: Vec<Argument>,
        optional: bool,
        loc: SourceLocation,
    },
    MethodCall {
        receiver: Place,
        property: Place,
        args: Vec<Argument>,
        receiver_optional: bool,
        call_optional: bool,
        loc: SourceLocation,
    },
    NewExpression {
        callee: Place,
        args: Vec<Argument>,
        loc: SourceLocation,
    },
    TypeCastExpression {
        value: Place,
        type_: Type,
        type_annotation: String,
        type_annotation_kind: TypeAnnotationKind,
        loc: SourceLocation,
    },

    // Object/Array
    ObjectExpression {
        properties: Vec<ObjectPropertyOrSpread>,
        loc: SourceLocation,
    },
    ArrayExpression {
        elements: Vec<ArrayElement>,
        loc: SourceLocation,
    },
    ObjectMethod {
        lowered_func: LoweredFunction,
        loc: SourceLocation,
    },

    // JSX
    JsxExpression {
        tag: JsxTag,
        props: Vec<JsxAttribute>,
        children: Option<Vec<Place>>,
        loc: SourceLocation,
        opening_loc: SourceLocation,
        closing_loc: SourceLocation,
    },
    JsxFragment {
        children: Vec<Place>,
        loc: SourceLocation,
    },

    // Property access
    PropertyLoad {
        object: Place,
        property: PropertyLiteral,
        optional: bool,
        loc: SourceLocation,
    },
    PropertyStore {
        object: Place,
        property: PropertyLiteral,
        value: Place,
        loc: SourceLocation,
    },
    PropertyDelete {
        object: Place,
        property: PropertyLiteral,
        loc: SourceLocation,
    },
    ComputedLoad {
        object: Place,
        property: Place,
        optional: bool,
        loc: SourceLocation,
    },
    ComputedStore {
        object: Place,
        property: Place,
        value: Place,
        loc: SourceLocation,
    },
    ComputedDelete {
        object: Place,
        property: Place,
        loc: SourceLocation,
    },

    // Globals
    LoadGlobal {
        binding: NonLocalBinding,
        loc: SourceLocation,
    },
    StoreGlobal {
        name: String,
        value: Place,
        loc: SourceLocation,
    },

    // Functions
    FunctionExpression {
        name: Option<String>,
        lowered_func: LoweredFunction,
        expr_type: FunctionExpressionType,
        loc: SourceLocation,
    },

    // Templates
    TaggedTemplateExpression {
        tag: Place,
        raw: String,
        cooked: Option<String>,
        loc: SourceLocation,
    },
    TemplateLiteral {
        subexprs: Vec<Place>,
        quasis: Vec<TemplateQuasi>,
        loc: SourceLocation,
    },

    // Async/Iterator
    Await {
        value: Place,
        loc: SourceLocation,
    },
    GetIterator {
        collection: Place,
        loc: SourceLocation,
    },
    IteratorNext {
        iterator: Place,
        collection: Place,
        loc: SourceLocation,
    },
    NextPropertyOf {
        value: Place,
        loc: SourceLocation,
    },

    // Update
    PrefixUpdate {
        lvalue: Place,
        operation: UpdateOperator,
        value: Place,
        loc: SourceLocation,
    },
    PostfixUpdate {
        lvalue: Place,
        operation: UpdateOperator,
        value: Place,
        loc: SourceLocation,
    },

    // Memoization markers
    StartMemoize {
        manual_memo_id: u32,
        deps: Option<Vec<ManualMemoDependency>>,
        loc: SourceLocation,
    },
    FinishMemoize {
        manual_memo_id: u32,
        decl: Place,
        pruned: bool,
        loc: SourceLocation,
    },

    // Ternary (simplified: not lowered to control flow)
    Ternary {
        test: Place,
        consequent: Place,
        alternate: Place,
        loc: SourceLocation,
    },

    // Logical (simplified: not lowered to control flow)
    LogicalExpression {
        operator: LogicalOperator,
        left: Place,
        right: Place,
        loc: SourceLocation,
    },
    /// Reactive-only value used by BuildReactiveFunction to preserve multi-step
    /// value blocks as expression trees instead of flattening them into
    /// top-level instructions.
    ReactiveSequenceExpression {
        instructions: Vec<ReactiveInstruction>,
        id: InstructionId,
        value: Box<InstructionValue>,
        loc: SourceLocation,
    },
    /// Reactive-only value used by BuildReactiveFunction to preserve optional
    /// chaining structure through codegen.
    ReactiveOptionalExpression {
        optional: bool,
        value: Box<InstructionValue>,
        loc: SourceLocation,
    },
    /// Reactive-only logical expression whose operands may themselves be
    /// nested reactive values.
    ReactiveLogicalExpression {
        operator: LogicalOperator,
        left: Box<InstructionValue>,
        right: Box<InstructionValue>,
        loc: SourceLocation,
    },
    /// Reactive-only conditional expression whose branches may themselves be
    /// nested reactive values.
    ReactiveConditionalExpression {
        test: Box<InstructionValue>,
        consequent: Box<InstructionValue>,
        alternate: Box<InstructionValue>,
        loc: SourceLocation,
    },

    // Other
    Debugger {
        loc: SourceLocation,
    },
}

impl InstructionValue {
    pub fn loc(&self) -> &SourceLocation {
        match self {
            Self::LoadLocal { loc, .. }
            | Self::LoadContext { loc, .. }
            | Self::DeclareLocal { loc, .. }
            | Self::DeclareContext { loc, .. }
            | Self::StoreLocal { loc, .. }
            | Self::StoreContext { loc, .. }
            | Self::Destructure { loc, .. }
            | Self::Primitive { loc, .. }
            | Self::JSXText { loc, .. }
            | Self::RegExpLiteral { loc, .. }
            | Self::MetaProperty { loc, .. }
            | Self::BinaryExpression { loc, .. }
            | Self::UnaryExpression { loc, .. }
            | Self::CallExpression { loc, .. }
            | Self::MethodCall { loc, .. }
            | Self::NewExpression { loc, .. }
            | Self::TypeCastExpression { loc, .. }
            | Self::ObjectExpression { loc, .. }
            | Self::ArrayExpression { loc, .. }
            | Self::ObjectMethod { loc, .. }
            | Self::JsxExpression { loc, .. }
            | Self::JsxFragment { loc, .. }
            | Self::PropertyLoad { loc, .. }
            | Self::PropertyStore { loc, .. }
            | Self::PropertyDelete { loc, .. }
            | Self::ComputedLoad { loc, .. }
            | Self::ComputedStore { loc, .. }
            | Self::ComputedDelete { loc, .. }
            | Self::LoadGlobal { loc, .. }
            | Self::StoreGlobal { loc, .. }
            | Self::FunctionExpression { loc, .. }
            | Self::TaggedTemplateExpression { loc, .. }
            | Self::TemplateLiteral { loc, .. }
            | Self::Await { loc, .. }
            | Self::GetIterator { loc, .. }
            | Self::IteratorNext { loc, .. }
            | Self::NextPropertyOf { loc, .. }
            | Self::PrefixUpdate { loc, .. }
            | Self::PostfixUpdate { loc, .. }
            | Self::StartMemoize { loc, .. }
            | Self::FinishMemoize { loc, .. }
            | Self::Ternary { loc, .. }
            | Self::LogicalExpression { loc, .. }
            | Self::ReactiveSequenceExpression { loc, .. }
            | Self::ReactiveOptionalExpression { loc, .. }
            | Self::ReactiveLogicalExpression { loc, .. }
            | Self::ReactiveConditionalExpression { loc, .. }
            | Self::Debugger { loc, .. } => loc,
        }
    }
}

// ---------------------------------------------------------------------------
// Supporting types for instructions
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum PrimitiveValue {
    Null,
    Undefined,
    Boolean(bool),
    Number(f64),
    String(String),
}

#[derive(Debug, Clone)]
pub struct LValue {
    pub place: Place,
    pub kind: InstructionKind,
}

#[derive(Debug, Clone)]
pub struct LValuePattern {
    pub pattern: Pattern,
    pub kind: InstructionKind,
}

#[derive(Debug, Clone)]
pub enum Pattern {
    Array(ArrayPattern),
    Object(ObjectPattern),
}

#[derive(Debug, Clone)]
pub struct ArrayPattern {
    pub items: Vec<ArrayElement>,
}

#[derive(Debug, Clone)]
pub struct ObjectPattern {
    pub properties: Vec<ObjectPropertyOrSpread>,
}

#[derive(Debug, Clone)]
pub enum ArrayElement {
    Place(Place),
    Spread(Place),
    Hole,
}

/// Property literal (string or number key).
#[derive(Debug, Clone, PartialEq)]
pub enum PropertyLiteral {
    String(String),
    Number(f64),
}

#[derive(Debug, Clone)]
pub enum ObjectPropertyKey {
    String(String),
    Identifier(String),
    Computed(Place),
    Number(f64),
}

#[derive(Debug, Clone)]
pub struct ObjectProperty {
    pub key: ObjectPropertyKey,
    pub type_: ObjectPropertyType,
    pub place: Place,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectPropertyType {
    Property,
    Method,
}

#[derive(Debug, Clone)]
pub enum ObjectPropertyOrSpread {
    Property(ObjectProperty),
    Spread(Place),
}

/// Function call argument.
#[derive(Debug, Clone)]
pub enum Argument {
    Place(Place),
    Spread(Place),
}

/// JSX element tag.
#[derive(Debug, Clone)]
pub enum JsxTag {
    /// A component or intrinsic element name loaded into a Place.
    Component(Place),
    /// A built-in HTML tag like "div", "span", etc.
    BuiltinTag(String),
    /// A fragment `<>...</>`.
    Fragment,
}

/// A JSX attribute.
#[derive(Debug, Clone)]
pub enum JsxAttribute {
    Attribute { name: String, place: Place },
    SpreadAttribute { argument: Place },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FunctionExpressionType {
    ArrowFunctionExpression,
    FunctionExpression,
    FunctionDeclaration,
}

#[derive(Debug, Clone)]
pub struct TemplateQuasi {
    pub raw: String,
    pub cooked: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOperator {
    Eq,          // ==
    NotEq,       // !=
    StrictEq,    // ===
    StrictNotEq, // !==
    Lt,          // <
    LtEq,        // <=
    Gt,          // >
    GtEq,        // >=
    LShift,      // <<
    RShift,      // >>
    URShift,     // >>>
    Add,         // +
    Sub,         // -
    Mul,         // *
    Div,         // /
    Mod,         // %
    Exp,         // **
    BitOr,       // |
    BitXor,      // ^
    BitAnd,      // &
    In,          // in
    InstanceOf,  // instanceof
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOperator {
    Minus,  // -
    Plus,   // +
    Not,    // !
    BitNot, // ~
    TypeOf, // typeof
    Void,   // void
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateOperator {
    Increment, // ++
    Decrement, // --
}

// ---------------------------------------------------------------------------
// Non-local bindings
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum NonLocalBinding {
    ImportDefault {
        name: String,
        module: String,
    },
    ImportNamespace {
        name: String,
        module: String,
    },
    ImportSpecifier {
        name: String,
        module: String,
        imported: String,
    },
    ModuleLocal {
        name: String,
    },
    Global {
        name: String,
    },
}

impl NonLocalBinding {
    pub fn name(&self) -> &str {
        match self {
            Self::ImportDefault { name, .. }
            | Self::ImportNamespace { name, .. }
            | Self::ImportSpecifier { name, .. }
            | Self::ModuleLocal { name }
            | Self::Global { name } => name,
        }
    }
}

// ---------------------------------------------------------------------------
// Manual memoization
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ManualMemoDependency {
    pub root: ManualMemoRoot,
    pub path: Vec<DependencyPathEntry>,
}

#[derive(Debug, Clone)]
pub enum ManualMemoRoot {
    NamedLocal(Place),
    Global { identifier_name: String },
}

#[derive(Debug, Clone)]
pub struct DependencyPathEntry {
    pub property: String,
    pub optional: bool,
}

// ---------------------------------------------------------------------------
// Lowered function (for nested functions)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct LoweredFunction {
    pub func: HIRFunction,
}

// ---------------------------------------------------------------------------
// Terminals
// ---------------------------------------------------------------------------

/// Block terminal — how control exits a basic block.
#[derive(Debug, Clone)]
pub enum Terminal {
    Unsupported {
        id: InstructionId,
        loc: SourceLocation,
    },
    Unreachable {
        id: InstructionId,
        loc: SourceLocation,
    },
    Throw {
        value: Place,
        id: InstructionId,
        loc: SourceLocation,
    },
    Return {
        value: Place,
        return_variant: ReturnVariant,
        id: InstructionId,
        loc: SourceLocation,
    },
    Goto {
        block: BlockId,
        variant: GotoVariant,
        id: InstructionId,
        loc: SourceLocation,
    },
    If {
        test: Place,
        consequent: BlockId,
        alternate: BlockId,
        fallthrough: BlockId,
        id: InstructionId,
        loc: SourceLocation,
    },
    Branch {
        test: Place,
        consequent: BlockId,
        alternate: BlockId,
        fallthrough: BlockId,
        id: InstructionId,
        loc: SourceLocation,
    },
    Switch {
        test: Place,
        cases: Vec<SwitchCase>,
        fallthrough: BlockId,
        id: InstructionId,
        loc: SourceLocation,
    },
    For {
        init: BlockId,
        test: BlockId,
        update: Option<BlockId>,
        loop_block: BlockId,
        fallthrough: BlockId,
        id: InstructionId,
        loc: SourceLocation,
    },
    ForOf {
        init: BlockId,
        test: BlockId,
        loop_block: BlockId,
        fallthrough: BlockId,
        id: InstructionId,
        loc: SourceLocation,
    },
    ForIn {
        init: BlockId,
        loop_block: BlockId,
        fallthrough: BlockId,
        id: InstructionId,
        loc: SourceLocation,
    },
    DoWhile {
        loop_block: BlockId,
        test: BlockId,
        fallthrough: BlockId,
        id: InstructionId,
        loc: SourceLocation,
    },
    While {
        test: BlockId,
        loop_block: BlockId,
        fallthrough: BlockId,
        id: InstructionId,
        loc: SourceLocation,
    },
    Logical {
        operator: LogicalOperator,
        test: BlockId,
        fallthrough: BlockId,
        id: InstructionId,
        loc: SourceLocation,
    },
    Ternary {
        test: BlockId,
        fallthrough: BlockId,
        id: InstructionId,
        loc: SourceLocation,
    },
    Optional {
        optional: bool,
        test: BlockId,
        fallthrough: BlockId,
        id: InstructionId,
        loc: SourceLocation,
    },
    Label {
        block: BlockId,
        fallthrough: BlockId,
        id: InstructionId,
        loc: SourceLocation,
    },
    Sequence {
        block: BlockId,
        fallthrough: BlockId,
        id: InstructionId,
        loc: SourceLocation,
    },
    Try {
        block: BlockId,
        handler_binding: Option<Place>,
        handler: BlockId,
        fallthrough: BlockId,
        id: InstructionId,
        loc: SourceLocation,
    },
    MaybeThrow {
        continuation: BlockId,
        handler: BlockId,
        id: InstructionId,
        loc: SourceLocation,
    },
    Scope {
        block: BlockId,
        fallthrough: BlockId,
        scope: ReactiveScope,
        id: InstructionId,
        loc: SourceLocation,
    },
    PrunedScope {
        block: BlockId,
        fallthrough: BlockId,
        scope: ReactiveScope,
        id: InstructionId,
        loc: SourceLocation,
    },
}

impl Terminal {
    pub fn id(&self) -> InstructionId {
        match self {
            Self::Unsupported { id, .. }
            | Self::Unreachable { id, .. }
            | Self::Throw { id, .. }
            | Self::Return { id, .. }
            | Self::Goto { id, .. }
            | Self::If { id, .. }
            | Self::Branch { id, .. }
            | Self::Switch { id, .. }
            | Self::For { id, .. }
            | Self::ForOf { id, .. }
            | Self::ForIn { id, .. }
            | Self::DoWhile { id, .. }
            | Self::While { id, .. }
            | Self::Logical { id, .. }
            | Self::Ternary { id, .. }
            | Self::Optional { id, .. }
            | Self::Label { id, .. }
            | Self::Sequence { id, .. }
            | Self::Try { id, .. }
            | Self::MaybeThrow { id, .. }
            | Self::Scope { id, .. }
            | Self::PrunedScope { id, .. } => *id,
        }
    }

    /// Returns the fallthrough block if this terminal has one.
    pub fn fallthrough(&self) -> Option<BlockId> {
        match self {
            Self::If { fallthrough, .. }
            | Self::Branch { fallthrough, .. }
            | Self::Switch { fallthrough, .. }
            | Self::For { fallthrough, .. }
            | Self::ForOf { fallthrough, .. }
            | Self::ForIn { fallthrough, .. }
            | Self::DoWhile { fallthrough, .. }
            | Self::While { fallthrough, .. }
            | Self::Logical { fallthrough, .. }
            | Self::Ternary { fallthrough, .. }
            | Self::Optional { fallthrough, .. }
            | Self::Label { fallthrough, .. }
            | Self::Sequence { fallthrough, .. }
            | Self::Try { fallthrough, .. }
            | Self::Scope { fallthrough, .. }
            | Self::PrunedScope { fallthrough, .. } => Some(*fallthrough),

            Self::Unsupported { .. }
            | Self::Unreachable { .. }
            | Self::Throw { .. }
            | Self::Return { .. }
            | Self::Goto { .. }
            | Self::MaybeThrow { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReturnVariant {
    Void,
    Implicit,
    Explicit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GotoVariant {
    Break,
    Continue,
    Try,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogicalOperator {
    And,               // &&
    Or,                // ||
    NullishCoalescing, // ??
}

#[derive(Debug, Clone)]
pub struct SwitchCase {
    pub test: Option<Place>,
    pub block: BlockId,
}

// ---------------------------------------------------------------------------
// Basic block
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    Block,
    Value,
    Loop,
    Sequence,
    Catch,
}

/// A phi node merges values from different predecessor blocks.
#[derive(Debug, Clone)]
pub struct Phi {
    pub place: Place,
    pub operands: HashMap<BlockId, Place>,
}

/// A basic block containing instructions and a terminal.
#[derive(Debug, Clone)]
pub struct BasicBlock {
    pub kind: BlockKind,
    pub id: BlockId,
    pub instructions: Vec<Instruction>,
    pub terminal: Terminal,
    pub preds: HashSet<BlockId>,
    pub phis: Vec<Phi>,
}

impl BasicBlock {
    /// Get the InstructionId assigned to this block's terminal.
    pub fn terminal_id(&self) -> InstructionId {
        get_terminal_id(&self.terminal)
    }
}

/// Extract the InstructionId from a terminal.
pub fn get_terminal_id(terminal: &Terminal) -> InstructionId {
    match terminal {
        Terminal::Return { id, .. }
        | Terminal::Throw { id, .. }
        | Terminal::If { id, .. }
        | Terminal::Branch { id, .. }
        | Terminal::Goto { id, .. }
        | Terminal::Switch { id, .. }
        | Terminal::Try { id, .. }
        | Terminal::Unsupported { id, .. }
        | Terminal::Unreachable { id, .. }
        | Terminal::For { id, .. }
        | Terminal::ForOf { id, .. }
        | Terminal::ForIn { id, .. }
        | Terminal::While { id, .. }
        | Terminal::DoWhile { id, .. }
        | Terminal::Label { id, .. }
        | Terminal::Scope { id, .. }
        | Terminal::PrunedScope { id, .. }
        | Terminal::Sequence { id, .. }
        | Terminal::Logical { id, .. }
        | Terminal::Ternary { id, .. }
        | Terminal::Optional { id, .. }
        | Terminal::MaybeThrow { id, .. } => *id,
    }
}

// ---------------------------------------------------------------------------
// HIR (control-flow graph)
// ---------------------------------------------------------------------------

/// The HIR control-flow graph: an entry block and a map of blocks.
#[derive(Debug, Clone)]
pub struct HIR {
    pub entry: BlockId,
    /// Blocks stored in reverse postorder (predecessors before successors).
    pub blocks: Vec<(BlockId, BasicBlock)>,
}

// ---------------------------------------------------------------------------
// HIR Function
// ---------------------------------------------------------------------------

/// What kind of React function this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReactFunctionType {
    Component,
    Hook,
    Other,
}

/// A function lowered to HIR form.
#[derive(Debug, Clone)]
pub struct HIRFunction {
    pub env: crate::environment::Environment,
    pub loc: SourceLocation,
    pub id: Option<String>,
    pub fn_type: ReactFunctionType,
    pub params: Vec<Argument>,
    pub returns: Place,
    pub context: Vec<Place>,
    pub body: HIR,
    pub generator: bool,
    pub async_: bool,
    pub directives: Vec<String>,
    /// Externally-visible aliasing effects of this function, populated by
    /// `analyse_functions` for inner function expressions / object methods.
    /// `None` until `analyse_functions` runs on the enclosing function.
    pub aliasing_effects: Option<Vec<crate::inference::aliasing_effects::AliasingEffect>>,
}

// ---------------------------------------------------------------------------
// Reactive scope
// ---------------------------------------------------------------------------

/// A reactive scope represents a set of instructions that should be memoized together.
#[derive(Debug, Clone)]
pub struct ReactiveScope {
    pub id: ScopeId,
    pub range: MutableRange,
    pub dependencies: Vec<ReactiveScopeDependency>,
    pub declarations: IndexMap<IdentifierId, ScopeDeclaration>,
    pub reassignments: Vec<Identifier>,
    pub merged_id: Option<ScopeId>,
    /// For scopes that contain early returns, the identifier + label used
    /// to communicate the early return value.
    pub early_return_value: Option<EarlyReturnValue>,
}

#[derive(Debug, Clone)]
pub struct EarlyReturnValue {
    pub value: Identifier,
    pub loc: SourceLocation,
    pub label: BlockId,
}

#[derive(Debug, Clone)]
pub struct ScopeDeclaration {
    pub identifier: Identifier,
    /// The scope in which the variable was originally declared.
    /// Used by prune_unused_scopes to distinguish own declarations from propagated ones.
    pub scope: ReactiveScope,
}

#[derive(Debug, Clone)]
pub struct ReactiveScopeDependency {
    pub identifier: Identifier,
    pub path: Vec<DependencyPathEntry>,
}

// ---------------------------------------------------------------------------
// Reactive Function (tree-shaped IR for codegen)
// ---------------------------------------------------------------------------

/// A reactive function is the tree-shaped output of the reactive scope analysis.
/// It's the input to the codegen phase.
#[derive(Debug)]
pub struct ReactiveFunction {
    pub loc: SourceLocation,
    pub id: Option<String>,
    pub name_hint: Option<String>,
    pub params: Vec<Argument>,
    pub generator: bool,
    pub async_: bool,
    pub body: ReactiveBlock,
    pub directives: Vec<String>,
}

pub type ReactiveBlock = Vec<ReactiveStatement>;

#[derive(Debug)]
pub enum ReactiveStatement {
    Instruction(Box<ReactiveInstruction>),
    Terminal(ReactiveTerminalStatement),
    Scope(ReactiveScopeBlock),
    PrunedScope(PrunedReactiveScopeBlock),
}

#[derive(Debug, Clone)]
pub struct ReactiveInstruction {
    pub id: InstructionId,
    pub lvalue: Option<Place>,
    pub value: InstructionValue,
    pub loc: SourceLocation,
}

#[derive(Debug)]
pub struct ReactiveTerminalStatement {
    pub terminal: ReactiveTerminal,
    pub label: Option<ReactiveLabel>,
}

#[derive(Debug)]
pub struct ReactiveLabel {
    pub id: BlockId,
    pub implicit: bool,
}

#[derive(Debug)]
pub struct ReactiveScopeBlock {
    pub scope: ReactiveScope,
    pub instructions: ReactiveBlock,
}

#[derive(Debug)]
pub struct PrunedReactiveScopeBlock {
    pub scope: ReactiveScope,
    pub instructions: ReactiveBlock,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReactiveTerminalTargetKind {
    /// Control will implicitly transfer to this block (no break/continue emitted).
    Implicit,
    /// An unlabeled break/continue is sufficient.
    Unlabeled,
    /// A labeled break/continue is required.
    Labeled,
}

#[derive(Debug)]
pub enum ReactiveTerminal {
    Break {
        target: BlockId,
        target_kind: ReactiveTerminalTargetKind,
        id: InstructionId,
        loc: SourceLocation,
    },
    Continue {
        target: BlockId,
        target_kind: ReactiveTerminalTargetKind,
        id: InstructionId,
        loc: SourceLocation,
    },
    Return {
        value: Place,
        id: InstructionId,
        loc: SourceLocation,
    },
    Throw {
        value: Place,
        id: InstructionId,
        loc: SourceLocation,
    },
    Switch {
        test: Place,
        cases: Vec<ReactiveSwitchCase>,
        id: InstructionId,
        loc: SourceLocation,
    },
    DoWhile {
        loop_block: ReactiveBlock,
        test: Place,
        id: InstructionId,
        loc: SourceLocation,
    },
    While {
        test: Place,
        loop_block: ReactiveBlock,
        id: InstructionId,
        loc: SourceLocation,
    },
    For {
        init: ReactiveBlock,
        test: Place,
        update: Option<ReactiveBlock>,
        loop_block: ReactiveBlock,
        id: InstructionId,
        loc: SourceLocation,
    },
    ForOf {
        init: ReactiveBlock,
        test: Place,
        loop_block: ReactiveBlock,
        id: InstructionId,
        loc: SourceLocation,
    },
    ForIn {
        init: ReactiveBlock,
        loop_block: ReactiveBlock,
        id: InstructionId,
        loc: SourceLocation,
    },
    If {
        test: Place,
        consequent: ReactiveBlock,
        alternate: Option<ReactiveBlock>,
        id: InstructionId,
        loc: SourceLocation,
    },
    Label {
        block: ReactiveBlock,
        id: InstructionId,
        loc: SourceLocation,
    },
    Try {
        block: ReactiveBlock,
        handler_binding: Option<Place>,
        handler: ReactiveBlock,
        id: InstructionId,
        loc: SourceLocation,
    },
}

#[derive(Debug)]
pub struct ReactiveSwitchCase {
    pub test: Option<Place>,
    pub block: Option<ReactiveBlock>,
}

// ---------------------------------------------------------------------------
// Helper constructors
// ---------------------------------------------------------------------------

static NEXT_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

pub fn make_instruction_id(id: u32) -> InstructionId {
    InstructionId(id)
}

pub fn make_block_id(id: u32) -> BlockId {
    BlockId(id)
}

pub fn make_identifier_id(id: u32) -> IdentifierId {
    IdentifierId(id)
}

pub fn make_declaration_id(id: u32) -> DeclarationId {
    DeclarationId(id)
}

pub fn make_scope_id(id: u32) -> ScopeId {
    ScopeId(id)
}

/// Create a minimal ReactiveScope with only the given ScopeId.
/// Used for ScopeDeclaration.scope where only the id matters (for has_own_declaration check).
pub fn make_declaration_scope(id: ScopeId) -> ReactiveScope {
    ReactiveScope {
        id,
        range: MutableRange::default(),
        dependencies: Vec::new(),
        declarations: IndexMap::new(),
        reassignments: Vec::new(),
        merged_id: None,
        early_return_value: None,
    }
}

pub fn make_type() -> Type {
    let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Type::TypeVar { id: TypeId(id) }
}

pub fn make_temporary_identifier(id: IdentifierId, loc: SourceLocation) -> Identifier {
    Identifier {
        id,
        declaration_id: DeclarationId(id.0),
        name: None,
        mutable_range: MutableRange::default(),
        scope: None,
        type_: make_type(),
        loc,
    }
}
