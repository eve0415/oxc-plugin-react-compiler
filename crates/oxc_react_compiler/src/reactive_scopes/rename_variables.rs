//! Rename variables for deterministic output.
//!
//! Port of `RenameVariables.ts` from upstream React Compiler.
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.
//!
//! Ensures that each named variable in the function has a unique name that does
//! not conflict with any other variables in the same block scope. Temporaries
//! are renamed to `t0, t1, t2, ...` (or `T0, T1, ...` for JSX tag positions).

use std::collections::{HashMap, HashSet};

use crate::hir::types::*;

/// Renames variables in the reactive function so that temporaries use the
/// `t0, t1, ...` scheme and named variables get numeric suffixes to avoid
/// conflicts. Returns the set of unique identifier names used.
pub fn rename_variables(
    func: &mut ReactiveFunction,
    reserve_globals_for_codegen: bool,
    extra_reserved_names: Option<&HashSet<String>>,
) -> HashSet<String> {
    let mut globals = collect_referenced_globals(func, reserve_globals_for_codegen);
    if let Some(extra_reserved_names) = extra_reserved_names {
        globals.extend(extra_reserved_names.iter().cloned());
    }
    let mut scopes = Scopes::new(globals);
    rename_variables_impl(func, &mut scopes);
    if std::env::var("DEBUG_RENAME_VARIABLES").is_ok() {
        eprintln!(
            "[RENAME_VARIABLES] globals={:?} names={:?}",
            scopes.globals, scopes.names
        );
    }
    if reserve_globals_for_codegen {
        scopes.names.extend(scopes.globals.iter().cloned());
    }
    scopes.names
}

fn rename_variables_impl(func: &mut ReactiveFunction, scopes: &mut Scopes) {
    scopes.enter();
    for (param_idx, param) in func.params.iter_mut().enumerate() {
        let ident = match param {
            Argument::Place(place) => &mut place.identifier,
            Argument::Spread(place) => &mut place.identifier,
        };
        if ident.name.is_some() {
            scopes.visit(ident);
        } else {
            // Unnamed params get temp names (t0, t1...) in codegen.
            // Reserve those names in the scope so subsequent promoted temps
            // don't collide. This matches upstream where the parameter temp
            // name is accounted for in the rename pass.
            let temp_name = format!("t{param_idx}");
            if let Some(frame) = scopes.stack.last_mut() {
                frame.insert(temp_name.clone(), ident.declaration_id);
            }
            scopes.names.insert(temp_name);
        }
    }
    visit_block(&mut func.body, scopes);
    scopes.leave();
}

fn visit_block(block: &mut ReactiveBlock, scopes: &mut Scopes) {
    scopes.enter();
    for stmt in block.iter_mut() {
        visit_statement(stmt, scopes);
    }
    scopes.leave();
}

fn visit_statement(stmt: &mut ReactiveStatement, scopes: &mut Scopes) {
    match stmt {
        ReactiveStatement::Instruction(instr) => {
            visit_instruction(instr, scopes);
        }
        ReactiveStatement::Terminal(term) => {
            visit_terminal_stmt(term, scopes);
        }
        ReactiveStatement::Scope(scope_block) => {
            // Match upstream RenameVariables.ts:96-101: visit ONLY declarations
            // before the body. Dependencies and reassignments are visited AFTER
            // the body to match upstream's visit order (where they get renamed
            // via shared object references during body traversal).
            for decl in scope_block.scope.declarations.values_mut() {
                scopes.visit(&mut decl.identifier);
            }

            visit_block(&mut scope_block.instructions, scopes);

            // Post-body: update dependency and reassignment identifier copies.
            // In upstream TypeScript, these share object references with
            // instruction-level identifiers, so renaming happens automatically.
            // In Rust, we must explicitly visit them to copy the renamed names
            // from the seen map.
            for dep in &mut scope_block.scope.dependencies {
                scopes.visit(&mut dep.identifier);
            }

            for reassignment in &mut scope_block.scope.reassignments {
                scopes.visit(reassignment);
            }

            if let Some(early_return) = &mut scope_block.scope.early_return_value {
                scopes.visit(&mut early_return.value);
            }
        }
        ReactiveStatement::PrunedScope(scope_block) => {
            // Upstream RenameVariables.visitPrunedScope calls traverseBlock
            // directly (without enter/leave) so pruned scope body statements
            // are visited at the current scope level.
            for inner_stmt in scope_block.instructions.iter_mut() {
                visit_statement(inner_stmt, scopes);
            }
        }
    }
}

fn visit_instruction(instr: &mut ReactiveInstruction, scopes: &mut Scopes) {
    // Upstream traverseInstruction visits lvalue before value (visitors.ts:89-92).
    if let Some(lvalue) = &mut instr.lvalue {
        scopes.visit(&mut lvalue.identifier);
    }
    visit_instruction_value(&mut instr.value, scopes);
}

fn visit_instruction_value(value: &mut InstructionValue, scopes: &mut Scopes) {
    match value {
        InstructionValue::LoadLocal { place, .. } | InstructionValue::LoadContext { place, .. } => {
            scopes.visit(&mut place.identifier);
        }
        InstructionValue::StoreLocal {
            lvalue, value: val, ..
        }
        | InstructionValue::StoreContext {
            lvalue, value: val, ..
        } => {
            scopes.visit(&mut val.identifier);
            scopes.visit(&mut lvalue.place.identifier);
        }
        InstructionValue::DeclareLocal { lvalue, .. }
        | InstructionValue::DeclareContext { lvalue, .. } => {
            scopes.visit(&mut lvalue.place.identifier);
        }
        InstructionValue::Destructure {
            value: val,
            lvalue: pat,
            ..
        } => {
            scopes.visit(&mut val.identifier);
            visit_lvalue_pattern(pat, scopes);
        }
        InstructionValue::BinaryExpression { left, right, .. } => {
            scopes.visit(&mut left.identifier);
            scopes.visit(&mut right.identifier);
        }
        InstructionValue::UnaryExpression { value: val, .. } => {
            scopes.visit(&mut val.identifier);
        }
        InstructionValue::CallExpression { callee, args, .. } => {
            scopes.visit(&mut callee.identifier);
            for arg in args.iter_mut() {
                visit_argument(arg, scopes);
            }
        }
        InstructionValue::MethodCall {
            receiver,
            property,
            args,
            ..
        } => {
            scopes.visit(&mut receiver.identifier);
            scopes.visit(&mut property.identifier);
            for arg in args.iter_mut() {
                visit_argument(arg, scopes);
            }
        }
        InstructionValue::NewExpression { callee, args, .. } => {
            scopes.visit(&mut callee.identifier);
            for arg in args.iter_mut() {
                visit_argument(arg, scopes);
            }
        }
        InstructionValue::ObjectExpression { properties, .. } => {
            for prop in properties.iter_mut() {
                match prop {
                    ObjectPropertyOrSpread::Property(p) => {
                        if let ObjectPropertyKey::Computed(place) = &mut p.key {
                            scopes.visit(&mut place.identifier);
                        }
                        scopes.visit(&mut p.place.identifier);
                    }
                    ObjectPropertyOrSpread::Spread(place) => {
                        scopes.visit(&mut place.identifier);
                    }
                }
            }
        }
        InstructionValue::ArrayExpression { elements, .. } => {
            for elem in elements.iter_mut() {
                match elem {
                    ArrayElement::Place(place) | ArrayElement::Spread(place) => {
                        scopes.visit(&mut place.identifier);
                    }
                    ArrayElement::Hole => {}
                }
            }
        }
        InstructionValue::PropertyLoad { object, .. } => {
            scopes.visit(&mut object.identifier);
        }
        InstructionValue::PropertyStore {
            object, value: val, ..
        } => {
            scopes.visit(&mut object.identifier);
            scopes.visit(&mut val.identifier);
        }
        InstructionValue::PropertyDelete { object, .. } => {
            scopes.visit(&mut object.identifier);
        }
        InstructionValue::ComputedLoad {
            object, property, ..
        } => {
            scopes.visit(&mut object.identifier);
            scopes.visit(&mut property.identifier);
        }
        InstructionValue::ComputedStore {
            object,
            property,
            value: val,
            ..
        } => {
            scopes.visit(&mut object.identifier);
            scopes.visit(&mut property.identifier);
            scopes.visit(&mut val.identifier);
        }
        InstructionValue::ComputedDelete {
            object, property, ..
        } => {
            scopes.visit(&mut object.identifier);
            scopes.visit(&mut property.identifier);
        }
        InstructionValue::TypeCastExpression { value: val, .. } => {
            scopes.visit(&mut val.identifier);
        }
        InstructionValue::JsxExpression {
            tag,
            props,
            children,
            ..
        } => {
            if let JsxTag::Component(place) = tag {
                scopes.visit(&mut place.identifier);
            }
            for attr in props.iter_mut() {
                match attr {
                    JsxAttribute::Attribute { place, .. } => {
                        scopes.visit(&mut place.identifier);
                    }
                    JsxAttribute::SpreadAttribute { argument } => {
                        scopes.visit(&mut argument.identifier);
                    }
                }
            }
            if let Some(children) = children {
                for child in children.iter_mut() {
                    scopes.visit(&mut child.identifier);
                }
            }
        }
        InstructionValue::JsxFragment { children, .. } => {
            for child in children.iter_mut() {
                scopes.visit(&mut child.identifier);
            }
        }
        InstructionValue::FunctionExpression { lowered_func, .. } => {
            visit_hir_function_places(&mut lowered_func.func, scopes);
        }
        InstructionValue::ObjectMethod { lowered_func, .. } => {
            visit_hir_function_places(&mut lowered_func.func, scopes);
        }
        InstructionValue::Ternary {
            test,
            consequent,
            alternate,
            ..
        } => {
            scopes.visit(&mut test.identifier);
            scopes.visit(&mut consequent.identifier);
            scopes.visit(&mut alternate.identifier);
        }
        InstructionValue::LogicalExpression { left, right, .. } => {
            scopes.visit(&mut left.identifier);
            scopes.visit(&mut right.identifier);
        }
        InstructionValue::ReactiveSequenceExpression {
            instructions,
            value,
            ..
        } => {
            for instr in instructions.iter_mut() {
                if let Some(lvalue) = &mut instr.lvalue {
                    scopes.visit(&mut lvalue.identifier);
                }
                visit_instruction_value(&mut instr.value, scopes);
            }
            visit_instruction_value(value, scopes);
        }
        InstructionValue::ReactiveOptionalExpression { value, .. } => {
            visit_instruction_value(value, scopes);
        }
        InstructionValue::ReactiveLogicalExpression { left, right, .. } => {
            visit_instruction_value(left, scopes);
            visit_instruction_value(right, scopes);
        }
        InstructionValue::TaggedTemplateExpression { tag, .. } => {
            scopes.visit(&mut tag.identifier);
        }
        InstructionValue::TemplateLiteral { subexprs, .. } => {
            for expr in subexprs.iter_mut() {
                scopes.visit(&mut expr.identifier);
            }
        }
        InstructionValue::Await { value: val, .. } => {
            scopes.visit(&mut val.identifier);
        }
        InstructionValue::GetIterator { collection, .. } => {
            scopes.visit(&mut collection.identifier);
        }
        InstructionValue::IteratorNext {
            iterator,
            collection,
            ..
        } => {
            scopes.visit(&mut iterator.identifier);
            scopes.visit(&mut collection.identifier);
        }
        InstructionValue::NextPropertyOf { value: val, .. } => {
            scopes.visit(&mut val.identifier);
        }
        InstructionValue::PrefixUpdate {
            lvalue, value: val, ..
        }
        | InstructionValue::PostfixUpdate {
            lvalue, value: val, ..
        } => {
            scopes.visit(&mut lvalue.identifier);
            scopes.visit(&mut val.identifier);
        }
        InstructionValue::FinishMemoize { decl, .. } => {
            scopes.visit(&mut decl.identifier);
        }
        InstructionValue::StoreGlobal { value: val, .. } => {
            scopes.visit(&mut val.identifier);
        }
        // No places to visit
        InstructionValue::Primitive { .. }
        | InstructionValue::JSXText { .. }
        | InstructionValue::RegExpLiteral { .. }
        | InstructionValue::LoadGlobal { .. }
        | InstructionValue::StartMemoize { .. }
        | InstructionValue::Debugger { .. } => {}
    }
}

fn visit_argument(arg: &mut Argument, scopes: &mut Scopes) {
    match arg {
        Argument::Place(place) | Argument::Spread(place) => {
            scopes.visit(&mut place.identifier);
        }
    }
}

fn visit_lvalue_pattern(pat: &mut LValuePattern, scopes: &mut Scopes) {
    // Match upstream RenameVariables.ts: visitLValue calls visit() only, not promote.
    // Promotion is handled by PromoteUsedTemporaries.
    match &mut pat.pattern {
        Pattern::Array(arr) => {
            for elem in arr.items.iter_mut() {
                match elem {
                    ArrayElement::Place(place) | ArrayElement::Spread(place) => {
                        scopes.visit(&mut place.identifier);
                    }
                    ArrayElement::Hole => {}
                }
            }
        }
        Pattern::Object(obj) => {
            for prop in obj.properties.iter_mut() {
                match prop {
                    ObjectPropertyOrSpread::Property(p) => {
                        scopes.visit(&mut p.place.identifier);
                    }
                    ObjectPropertyOrSpread::Spread(place) => {
                        scopes.visit(&mut place.identifier);
                    }
                }
            }
        }
    }
}

fn visit_hir_function_places(func: &mut HIRFunction, scopes: &mut Scopes) {
    // Visit inner HIR function places to rename variables, matching upstream
    // ReactiveFunctionVisitor.visitHirFunction (visitors.ts:233-252).
    // This ensures inner function variables don't collide with outer scope names.
    for param in &mut func.params {
        match param {
            Argument::Place(place) => scopes.visit(&mut place.identifier),
            Argument::Spread(place) => scopes.visit(&mut place.identifier),
        }
    }
    for context_place in &mut func.context {
        scopes.visit(&mut context_place.identifier);
    }
    for (_, block) in &mut func.body.blocks {
        for instr in &mut block.instructions {
            // Visit lvalue
            scopes.visit(&mut instr.lvalue.identifier);
            // Visit instruction value places
            visit_hir_instruction_value(&mut instr.value, scopes);
        }
        // Visit terminal operands
        visit_hir_terminal_places(&mut block.terminal, scopes);
    }
}

fn visit_hir_instruction_value(value: &mut InstructionValue, scopes: &mut Scopes) {
    match value {
        InstructionValue::LoadLocal { place, .. } | InstructionValue::LoadContext { place, .. } => {
            scopes.visit(&mut place.identifier);
        }
        InstructionValue::StoreLocal {
            lvalue, value: val, ..
        }
        | InstructionValue::StoreContext {
            lvalue, value: val, ..
        } => {
            scopes.visit(&mut val.identifier);
            scopes.visit(&mut lvalue.place.identifier);
        }
        InstructionValue::DeclareLocal { lvalue, .. }
        | InstructionValue::DeclareContext { lvalue, .. } => {
            scopes.visit(&mut lvalue.place.identifier);
        }
        InstructionValue::Destructure {
            value: val,
            lvalue: pat,
            ..
        } => {
            scopes.visit(&mut val.identifier);
            visit_hir_pattern(pat, scopes);
        }
        InstructionValue::CallExpression { callee, args, .. } => {
            scopes.visit(&mut callee.identifier);
            for arg in args.iter_mut() {
                visit_hir_argument(arg, scopes);
            }
        }
        InstructionValue::MethodCall {
            receiver,
            property,
            args,
            ..
        } => {
            scopes.visit(&mut receiver.identifier);
            scopes.visit(&mut property.identifier);
            for arg in args.iter_mut() {
                visit_hir_argument(arg, scopes);
            }
        }
        InstructionValue::BinaryExpression { left, right, .. } => {
            scopes.visit(&mut left.identifier);
            scopes.visit(&mut right.identifier);
        }
        InstructionValue::UnaryExpression { value: val, .. } => {
            scopes.visit(&mut val.identifier);
        }
        InstructionValue::PropertyLoad { object, .. } => {
            scopes.visit(&mut object.identifier);
        }
        InstructionValue::PropertyStore {
            object, value: val, ..
        } => {
            scopes.visit(&mut object.identifier);
            scopes.visit(&mut val.identifier);
        }
        InstructionValue::ComputedLoad {
            object, property, ..
        } => {
            scopes.visit(&mut object.identifier);
            scopes.visit(&mut property.identifier);
        }
        InstructionValue::ComputedStore {
            object,
            property,
            value: val,
            ..
        } => {
            scopes.visit(&mut object.identifier);
            scopes.visit(&mut property.identifier);
            scopes.visit(&mut val.identifier);
        }
        InstructionValue::ObjectExpression { properties, .. } => {
            for prop in properties.iter_mut() {
                match prop {
                    ObjectPropertyOrSpread::Property(p) => {
                        if let ObjectPropertyKey::Computed(place) = &mut p.key {
                            scopes.visit(&mut place.identifier);
                        }
                        scopes.visit(&mut p.place.identifier);
                    }
                    ObjectPropertyOrSpread::Spread(place) => {
                        scopes.visit(&mut place.identifier);
                    }
                }
            }
        }
        InstructionValue::ArrayExpression { elements, .. } => {
            for elem in elements.iter_mut() {
                match elem {
                    ArrayElement::Place(place) | ArrayElement::Spread(place) => {
                        scopes.visit(&mut place.identifier);
                    }
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
            if let JsxTag::Component(place) = tag {
                scopes.visit(&mut place.identifier);
            }
            for attr in props.iter_mut() {
                match attr {
                    JsxAttribute::Attribute { place, .. } => {
                        scopes.visit(&mut place.identifier);
                    }
                    JsxAttribute::SpreadAttribute { argument } => {
                        scopes.visit(&mut argument.identifier);
                    }
                }
            }
            if let Some(children) = children {
                for child in children.iter_mut() {
                    scopes.visit(&mut child.identifier);
                }
            }
        }
        InstructionValue::JsxFragment { children, .. } => {
            for child in children.iter_mut() {
                scopes.visit(&mut child.identifier);
            }
        }
        InstructionValue::FunctionExpression { lowered_func, .. }
        | InstructionValue::ObjectMethod { lowered_func, .. } => {
            // Recursively visit inner functions (matching upstream visitors.ts:244-246)
            visit_hir_function_places(&mut lowered_func.func, scopes);
        }
        InstructionValue::Ternary {
            test,
            consequent,
            alternate,
            ..
        } => {
            scopes.visit(&mut test.identifier);
            scopes.visit(&mut consequent.identifier);
            scopes.visit(&mut alternate.identifier);
        }
        InstructionValue::LogicalExpression { left, right, .. } => {
            scopes.visit(&mut left.identifier);
            scopes.visit(&mut right.identifier);
        }
        InstructionValue::ReactiveSequenceExpression {
            instructions,
            value,
            ..
        } => {
            for instr in instructions.iter_mut() {
                if let Some(lvalue) = &mut instr.lvalue {
                    scopes.visit(&mut lvalue.identifier);
                }
                visit_hir_instruction_value(&mut instr.value, scopes);
            }
            visit_hir_instruction_value(value, scopes);
        }
        InstructionValue::ReactiveOptionalExpression { value, .. } => {
            visit_hir_instruction_value(value, scopes);
        }
        InstructionValue::ReactiveLogicalExpression { left, right, .. } => {
            visit_hir_instruction_value(left, scopes);
            visit_hir_instruction_value(right, scopes);
        }
        InstructionValue::NewExpression { callee, args, .. } => {
            scopes.visit(&mut callee.identifier);
            for arg in args.iter_mut() {
                visit_hir_argument(arg, scopes);
            }
        }
        InstructionValue::TypeCastExpression { value: val, .. } => {
            scopes.visit(&mut val.identifier);
        }
        InstructionValue::TaggedTemplateExpression { tag, .. } => {
            scopes.visit(&mut tag.identifier);
        }
        InstructionValue::TemplateLiteral { subexprs, .. } => {
            for expr in subexprs.iter_mut() {
                scopes.visit(&mut expr.identifier);
            }
        }
        InstructionValue::Await { value: val, .. } => {
            scopes.visit(&mut val.identifier);
        }
        InstructionValue::GetIterator { collection, .. } => {
            scopes.visit(&mut collection.identifier);
        }
        InstructionValue::IteratorNext {
            iterator,
            collection,
            ..
        } => {
            scopes.visit(&mut iterator.identifier);
            scopes.visit(&mut collection.identifier);
        }
        InstructionValue::NextPropertyOf { value: val, .. } => {
            scopes.visit(&mut val.identifier);
        }
        InstructionValue::PrefixUpdate {
            lvalue, value: val, ..
        }
        | InstructionValue::PostfixUpdate {
            lvalue, value: val, ..
        } => {
            scopes.visit(&mut lvalue.identifier);
            scopes.visit(&mut val.identifier);
        }
        InstructionValue::FinishMemoize { decl, .. } => {
            scopes.visit(&mut decl.identifier);
        }
        InstructionValue::StoreGlobal { value: val, .. } => {
            scopes.visit(&mut val.identifier);
        }
        InstructionValue::PropertyDelete { object, .. } => {
            scopes.visit(&mut object.identifier);
        }
        InstructionValue::ComputedDelete {
            object, property, ..
        } => {
            scopes.visit(&mut object.identifier);
            scopes.visit(&mut property.identifier);
        }
        InstructionValue::Primitive { .. }
        | InstructionValue::JSXText { .. }
        | InstructionValue::RegExpLiteral { .. }
        | InstructionValue::LoadGlobal { .. }
        | InstructionValue::StartMemoize { .. }
        | InstructionValue::Debugger { .. } => {}
    }
}

fn visit_hir_argument(arg: &mut Argument, scopes: &mut Scopes) {
    match arg {
        Argument::Place(place) | Argument::Spread(place) => {
            scopes.visit(&mut place.identifier);
        }
    }
}

fn visit_hir_pattern(pat: &mut LValuePattern, scopes: &mut Scopes) {
    match &mut pat.pattern {
        Pattern::Array(arr) => {
            for elem in arr.items.iter_mut() {
                match elem {
                    ArrayElement::Place(place) | ArrayElement::Spread(place) => {
                        scopes.visit(&mut place.identifier);
                    }
                    ArrayElement::Hole => {}
                }
            }
        }
        Pattern::Object(obj) => {
            for prop in obj.properties.iter_mut() {
                match prop {
                    ObjectPropertyOrSpread::Property(p) => {
                        scopes.visit(&mut p.place.identifier);
                    }
                    ObjectPropertyOrSpread::Spread(place) => {
                        scopes.visit(&mut place.identifier);
                    }
                }
            }
        }
    }
}

fn visit_hir_terminal_places(terminal: &mut Terminal, scopes: &mut Scopes) {
    match terminal {
        Terminal::Return { value, .. } | Terminal::Throw { value, .. } => {
            scopes.visit(&mut value.identifier);
        }
        Terminal::If { test, .. } | Terminal::Branch { test, .. } => {
            scopes.visit(&mut test.identifier);
        }
        Terminal::Switch { test, .. } => {
            scopes.visit(&mut test.identifier);
        }
        Terminal::Try {
            handler_binding, ..
        } => {
            if let Some(binding) = handler_binding {
                scopes.visit(&mut binding.identifier);
            }
        }
        Terminal::For { .. }
        | Terminal::ForOf { .. }
        | Terminal::ForIn { .. }
        | Terminal::While { .. }
        | Terminal::DoWhile { .. }
        | Terminal::Goto { .. }
        | Terminal::Label { .. }
        | Terminal::Scope { .. }
        | Terminal::PrunedScope { .. }
        | Terminal::Sequence { .. }
        | Terminal::Logical { .. }
        | Terminal::Ternary { .. }
        | Terminal::Optional { .. }
        | Terminal::Unsupported { .. }
        | Terminal::Unreachable { .. } => {}
    }
}

fn visit_terminal_stmt(term_stmt: &mut ReactiveTerminalStatement, scopes: &mut Scopes) {
    visit_terminal(&mut term_stmt.terminal, scopes);
}

fn visit_terminal(terminal: &mut ReactiveTerminal, scopes: &mut Scopes) {
    match terminal {
        ReactiveTerminal::Return { value, .. } | ReactiveTerminal::Throw { value, .. } => {
            scopes.visit(&mut value.identifier);
        }
        ReactiveTerminal::If {
            test,
            consequent,
            alternate,
            ..
        } => {
            scopes.visit(&mut test.identifier);
            visit_block(consequent, scopes);
            if let Some(alt) = alternate {
                visit_block(alt, scopes);
            }
        }
        ReactiveTerminal::Switch { test, cases, .. } => {
            scopes.visit(&mut test.identifier);
            for case in cases.iter_mut() {
                if let Some(t) = &mut case.test {
                    scopes.visit(&mut t.identifier);
                }
                if let Some(block) = &mut case.block {
                    visit_block(block, scopes);
                }
            }
        }
        ReactiveTerminal::DoWhile {
            loop_block, test, ..
        } => {
            visit_block(loop_block, scopes);
            scopes.visit(&mut test.identifier);
        }
        ReactiveTerminal::While {
            test, loop_block, ..
        } => {
            scopes.visit(&mut test.identifier);
            visit_block(loop_block, scopes);
        }
        ReactiveTerminal::For {
            init,
            test,
            update,
            loop_block,
            ..
        } => {
            visit_block(init, scopes);
            scopes.visit(&mut test.identifier);
            if let Some(upd) = update {
                visit_block(upd, scopes);
            }
            visit_block(loop_block, scopes);
        }
        ReactiveTerminal::ForOf {
            init,
            test,
            loop_block,
            ..
        } => {
            visit_block(init, scopes);
            scopes.visit(&mut test.identifier);
            visit_block(loop_block, scopes);
        }
        ReactiveTerminal::ForIn {
            init, loop_block, ..
        } => {
            visit_block(init, scopes);
            visit_block(loop_block, scopes);
        }
        ReactiveTerminal::Label { block, .. } => {
            visit_block(block, scopes);
        }
        ReactiveTerminal::Try {
            block,
            handler_binding,
            handler,
            ..
        } => {
            visit_block(block, scopes);
            if let Some(binding) = handler_binding {
                scopes.visit(&mut binding.identifier);
            }
            visit_block(handler, scopes);
        }
        ReactiveTerminal::Break { .. } | ReactiveTerminal::Continue { .. } => {}
    }
}

// ---------------------------------------------------------------------------
// Collect referenced globals
// ---------------------------------------------------------------------------

fn collect_referenced_globals(
    func: &ReactiveFunction,
    include_nested_hir_globals: bool,
) -> HashSet<String> {
    let mut globals = HashSet::new();
    collect_globals_block(&func.body, &mut globals, include_nested_hir_globals);
    globals
}

fn collect_globals_block(
    block: &ReactiveBlock,
    globals: &mut HashSet<String>,
    include_nested_hir_globals: bool,
) {
    for stmt in block {
        match stmt {
            ReactiveStatement::Instruction(instr) => {
                collect_globals_instruction_value(
                    &instr.value,
                    globals,
                    include_nested_hir_globals,
                );
            }
            ReactiveStatement::Terminal(term) => {
                collect_globals_terminal(&term.terminal, globals, include_nested_hir_globals);
            }
            ReactiveStatement::Scope(scope) => {
                collect_globals_block(&scope.instructions, globals, include_nested_hir_globals);
            }
            ReactiveStatement::PrunedScope(scope) => {
                collect_globals_block(&scope.instructions, globals, include_nested_hir_globals);
            }
        }
    }
}

fn collect_globals_instruction_value(
    value: &InstructionValue,
    globals: &mut HashSet<String>,
    include_nested_hir_globals: bool,
) {
    match value {
        InstructionValue::LoadGlobal { binding, .. } => {
            globals.insert(binding.name().to_string());
        }
        InstructionValue::FunctionExpression { lowered_func, .. }
        | InstructionValue::ObjectMethod { lowered_func, .. }
            if include_nested_hir_globals =>
        {
            collect_globals_hir_function(&lowered_func.func, globals);
        }
        _ => {}
    }
}

fn collect_globals_hir_function(func: &HIRFunction, globals: &mut HashSet<String>) {
    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            collect_globals_instruction_value(&instr.value, globals, true);
        }
    }
}

fn collect_globals_terminal(
    terminal: &ReactiveTerminal,
    globals: &mut HashSet<String>,
    include_nested_hir_globals: bool,
) {
    match terminal {
        ReactiveTerminal::If {
            consequent,
            alternate,
            ..
        } => {
            collect_globals_block(consequent, globals, include_nested_hir_globals);
            if let Some(alt) = alternate {
                collect_globals_block(alt, globals, include_nested_hir_globals);
            }
        }
        ReactiveTerminal::Switch { cases, .. } => {
            for case in cases {
                if let Some(block) = &case.block {
                    collect_globals_block(block, globals, include_nested_hir_globals);
                }
            }
        }
        ReactiveTerminal::DoWhile { loop_block, .. }
        | ReactiveTerminal::While { loop_block, .. } => {
            collect_globals_block(loop_block, globals, include_nested_hir_globals);
        }
        ReactiveTerminal::For {
            init,
            update,
            loop_block,
            ..
        } => {
            collect_globals_block(init, globals, include_nested_hir_globals);
            if let Some(upd) = update {
                collect_globals_block(upd, globals, include_nested_hir_globals);
            }
            collect_globals_block(loop_block, globals, include_nested_hir_globals);
        }
        ReactiveTerminal::ForOf {
            init, loop_block, ..
        } => {
            collect_globals_block(init, globals, include_nested_hir_globals);
            collect_globals_block(loop_block, globals, include_nested_hir_globals);
        }
        ReactiveTerminal::ForIn {
            init, loop_block, ..
        } => {
            collect_globals_block(init, globals, include_nested_hir_globals);
            collect_globals_block(loop_block, globals, include_nested_hir_globals);
        }
        ReactiveTerminal::Label { block, .. } => {
            collect_globals_block(block, globals, include_nested_hir_globals);
        }
        ReactiveTerminal::Try { block, handler, .. } => {
            collect_globals_block(block, globals, include_nested_hir_globals);
            collect_globals_block(handler, globals, include_nested_hir_globals);
        }
        ReactiveTerminal::Break { .. }
        | ReactiveTerminal::Continue { .. }
        | ReactiveTerminal::Return { .. }
        | ReactiveTerminal::Throw { .. } => {}
    }
}

// ---------------------------------------------------------------------------
// Scopes helper
// ---------------------------------------------------------------------------

struct Scopes {
    /// Maps DeclarationId -> already-assigned name for dedup across scopes.
    seen: HashMap<DeclarationId, IdentifierName>,
    /// Stack of scope frames. Each frame maps name -> DeclarationId.
    stack: Vec<HashMap<String, DeclarationId>>,
    /// Global names to avoid.
    globals: HashSet<String>,
    /// All unique names assigned so far.
    pub names: HashSet<String>,
}

impl Scopes {
    fn new(globals: HashSet<String>) -> Self {
        Self {
            seen: HashMap::new(),
            stack: vec![HashMap::new()],
            globals,
            names: HashSet::new(),
        }
    }

    fn visit(&mut self, identifier: &mut Identifier) {
        let debug_trace = std::env::var("DEBUG_RENAME_TRACE").is_ok();
        if let Some(mapped) = self.seen.get(&identifier.declaration_id) {
            if debug_trace {
                eprintln!(
                    "[RENAME_TRACE] reuse decl={} ident={} -> {} depth={}",
                    identifier.declaration_id.0,
                    identifier.id.0,
                    mapped.value(),
                    self.stack.len()
                );
            }
            identifier.name = Some(mapped.clone());
            return;
        }

        let original_name = match &identifier.name {
            Some(name) => name.clone(),
            None => {
                if debug_trace {
                    eprintln!(
                        "[RENAME_TRACE] skip unnamed decl={} ident={} depth={}",
                        identifier.declaration_id.0,
                        identifier.id.0,
                        self.stack.len()
                    );
                }
                return;
            }
        };

        let is_promoted_temp =
            matches!(&original_name, IdentifierName::Promoted(v) if v.starts_with("#t"));
        let is_promoted_jsx =
            matches!(&original_name, IdentifierName::Promoted(v) if v.starts_with("#T"));

        let mut name;
        let mut id: u32 = 0;

        if is_promoted_temp {
            name = format!("t{id}");
            id += 1;
        } else if is_promoted_jsx {
            name = format!("T{id}");
            id += 1;
        } else {
            name = original_name.value().to_string();
        }

        while self.lookup(&name).is_some() || self.globals.contains(&name) {
            if is_promoted_temp {
                name = format!("t{id}");
                id += 1;
            } else if is_promoted_jsx {
                name = format!("T{id}");
                id += 1;
            } else {
                // Upstream uses `$` in RenameVariables.ts:159.
                name = format!("{}${id}", original_name.value());
                id += 1;
            }
        }

        let new_ident_name = IdentifierName::Named(name.clone());
        if debug_trace {
            eprintln!(
                "[RENAME_TRACE] assign decl={} ident={} original={} -> {} depth={}",
                identifier.declaration_id.0,
                identifier.id.0,
                original_name.value(),
                name,
                self.stack.len()
            );
        }

        identifier.name = Some(new_ident_name.clone());
        self.seen.insert(identifier.declaration_id, new_ident_name);
        if let Some(frame) = self.stack.last_mut() {
            frame.insert(name.clone(), identifier.declaration_id);
        }
        self.names.insert(name);
    }

    fn lookup(&self, name: &str) -> Option<DeclarationId> {
        for frame in self.stack.iter().rev() {
            if let Some(&decl_id) = frame.get(name) {
                return Some(decl_id);
            }
        }
        None
    }

    fn enter(&mut self) {
        self.stack.push(HashMap::new());
    }

    fn leave(&mut self) {
        self.stack.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_identifier(id: u32, name: Option<IdentifierName>) -> Identifier {
        Identifier {
            id: IdentifierId(id),
            declaration_id: DeclarationId(id),
            name,
            mutable_range: MutableRange::default(),
            scope: None,
            type_: Type::Poly,
            loc: SourceLocation::Generated,
        }
    }

    fn make_test_place(id: u32, name: Option<IdentifierName>) -> Place {
        Place {
            identifier: make_test_identifier(id, name),
            effect: Effect::Unknown,
            reactive: false,
            loc: SourceLocation::Generated,
        }
    }

    #[test]
    fn test_rename_promoted_temporaries() {
        let mut func = ReactiveFunction {
            id: None,
            name_hint: None,
            params: vec![],
            body: vec![
                ReactiveStatement::Instruction(Box::new(ReactiveInstruction {
                    id: InstructionId(0),
                    lvalue: Some(make_test_place(
                        1,
                        Some(IdentifierName::Promoted("#t1".to_string())),
                    )),
                    value: InstructionValue::Primitive {
                        value: PrimitiveValue::Number(1.0),
                        loc: SourceLocation::Generated,
                    },
                    loc: SourceLocation::Generated,
                })),
                ReactiveStatement::Instruction(Box::new(ReactiveInstruction {
                    id: InstructionId(1),
                    lvalue: Some(make_test_place(
                        2,
                        Some(IdentifierName::Promoted("#t2".to_string())),
                    )),
                    value: InstructionValue::Primitive {
                        value: PrimitiveValue::Number(2.0),
                        loc: SourceLocation::Generated,
                    },
                    loc: SourceLocation::Generated,
                })),
            ],
        };

        let names = rename_variables(&mut func, false, None);

        assert!(names.contains("t0"));
        assert!(names.contains("t1"));

        if let ReactiveStatement::Instruction(instr) = &func.body[0] {
            assert_eq!(
                instr
                    .lvalue
                    .as_ref()
                    .unwrap()
                    .identifier
                    .name
                    .as_ref()
                    .unwrap()
                    .value(),
                "t0"
            );
        }
        if let ReactiveStatement::Instruction(instr) = &func.body[1] {
            assert_eq!(
                instr
                    .lvalue
                    .as_ref()
                    .unwrap()
                    .identifier
                    .name
                    .as_ref()
                    .unwrap()
                    .value(),
                "t1"
            );
        }
    }

    #[test]
    fn test_rename_named_variables_no_conflict() {
        let mut func = ReactiveFunction {
            id: None,
            name_hint: None,
            params: vec![Argument::Place(make_test_place(
                1,
                Some(IdentifierName::Named("x".to_string())),
            ))],
            body: vec![],
        };

        let names = rename_variables(&mut func, false, None);
        assert!(names.contains("x"));
    }
}
