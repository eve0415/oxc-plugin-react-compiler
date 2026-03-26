//! HIR visitors — traversal helpers for instruction operands and lvalues.
//!
//! Port of `visitors.ts` from upstream React Compiler.
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.

use super::types::*;

/// Apply `f` to each operand (read) Place of an instruction value.
pub fn for_each_instruction_operand(instr: &Instruction, mut f: impl FnMut(&Place)) {
    for_each_value_operand(&instr.value, &mut f);
}

/// Apply `f` to each operand (read) Place of an instruction value.
pub fn for_each_instruction_value_operand(value: &InstructionValue, mut f: impl FnMut(&Place)) {
    for_each_value_operand(value, &mut f);
}

/// Apply `f` to each operand (read) Place of an instruction value, mutably.
pub fn map_instruction_operands(instr: &mut Instruction, mut f: impl FnMut(&mut Place)) {
    map_value_operands(&mut instr.value, &mut f);
}

/// Apply `f` to each lvalue (write) Place of an instruction (read-only).
pub fn for_each_instruction_lvalue(instr: &Instruction, mut f: impl FnMut(&Place)) {
    f(&instr.lvalue);
    match &instr.value {
        InstructionValue::StoreLocal { lvalue, .. }
        | InstructionValue::StoreContext { lvalue, .. }
        | InstructionValue::DeclareLocal { lvalue, .. }
        | InstructionValue::DeclareContext { lvalue, .. } => {
            f(&lvalue.place);
        }
        InstructionValue::Destructure { lvalue, .. } => {
            for_each_pattern_place(&lvalue.pattern, &mut f);
        }
        InstructionValue::PrefixUpdate { lvalue, .. }
        | InstructionValue::PostfixUpdate { lvalue, .. } => {
            f(lvalue);
        }
        _ => {}
    }
}

/// Apply `f` to each operand Place in a terminal (read-only).
pub fn for_each_terminal_operand(terminal: &Terminal, mut f: impl FnMut(&Place)) {
    match terminal {
        Terminal::Throw { value, .. } | Terminal::Return { value, .. } => {
            f(value);
        }
        Terminal::If { test, .. } | Terminal::Branch { test, .. } => {
            f(test);
        }
        Terminal::Switch { test, cases, .. } => {
            f(test);
            for case in cases {
                if let Some(t) = &case.test {
                    f(t);
                }
            }
        }
        Terminal::Try {
            handler_binding: Some(binding),
            ..
        } => f(binding),
        _ => {}
    }
}

/// Apply `f` to each lvalue (write) Place of an instruction.
pub fn map_instruction_lvalues(instr: &mut Instruction, mut f: impl FnMut(&mut Place)) {
    f(&mut instr.lvalue);
    match &mut instr.value {
        // Upstream `mapInstructionLValues()` only remaps write targets for local
        // declarations/stores. Context store targets are remapped as operands.
        InstructionValue::StoreLocal { lvalue, .. }
        | InstructionValue::DeclareLocal { lvalue, .. } => {
            f(&mut lvalue.place);
        }
        InstructionValue::Destructure { lvalue, .. } => {
            map_pattern_places(&mut lvalue.pattern, &mut f);
        }
        InstructionValue::PrefixUpdate { lvalue, .. }
        | InstructionValue::PostfixUpdate { lvalue, .. } => {
            f(lvalue);
        }
        _ => {}
    }
}

/// Apply `f` to each operand Place in a terminal.
pub fn map_terminal_operands(terminal: &mut Terminal, mut f: impl FnMut(&mut Place)) {
    match terminal {
        Terminal::Throw { value, .. } | Terminal::Return { value, .. } => {
            f(value);
        }
        Terminal::If { test, .. } | Terminal::Branch { test, .. } => {
            f(test);
        }
        Terminal::Switch { test, cases, .. } => {
            f(test);
            for case in cases {
                if let Some(t) = &mut case.test {
                    f(t);
                }
            }
        }
        Terminal::Try {
            handler_binding, ..
        } => {
            if let Some(binding) = handler_binding {
                f(binding);
            }
        }
        // Terminals with no operands
        Terminal::Unsupported { .. }
        | Terminal::Unreachable { .. }
        | Terminal::Goto { .. }
        | Terminal::For { .. }
        | Terminal::ForOf { .. }
        | Terminal::ForIn { .. }
        | Terminal::DoWhile { .. }
        | Terminal::While { .. }
        | Terminal::Logical { .. }
        | Terminal::Ternary { .. }
        | Terminal::Optional { .. }
        | Terminal::Label { .. }
        | Terminal::Sequence { .. }
        | Terminal::Scope { .. }
        | Terminal::PrunedScope { .. } => {}
    }
}

pub(crate) fn for_each_value_operand(value: &InstructionValue, f: &mut impl FnMut(&Place)) {
    match value {
        InstructionValue::LoadLocal { place, .. } | InstructionValue::LoadContext { place, .. } => {
            f(place);
        }
        InstructionValue::StoreLocal { value, .. } => {
            f(value);
        }
        InstructionValue::StoreContext { lvalue, value, .. } => {
            // Upstream yields both lvalue.place and value for StoreContext
            f(&lvalue.place);
            f(value);
        }
        InstructionValue::Destructure { value, .. } => {
            f(value);
        }
        InstructionValue::BinaryExpression { left, right, .. } => {
            f(left);
            f(right);
        }
        InstructionValue::UnaryExpression { value, .. } => {
            f(value);
        }
        InstructionValue::CallExpression { callee, args, .. } => {
            f(callee);
            for arg in args {
                match arg {
                    Argument::Place(p) | Argument::Spread(p) => f(p),
                }
            }
        }
        InstructionValue::MethodCall {
            receiver,
            property,
            args,
            ..
        } => {
            f(receiver);
            f(property);
            for arg in args {
                match arg {
                    Argument::Place(p) | Argument::Spread(p) => f(p),
                }
            }
        }
        InstructionValue::NewExpression { callee, args, .. } => {
            f(callee);
            for arg in args {
                match arg {
                    Argument::Place(p) | Argument::Spread(p) => f(p),
                }
            }
        }
        InstructionValue::ObjectExpression { properties, .. } => {
            for prop in properties {
                match prop {
                    ObjectPropertyOrSpread::Property(p) => {
                        f(&p.place);
                        if let ObjectPropertyKey::Computed(place) = &p.key {
                            f(place);
                        }
                    }
                    ObjectPropertyOrSpread::Spread(p) => f(p),
                }
            }
        }
        InstructionValue::ArrayExpression { elements, .. } => {
            for elem in elements {
                match elem {
                    ArrayElement::Place(p) | ArrayElement::Spread(p) => f(p),
                    ArrayElement::Hole => {}
                }
            }
        }
        InstructionValue::JsxExpression {
            tag,
            props,
            children,
            ..
        } => {
            if let JsxTag::Component(p) = tag {
                f(p);
            }
            for attr in props {
                match attr {
                    JsxAttribute::Attribute { place, .. } => f(place),
                    JsxAttribute::SpreadAttribute { argument } => f(argument),
                }
            }
            if let Some(children) = children {
                for child in children {
                    f(child);
                }
            }
        }
        InstructionValue::JsxFragment { children, .. } => {
            for child in children {
                f(child);
            }
        }
        InstructionValue::PropertyLoad { object, .. } => f(object),
        InstructionValue::PropertyStore { object, value, .. } => {
            f(object);
            f(value);
        }
        InstructionValue::PropertyDelete { object, .. } => f(object),
        InstructionValue::ComputedLoad {
            object, property, ..
        } => {
            f(object);
            f(property);
        }
        InstructionValue::ComputedStore {
            object,
            property,
            value,
            ..
        } => {
            f(object);
            f(property);
            f(value);
        }
        InstructionValue::ComputedDelete {
            object, property, ..
        } => {
            f(object);
            f(property);
        }
        InstructionValue::StoreGlobal { value, .. } => f(value),
        InstructionValue::TypeCastExpression { value, .. } => f(value),
        InstructionValue::TaggedTemplateExpression { tag, .. } => f(tag),
        InstructionValue::TemplateLiteral { subexprs, .. } => {
            for s in subexprs {
                f(s);
            }
        }
        InstructionValue::Await { value, .. } => f(value),
        InstructionValue::GetIterator { collection, .. } => f(collection),
        InstructionValue::IteratorNext {
            iterator,
            collection,
            ..
        } => {
            f(iterator);
            f(collection);
        }
        InstructionValue::NextPropertyOf { value, .. } => f(value),
        InstructionValue::PrefixUpdate { value, .. }
        | InstructionValue::PostfixUpdate { value, .. } => {
            f(value);
        }
        InstructionValue::FinishMemoize { decl, .. } => f(decl),
        InstructionValue::Ternary {
            test,
            consequent,
            alternate,
            ..
        } => {
            f(test);
            f(consequent);
            f(alternate);
        }
        InstructionValue::LogicalExpression { left, right, .. } => {
            f(left);
            f(right);
        }
        InstructionValue::ReactiveSequenceExpression {
            instructions,
            value,
            ..
        } => {
            for instr in instructions {
                if let Some(lvalue) = &instr.lvalue {
                    f(lvalue);
                }
                for_each_value_operand(&instr.value, f);
            }
            for_each_value_operand(value, f);
        }
        InstructionValue::ReactiveOptionalExpression { value, .. } => {
            for_each_value_operand(value, f);
        }
        InstructionValue::ReactiveLogicalExpression { left, right, .. } => {
            for_each_value_operand(left, f);
            for_each_value_operand(right, f);
        }
        // FunctionExpression/ObjectMethod: context captures are operands
        InstructionValue::FunctionExpression { lowered_func, .. }
        | InstructionValue::ObjectMethod { lowered_func, .. } => {
            for place in &lowered_func.func.context {
                f(place);
            }
        }
        // StartMemoize: yield NamedLocal deps as operands to keep them alive through DCE
        InstructionValue::StartMemoize { deps, .. } => {
            if let Some(deps) = deps {
                for dep in deps {
                    if let ManualMemoRoot::NamedLocal(place) = &dep.root {
                        f(place);
                    }
                }
            }
        }
        // No operands
        InstructionValue::Primitive { .. }
        | InstructionValue::JSXText { .. }
        | InstructionValue::RegExpLiteral { .. }
        | InstructionValue::LoadGlobal { .. }
        | InstructionValue::DeclareLocal { .. }
        | InstructionValue::DeclareContext { .. }
        | InstructionValue::MetaProperty { .. }
        | InstructionValue::Debugger { .. } => {}
    }
}

fn map_value_operands(value: &mut InstructionValue, f: &mut impl FnMut(&mut Place)) {
    match value {
        InstructionValue::LoadLocal { place, .. } | InstructionValue::LoadContext { place, .. } => {
            f(place);
        }
        InstructionValue::StoreLocal { value, .. } => {
            f(value);
        }
        InstructionValue::StoreContext { lvalue, value, .. } => {
            // Upstream yields both lvalue.place and value for StoreContext
            f(&mut lvalue.place);
            f(value);
        }
        InstructionValue::Destructure { value, .. } => {
            f(value);
        }
        InstructionValue::BinaryExpression { left, right, .. } => {
            f(left);
            f(right);
        }
        InstructionValue::UnaryExpression { value, .. } => {
            f(value);
        }
        InstructionValue::CallExpression { callee, args, .. } => {
            f(callee);
            for arg in args {
                match arg {
                    Argument::Place(p) | Argument::Spread(p) => f(p),
                }
            }
        }
        InstructionValue::MethodCall {
            receiver,
            property,
            args,
            ..
        } => {
            f(receiver);
            f(property);
            for arg in args {
                match arg {
                    Argument::Place(p) | Argument::Spread(p) => f(p),
                }
            }
        }
        InstructionValue::NewExpression { callee, args, .. } => {
            f(callee);
            for arg in args {
                match arg {
                    Argument::Place(p) | Argument::Spread(p) => f(p),
                }
            }
        }
        InstructionValue::ObjectExpression { properties, .. } => {
            for prop in properties {
                match prop {
                    ObjectPropertyOrSpread::Property(p) => {
                        f(&mut p.place);
                        if let ObjectPropertyKey::Computed(place) = &mut p.key {
                            f(place);
                        }
                    }
                    ObjectPropertyOrSpread::Spread(p) => f(p),
                }
            }
        }
        InstructionValue::ArrayExpression { elements, .. } => {
            for elem in elements {
                match elem {
                    ArrayElement::Place(p) | ArrayElement::Spread(p) => f(p),
                    ArrayElement::Hole => {}
                }
            }
        }
        InstructionValue::JsxExpression {
            tag,
            props,
            children,
            ..
        } => {
            if let JsxTag::Component(p) = tag {
                f(p);
            }
            for attr in props {
                match attr {
                    JsxAttribute::Attribute { place, .. } => f(place),
                    JsxAttribute::SpreadAttribute { argument } => f(argument),
                }
            }
            if let Some(children) = children {
                for child in children {
                    f(child);
                }
            }
        }
        InstructionValue::JsxFragment { children, .. } => {
            for child in children {
                f(child);
            }
        }
        InstructionValue::PropertyLoad { object, .. } => f(object),
        InstructionValue::PropertyStore { object, value, .. } => {
            f(object);
            f(value);
        }
        InstructionValue::PropertyDelete { object, .. } => f(object),
        InstructionValue::ComputedLoad {
            object, property, ..
        } => {
            f(object);
            f(property);
        }
        InstructionValue::ComputedStore {
            object,
            property,
            value,
            ..
        } => {
            f(object);
            f(property);
            f(value);
        }
        InstructionValue::ComputedDelete {
            object, property, ..
        } => {
            f(object);
            f(property);
        }
        InstructionValue::StoreGlobal { value, .. } => f(value),
        InstructionValue::TypeCastExpression { value, .. } => f(value),
        InstructionValue::TaggedTemplateExpression { tag, .. } => f(tag),
        InstructionValue::TemplateLiteral { subexprs, .. } => {
            for s in subexprs {
                f(s);
            }
        }
        InstructionValue::Await { value, .. } => f(value),
        InstructionValue::GetIterator { collection, .. } => f(collection),
        InstructionValue::IteratorNext {
            iterator,
            collection,
            ..
        } => {
            f(iterator);
            f(collection);
        }
        InstructionValue::NextPropertyOf { value, .. } => f(value),
        InstructionValue::PrefixUpdate { value, .. }
        | InstructionValue::PostfixUpdate { value, .. } => {
            f(value);
        }
        InstructionValue::FinishMemoize { decl, .. } => f(decl),
        InstructionValue::Ternary {
            test,
            consequent,
            alternate,
            ..
        } => {
            f(test);
            f(consequent);
            f(alternate);
        }
        InstructionValue::LogicalExpression { left, right, .. } => {
            f(left);
            f(right);
        }
        InstructionValue::ReactiveSequenceExpression {
            instructions,
            value,
            ..
        } => {
            for instr in instructions {
                if let Some(lvalue) = &mut instr.lvalue {
                    f(lvalue);
                }
                map_value_operands(&mut instr.value, f);
            }
            map_value_operands(value, f);
        }
        InstructionValue::ReactiveOptionalExpression { value, .. } => {
            map_value_operands(value, f);
        }
        InstructionValue::ReactiveLogicalExpression { left, right, .. } => {
            map_value_operands(left, f);
            map_value_operands(right, f);
        }
        // FunctionExpression/ObjectMethod: context captures are operands
        InstructionValue::FunctionExpression { lowered_func, .. }
        | InstructionValue::ObjectMethod { lowered_func, .. } => {
            for place in &mut lowered_func.func.context {
                f(place);
            }
        }
        // StartMemoize: yield NamedLocal deps as operands to keep them alive through DCE
        InstructionValue::StartMemoize { deps, .. } => {
            if let Some(deps) = deps {
                for dep in deps {
                    if let ManualMemoRoot::NamedLocal(place) = &mut dep.root {
                        f(place);
                    }
                }
            }
        }
        // No operands
        InstructionValue::Primitive { .. }
        | InstructionValue::JSXText { .. }
        | InstructionValue::RegExpLiteral { .. }
        | InstructionValue::LoadGlobal { .. }
        | InstructionValue::DeclareLocal { .. }
        | InstructionValue::DeclareContext { .. }
        | InstructionValue::MetaProperty { .. }
        | InstructionValue::Debugger { .. } => {}
    }
}

pub fn for_each_pattern_place(pattern: &Pattern, f: &mut impl FnMut(&Place)) {
    match pattern {
        Pattern::Array(arr) => {
            for item in &arr.items {
                match item {
                    ArrayElement::Place(p) | ArrayElement::Spread(p) => f(p),
                    ArrayElement::Hole => {}
                }
            }
        }
        Pattern::Object(obj) => {
            for prop in &obj.properties {
                match prop {
                    ObjectPropertyOrSpread::Property(p) => {
                        f(&p.place);
                        if let ObjectPropertyKey::Computed(place) = &p.key {
                            f(place);
                        }
                    }
                    ObjectPropertyOrSpread::Spread(p) => f(p),
                }
            }
        }
    }
}

fn map_pattern_places(pattern: &mut Pattern, f: &mut impl FnMut(&mut Place)) {
    match pattern {
        Pattern::Array(arr) => {
            for item in &mut arr.items {
                match item {
                    ArrayElement::Place(p) | ArrayElement::Spread(p) => f(p),
                    ArrayElement::Hole => {}
                }
            }
        }
        Pattern::Object(obj) => {
            for prop in &mut obj.properties {
                match prop {
                    ObjectPropertyOrSpread::Property(p) => {
                        f(&mut p.place);
                        if let ObjectPropertyKey::Computed(place) = &mut p.key {
                            f(place);
                        }
                    }
                    ObjectPropertyOrSpread::Spread(p) => f(p),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tp(id: u32, name: &str) -> Place {
        Place {
            identifier: Identifier {
                id: IdentifierId(id),
                declaration_id: DeclarationId(id),
                name: Some(IdentifierName::Named(name.to_string())),
                mutable_range: MutableRange::default(),
                scope: None,
                type_: Type::Poly,
                loc: SourceLocation::Generated,
            },
            effect: Effect::Read,
            reactive: false,
            loc: SourceLocation::Generated,
        }
    }

    fn make_instr(value: InstructionValue) -> Instruction {
        Instruction {
            id: InstructionId(0),
            lvalue: tp(999, "$tmp"),
            value,
            loc: SourceLocation::Generated,
            effects: None,
        }
    }

    #[test]
    fn operand_visitor_load_local() {
        let instr = make_instr(InstructionValue::LoadLocal {
            place: tp(1, "a"),
            loc: SourceLocation::Generated,
        });
        let mut count = 0;
        for_each_instruction_operand(&instr, |_| count += 1);
        assert_eq!(count, 1);
    }

    #[test]
    fn operand_visitor_binary_expression() {
        let instr = make_instr(InstructionValue::BinaryExpression {
            operator: BinaryOperator::Add,
            left: tp(1, "a"),
            right: tp(2, "b"),
            loc: SourceLocation::Generated,
        });
        let mut count = 0;
        for_each_instruction_operand(&instr, |_| count += 1);
        assert_eq!(count, 2);
    }

    #[test]
    fn operand_visitor_primitive() {
        let instr = make_instr(InstructionValue::Primitive {
            value: PrimitiveValue::Number(42.0),
            loc: SourceLocation::Generated,
        });
        let mut count = 0;
        for_each_instruction_operand(&instr, |_| count += 1);
        assert_eq!(count, 0);
    }

    #[test]
    fn operand_visitor_store_local() {
        let instr = make_instr(InstructionValue::StoreLocal {
            lvalue: LValue {
                place: tp(1, "x"),
                kind: InstructionKind::Reassign,
            },
            value: tp(2, "y"),
            loc: SourceLocation::Generated,
        });
        let mut count = 0;
        for_each_instruction_operand(&instr, |_| count += 1);
        assert_eq!(count, 1);
    }

    #[test]
    fn operand_visitor_unary() {
        let instr = make_instr(InstructionValue::UnaryExpression {
            operator: UnaryOperator::Not,
            value: tp(1, "a"),
            loc: SourceLocation::Generated,
        });
        let mut count = 0;
        for_each_instruction_operand(&instr, |_| count += 1);
        assert_eq!(count, 1);
    }

    #[test]
    fn terminal_visitor_return() {
        let terminal = Terminal::Return {
            value: tp(1, "ret"),
            return_variant: ReturnVariant::Explicit,
            id: InstructionId(0),
            loc: SourceLocation::Generated,
        };
        let mut count = 0;
        for_each_terminal_operand(&terminal, |_| count += 1);
        assert_eq!(count, 1);
    }

    #[test]
    fn terminal_visitor_if() {
        let terminal = Terminal::If {
            test: tp(1, "cond"),
            consequent: BlockId(1),
            alternate: BlockId(2),
            fallthrough: BlockId(3),
            id: InstructionId(0),
            loc: SourceLocation::Generated,
        };
        let mut count = 0;
        for_each_terminal_operand(&terminal, |_| count += 1);
        assert_eq!(count, 1);
    }

    #[test]
    fn terminal_visitor_goto() {
        let terminal = Terminal::Goto {
            block: BlockId(1),
            variant: GotoVariant::Break,
            id: InstructionId(0),
            loc: SourceLocation::Generated,
        };
        let mut count = 0;
        for_each_terminal_operand(&terminal, |_| count += 1);
        assert_eq!(count, 0);
    }

    #[test]
    fn lvalue_visitor_store_local() {
        let instr = make_instr(InstructionValue::StoreLocal {
            lvalue: LValue {
                place: tp(1, "x"),
                kind: InstructionKind::Reassign,
            },
            value: tp(2, "y"),
            loc: SourceLocation::Generated,
        });
        let mut count = 0;
        for_each_instruction_lvalue(&instr, |_| count += 1);
        // 1 for instr.lvalue + 1 for StoreLocal's lvalue.place = 2
        assert_eq!(count, 2);
    }

    #[test]
    fn lvalue_visitor_primitive() {
        let instr = make_instr(InstructionValue::Primitive {
            value: PrimitiveValue::Null,
            loc: SourceLocation::Generated,
        });
        let mut count = 0;
        for_each_instruction_lvalue(&instr, |_| count += 1);
        // Only instr.lvalue, no additional lvalues from Primitive
        assert_eq!(count, 1);
    }
}
