use crate::hir::types::{
    DeclarationId, InstructionValue, LogicalOperator, Place, ReactiveBlock, ReactiveFunction,
    ReactiveInstruction, ReactiveStatement, ReactiveTerminal,
};

/// Fuse a trailing `tmp = scopeValue ?? fallback; return tmp;` pattern back into
/// the preceding reactive scope so codegen caches the coalesced value directly.
pub fn fuse_trailing_nullish_return_into_scope(func: &mut ReactiveFunction) {
    fuse_block(&mut func.body);
}

fn fuse_block(block: &mut ReactiveBlock) {
    let mut i = 0usize;
    while i < block.len() {
        recurse_statement(&mut block[i]);
        let _ = maybe_fuse_scope_trailing_nullish_return(block, i);
        i += 1;
    }
}

fn recurse_statement(stmt: &mut ReactiveStatement) {
    match stmt {
        ReactiveStatement::Instruction(_) => {}
        ReactiveStatement::Scope(scope_block) => {
            fuse_block(&mut scope_block.instructions);
        }
        ReactiveStatement::PrunedScope(scope_block) => {
            fuse_block(&mut scope_block.instructions);
        }
        ReactiveStatement::Terminal(term_stmt) => {
            recurse_terminal(&mut term_stmt.terminal);
        }
    }
}

fn recurse_terminal(terminal: &mut ReactiveTerminal) {
    match terminal {
        ReactiveTerminal::Break { .. }
        | ReactiveTerminal::Continue { .. }
        | ReactiveTerminal::Return { .. }
        | ReactiveTerminal::Throw { .. } => {}
        ReactiveTerminal::If {
            consequent,
            alternate,
            ..
        } => {
            fuse_block(consequent);
            if let Some(alt) = alternate {
                fuse_block(alt);
            }
        }
        ReactiveTerminal::Switch { cases, .. } => {
            for case in cases {
                if let Some(block) = &mut case.block {
                    fuse_block(block);
                }
            }
        }
        ReactiveTerminal::DoWhile { loop_block, .. }
        | ReactiveTerminal::While { loop_block, .. } => {
            fuse_block(loop_block);
        }
        ReactiveTerminal::For {
            init,
            update,
            loop_block,
            ..
        } => {
            fuse_block(init);
            if let Some(update) = update {
                fuse_block(update);
            }
            fuse_block(loop_block);
        }
        ReactiveTerminal::ForOf {
            init, loop_block, ..
        }
        | ReactiveTerminal::ForIn {
            init, loop_block, ..
        } => {
            fuse_block(init);
            fuse_block(loop_block);
        }
        ReactiveTerminal::Label { block, .. } => {
            fuse_block(block);
        }
        ReactiveTerminal::Try { block, handler, .. } => {
            fuse_block(block);
            fuse_block(handler);
        }
    }
}

fn maybe_fuse_scope_trailing_nullish_return(block: &mut ReactiveBlock, scope_index: usize) -> bool {
    let Some(scope_stmt) = block.get(scope_index) else {
        return false;
    };
    let ReactiveStatement::Scope(scope_block) = scope_stmt else {
        return false;
    };

    if scope_block.scope.declarations.len() != 1 || !scope_block.scope.reassignments.is_empty() {
        return false;
    }

    let Some(scope_decl) = scope_block.scope.declarations.values().next() else {
        return false;
    };
    let source_decl = scope_decl.identifier.declaration_id;
    let Some(source_place) =
        find_scope_output_place(&scope_block.instructions, source_decl).cloned()
    else {
        return false;
    };

    enum TailKind {
        Return,
        Consumer,
    }

    let mut logical_offset: Option<usize> = None;
    let mut tail_kind: Option<TailKind> = None;
    let mut cursor = scope_index + 1;
    while cursor < block.len() {
        let ReactiveStatement::Instruction(instr) = &block[cursor] else {
            return false;
        };
        if let InstructionValue::LogicalExpression {
            operator,
            left,
            right,
            ..
        } = &instr.value
        {
            if *operator != LogicalOperator::NullishCoalescing {
                return false;
            }
            let Some(logical_lvalue) = &instr.lvalue else {
                return false;
            };
            if left.identifier.declaration_id != source_decl
                || right.identifier.declaration_id == source_decl
            {
                return false;
            }
            let Some(next_stmt) = block.get(cursor + 1) else {
                return false;
            };
            match next_stmt {
                ReactiveStatement::Terminal(term_stmt) => {
                    let ReactiveTerminal::Return { value, .. } = &term_stmt.terminal else {
                        return false;
                    };
                    if value.identifier.declaration_id != logical_lvalue.identifier.declaration_id {
                        return false;
                    }
                    tail_kind = Some(TailKind::Return);
                }
                ReactiveStatement::Instruction(next_instr) => {
                    if !is_fusable_nullish_consumer_instruction(
                        next_instr,
                        logical_lvalue.identifier.declaration_id,
                    ) {
                        return false;
                    }
                    tail_kind = Some(TailKind::Consumer);
                }
                _ => return false,
            }
            logical_offset = Some(cursor - (scope_index + 1));
            break;
        }
        if !is_fusable_inline_temp_instruction(instr) {
            return false;
        }
        cursor += 1;
    }

    let Some(logical_offset) = logical_offset else {
        return false;
    };
    let Some(tail_kind) = tail_kind else {
        return false;
    };

    let move_start = scope_index + 1;
    let move_end = move_start + logical_offset + 1;
    let mut moved: Vec<ReactiveStatement> = block.drain(move_start..move_end).collect();
    let Some(logical_stmt) = moved.pop() else {
        return false;
    };

    let ReactiveStatement::Scope(scope_block_mut) = &mut block[scope_index] else {
        return false;
    };
    scope_block_mut.instructions.extend(moved);
    let ReactiveStatement::Instruction(mut logical_instr) = logical_stmt else {
        return false;
    };
    let logical_temp_decl = logical_instr
        .lvalue
        .as_ref()
        .map(|p| p.identifier.declaration_id)
        .unwrap_or(source_decl);
    logical_instr.lvalue = Some(source_place.clone());
    scope_block_mut
        .instructions
        .push(ReactiveStatement::Instruction(logical_instr));

    match tail_kind {
        TailKind::Return => {
            let Some(ReactiveStatement::Terminal(term_stmt)) = block.get_mut(scope_index + 1)
            else {
                return false;
            };
            let ReactiveTerminal::Return { value, .. } = &mut term_stmt.terminal else {
                return false;
            };
            *value = source_place;
        }
        TailKind::Consumer => {
            let Some(ReactiveStatement::Instruction(instr)) = block.get_mut(scope_index + 1) else {
                return false;
            };
            rewrite_instruction_decl_use(instr, logical_temp_decl, &source_place);
        }
    }

    true
}

fn find_scope_output_place(
    block: &[ReactiveStatement],
    scope_decl: crate::hir::types::DeclarationId,
) -> Option<&crate::hir::types::Place> {
    for stmt in block.iter().rev() {
        let ReactiveStatement::Instruction(instr) = stmt else {
            continue;
        };
        let Some(lvalue) = &instr.lvalue else {
            continue;
        };
        if lvalue.identifier.declaration_id == scope_decl {
            return Some(lvalue);
        }
    }
    None
}

fn is_fusable_inline_temp_instruction(instr: &ReactiveInstruction) -> bool {
    let Some(lvalue) = &instr.lvalue else {
        return false;
    };
    if !is_fusable_temp_lvalue(lvalue) {
        return false;
    }
    !matches!(
        instr.value,
        InstructionValue::StoreLocal { .. }
            | InstructionValue::StoreContext { .. }
            | InstructionValue::DeclareLocal { .. }
            | InstructionValue::DeclareContext { .. }
            | InstructionValue::Destructure { .. }
            | InstructionValue::ObjectMethod { .. }
            | InstructionValue::StartMemoize { .. }
            | InstructionValue::FinishMemoize { .. }
            | InstructionValue::Debugger { .. }
            | InstructionValue::MethodCall { .. }
    )
}

fn is_fusable_temp_lvalue(place: &crate::hir::types::Place) -> bool {
    match place.identifier.name.as_ref() {
        None => true,
        Some(crate::hir::types::IdentifierName::Named(name))
        | Some(crate::hir::types::IdentifierName::Promoted(name)) => {
            is_codegen_temp_name(name.as_str())
        }
    }
}

fn is_codegen_temp_name(name: &str) -> bool {
    if !name.starts_with('t') || name.len() < 2 {
        return false;
    }
    name[1..].chars().all(|c| c.is_ascii_digit())
}

fn is_fusable_nullish_consumer_instruction(
    instr: &ReactiveInstruction,
    target_decl: DeclarationId,
) -> bool {
    if instr.lvalue.is_some() {
        return false;
    }
    match &instr.value {
        InstructionValue::CallExpression { callee, args, .. } => {
            place_uses_decl(callee, target_decl)
                || args.iter().any(|arg| argument_uses_decl(arg, target_decl))
        }
        InstructionValue::MethodCall {
            receiver,
            property,
            args,
            ..
        } => {
            place_uses_decl(receiver, target_decl)
                || place_uses_decl(property, target_decl)
                || args.iter().any(|arg| argument_uses_decl(arg, target_decl))
        }
        _ => false,
    }
}

fn rewrite_instruction_decl_use(
    instr: &mut ReactiveInstruction,
    target_decl: DeclarationId,
    replacement: &Place,
) {
    match &mut instr.value {
        InstructionValue::CallExpression { callee, args, .. } => {
            rewrite_place_decl(callee, target_decl, replacement);
            for arg in args {
                rewrite_argument_decl(arg, target_decl, replacement);
            }
        }
        InstructionValue::MethodCall {
            receiver,
            property,
            args,
            ..
        } => {
            rewrite_place_decl(receiver, target_decl, replacement);
            rewrite_place_decl(property, target_decl, replacement);
            for arg in args {
                rewrite_argument_decl(arg, target_decl, replacement);
            }
        }
        _ => {}
    }
}

fn place_uses_decl(place: &Place, target_decl: DeclarationId) -> bool {
    place.identifier.declaration_id == target_decl
}

fn argument_uses_decl(arg: &crate::hir::types::Argument, target_decl: DeclarationId) -> bool {
    match arg {
        crate::hir::types::Argument::Place(place) | crate::hir::types::Argument::Spread(place) => {
            place_uses_decl(place, target_decl)
        }
    }
}

fn rewrite_place_decl(place: &mut Place, target_decl: DeclarationId, replacement: &Place) {
    if place.identifier.declaration_id == target_decl {
        place.identifier = replacement.identifier.clone();
    }
}

fn rewrite_argument_decl(
    arg: &mut crate::hir::types::Argument,
    target_decl: DeclarationId,
    replacement: &Place,
) {
    match arg {
        crate::hir::types::Argument::Place(place) | crate::hir::types::Argument::Spread(place) => {
            rewrite_place_decl(place, target_decl, replacement);
        }
    }
}
