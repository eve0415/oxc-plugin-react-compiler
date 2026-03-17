//! HIR Builder — constructs the control-flow graph.
//!
//! Port of `HIRBuilder.ts` from upstream React Compiler.
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.

use std::collections::{HashMap, HashSet};
use std::{cell::RefCell, rc::Rc};

use super::types::*;

/// A work-in-progress block that does not yet have a terminal.
pub struct WipBlock {
    pub id: BlockId,
    pub instructions: Vec<Instruction>,
    pub kind: BlockKind,
}

/// Scope for tracking break/continue targets.
enum Scope {
    Loop {
        label: Option<String>,
        continue_block: BlockId,
        break_block: BlockId,
    },
    Switch {
        label: Option<String>,
        break_block: BlockId,
    },
    Label {
        label: String,
        break_block: BlockId,
    },
}

/// Counter for generating unique IDs.
pub struct IdCounter {
    pub next_block: u32,
    pub next_identifier: u32,
}

impl Default for IdCounter {
    fn default() -> Self {
        Self::new()
    }
}

impl IdCounter {
    pub fn new() -> Self {
        Self {
            next_block: 0,
            next_identifier: 0,
        }
    }

    pub fn next_block_id(&mut self) -> BlockId {
        let id = BlockId::new(self.next_block);
        self.next_block += 1;
        id
    }

    pub fn next_identifier_id(&mut self) -> IdentifierId {
        let id = IdentifierId::new(self.next_identifier);
        self.next_identifier += 1;
        id
    }

    /// Observe an existing InstructionId (used as a proxy for identifier IDs
    /// in terminals) so the counter stays above it.
    pub fn observe_identifier_id(&mut self, id: InstructionId) {
        if id.0 >= self.next_identifier {
            self.next_identifier = id.0 + 1;
        }
    }

    /// Observe an existing IdentifierId so the counter stays above it.
    pub fn observe_identifier_id_from_ident(&mut self, id: IdentifierId) {
        if id.0 >= self.next_identifier {
            self.next_identifier = id.0 + 1;
        }
    }

    /// Observe an existing BlockId so the counter stays above it.
    pub fn observe_block_id(&mut self, id: BlockId) {
        if id.0 >= self.next_block {
            self.next_block = id.0 + 1;
        }
    }
}

/// Binding info for a variable.
#[derive(Clone)]
pub struct BindingEntry {
    pub name: String,
    pub identifier: Identifier,
    /// Whether this binding was declared with `const`.
    pub is_const: bool,
}

pub type Bindings = HashMap<String, BindingEntry>;

/// Helper class for constructing a control-flow graph.
pub struct HIRBuilder {
    completed: Vec<(BlockId, BasicBlock)>,
    current: WipBlock,
    entry: BlockId,
    scopes: Vec<Scope>,
    binding_scopes: Vec<Vec<(String, Option<BindingEntry>)>>,
    pub env: crate::environment::Environment,
    pub bindings: Bindings,
    exception_handlers: Vec<BlockId>,
    pub errors: Vec<crate::error::CompilerDiagnostic>,
    /// Nesting depth inside `<fbt>` / `<fbs>` JSX trees.
    pub fbt_depth: u32,
    /// Tracks emitted binding names per source-name family (`x`, `x_0`, ...).
    /// Shared across nested lowering to mirror upstream resolveBinding.
    binding_name_counters: Rc<RefCell<HashMap<String, u32>>>,
    /// Identifiers treated as context variables in this function.
    context_identifier_ids: HashSet<IdentifierId>,
    /// Stable insertion order for context identifiers.
    context_identifier_order: Vec<IdentifierId>,
    /// Canonical identifier payload for each context identifier.
    context_identifiers: HashMap<IdentifierId, Identifier>,
}

/// Recompute predecessor sets for all blocks in the HIR body.
pub fn mark_predecessors(body: &mut HIR) {
    let mut pred_map: HashMap<BlockId, HashSet<BlockId>> = HashMap::new();
    for (id, _) in &body.blocks {
        pred_map.insert(*id, HashSet::new());
    }
    for (_, block) in &body.blocks {
        for succ in each_terminal_successor(&block.terminal) {
            if let Some(preds) = pred_map.get_mut(&succ) {
                preds.insert(block.id);
            }
        }
    }
    for (_, block) in &mut body.blocks {
        if let Some(preds) = pred_map.remove(&block.id) {
            block.preds = preds;
        }
    }
}

/// Sort blocks in reverse postorder.
pub fn reverse_postorder_blocks(body: &mut HIR) {
    // Port of upstream `getReversePostorderedBlocks`:
    // - visit fallthrough first (as not-used) to preserve program structure
    // - visit control-flow successors in reverse order so siblings end up in source order
    // - retain only used blocks; keep used fallthrough-only blocks as unreachable shells
    let mut visited = HashSet::new();
    let mut used = HashSet::new();
    let mut used_fallthroughs = HashSet::new();
    let mut postorder = Vec::new();

    fn visit(
        id: BlockId,
        blocks: &HashMap<BlockId, &BasicBlock>,
        is_used: bool,
        visited: &mut HashSet<BlockId>,
        used: &mut HashSet<BlockId>,
        used_fallthroughs: &mut HashSet<BlockId>,
        postorder: &mut Vec<BlockId>,
    ) {
        let was_used = used.contains(&id);
        let was_visited = visited.contains(&id);
        visited.insert(id);
        if is_used {
            used.insert(id);
        }
        if was_visited && (was_used || !is_used) {
            return;
        }

        let Some(block) = blocks.get(&id) else {
            // Keep this tolerant to partially-invalid CFGs in debug/recovery flows.
            return;
        };

        let mut successors = each_terminal_successor(&block.terminal);
        successors.reverse();
        if let Some(fallthrough) = block.terminal.fallthrough() {
            if is_used {
                used_fallthroughs.insert(fallthrough);
            }
            visit(
                fallthrough,
                blocks,
                false,
                visited,
                used,
                used_fallthroughs,
                postorder,
            );
        }
        for succ in successors {
            visit(
                succ,
                blocks,
                is_used,
                visited,
                used,
                used_fallthroughs,
                postorder,
            );
        }

        if !was_visited {
            postorder.push(id);
        }
    }

    let block_map: HashMap<BlockId, &BasicBlock> =
        body.blocks.iter().map(|(id, b)| (*id, b)).collect();
    visit(
        body.entry,
        &block_map,
        true,
        &mut visited,
        &mut used,
        &mut used_fallthroughs,
        &mut postorder,
    );

    postorder.reverse();

    let mut new_blocks = Vec::new();
    let mut existing_blocks: HashMap<BlockId, BasicBlock> = body.blocks.drain(..).collect();

    for id in postorder {
        if used.contains(&id) {
            if let Some(block) = existing_blocks.remove(&id) {
                new_blocks.push((id, block));
            }
        } else if used_fallthroughs.contains(&id)
            && let Some(mut block) = existing_blocks.remove(&id)
        {
            let terminal_id = block.terminal.id();
            let loc = match &block.terminal {
                Terminal::Unsupported { loc, .. }
                | Terminal::Unreachable { loc, .. }
                | Terminal::Throw { loc, .. }
                | Terminal::Return { loc, .. }
                | Terminal::Goto { loc, .. }
                | Terminal::If { loc, .. }
                | Terminal::Branch { loc, .. }
                | Terminal::Switch { loc, .. }
                | Terminal::For { loc, .. }
                | Terminal::ForOf { loc, .. }
                | Terminal::ForIn { loc, .. }
                | Terminal::DoWhile { loc, .. }
                | Terminal::While { loc, .. }
                | Terminal::Logical { loc, .. }
                | Terminal::Ternary { loc, .. }
                | Terminal::Optional { loc, .. }
                | Terminal::Label { loc, .. }
                | Terminal::Sequence { loc, .. }
                | Terminal::Try { loc, .. }
                | Terminal::MaybeThrow { loc, .. }
                | Terminal::Scope { loc, .. }
                | Terminal::PrunedScope { loc, .. } => loc.clone(),
            };
            block.instructions.clear();
            block.terminal = Terminal::Unreachable {
                id: terminal_id,
                loc,
            };
            new_blocks.push((id, block));
        }
    }

    body.blocks = new_blocks;
}

/// Port of upstream `removeUnreachableForUpdates` (HIRBuilder.ts:725-735).
/// If a For terminal's update block was removed during dead-block elimination,
/// null out the update pointer.
pub fn remove_unreachable_for_updates(body: &mut HIR) {
    let block_ids: HashSet<BlockId> = body.blocks.iter().map(|(id, _)| *id).collect();
    for (_, block) in &mut body.blocks {
        if let Terminal::For { update, .. } = &mut block.terminal
            && let Some(update_id) = *update
            && !block_ids.contains(&update_id)
        {
            *update = None;
        }
    }
}

/// Port of upstream `removeDeadDoWhileStatements` (HIRBuilder.ts:737-761).
/// If a DoWhile terminal's test block is unreachable, replace the terminal
/// with a Goto to the loop body (effectively inlining the loop body once).
pub fn remove_dead_do_while_statements(body: &mut HIR) {
    let block_ids: HashSet<BlockId> = body.blocks.iter().map(|(id, _)| *id).collect();
    for (_, block) in &mut body.blocks {
        if let Terminal::DoWhile {
            test,
            loop_block,
            id,
            loc,
            ..
        } = &block.terminal
            && !block_ids.contains(test)
        {
            block.terminal = Terminal::Goto {
                block: *loop_block,
                variant: GotoVariant::Break,
                id: *id,
                loc: loc.clone(),
            };
        }
    }
}

/// Iterate control-flow successors using upstream `eachTerminalSuccessor` semantics.
///
/// This intentionally excludes pseudo-successors like `fallthrough`.
pub fn each_terminal_successor(terminal: &Terminal) -> Vec<BlockId> {
    match terminal {
        Terminal::Goto { block, .. } => vec![*block],
        Terminal::If {
            consequent,
            alternate,
            ..
        }
        | Terminal::Branch {
            consequent,
            alternate,
            ..
        } => vec![*consequent, *alternate],
        Terminal::Switch { cases, .. } => cases.iter().map(|c| c.block).collect(),
        Terminal::Optional { test, .. }
        | Terminal::Ternary { test, .. }
        | Terminal::Logical { test, .. } => vec![*test],
        Terminal::DoWhile { loop_block, .. } => vec![*loop_block],
        Terminal::While { test, .. } => vec![*test],
        Terminal::For { init, .. }
        | Terminal::ForOf { init, .. }
        | Terminal::ForIn { init, .. } => {
            vec![*init]
        }
        Terminal::Label { block, .. } | Terminal::Sequence { block, .. } => vec![*block],
        Terminal::MaybeThrow {
            continuation,
            handler,
            ..
        } => vec![*continuation, *handler],
        // Upstream `eachTerminalSuccessor` excludes `try.handler` because try
        // bodies are modeled with `MaybeThrow` edges. This Rust port does not
        // yet emit per-instruction `MaybeThrow` in try bodies, so excluding
        // handler here incorrectly drops reachable catch blocks during
        // reverse-postorder/minification passes.
        Terminal::Try { block, handler, .. } => vec![*block, *handler],
        Terminal::Scope { block, .. } | Terminal::PrunedScope { block, .. } => vec![*block],
        Terminal::Unsupported { .. }
        | Terminal::Unreachable { .. }
        | Terminal::Throw { .. }
        | Terminal::Return { .. } => vec![],
    }
}

impl HIRBuilder {
    pub fn new(env: crate::environment::Environment) -> Self {
        Self::new_with_binding_name_counters(env, Rc::new(RefCell::new(HashMap::new())))
    }

    pub fn new_with_binding_name_counters(
        env: crate::environment::Environment,
        binding_name_counters: Rc<RefCell<HashMap<String, u32>>>,
    ) -> Self {
        let entry = BlockId::new(env.next_block_id());
        let current = WipBlock {
            id: entry,
            instructions: Vec::new(),
            kind: BlockKind::Block,
        };
        Self {
            completed: Vec::new(),
            current,
            entry,
            scopes: Vec::new(),
            binding_scopes: Vec::new(),
            env,
            bindings: HashMap::new(),
            exception_handlers: Vec::new(),
            errors: Vec::new(),
            fbt_depth: 0,
            binding_name_counters,
            context_identifier_ids: HashSet::new(),
            context_identifier_order: Vec::new(),
            context_identifiers: HashMap::new(),
        }
    }

    pub fn binding_name_counters(&self) -> Rc<RefCell<HashMap<String, u32>>> {
        Rc::clone(&self.binding_name_counters)
    }

    /// Push a Todo diagnostic (unsupported construct).
    pub fn push_todo(&mut self, message: String) {
        self.errors.push(crate::error::CompilerDiagnostic {
            severity: crate::error::DiagnosticSeverity::Todo,
            message,
            category: Some(crate::error::ErrorCategory::Todo),
        });
    }

    /// Push an Invariant diagnostic (internal assumption violated).
    pub fn push_invariant(&mut self, message: String) {
        self.errors.push(crate::error::CompilerDiagnostic {
            severity: crate::error::DiagnosticSeverity::Invariant,
            message,
            category: Some(crate::error::ErrorCategory::Invariant),
        });
    }

    /// Check if any errors accumulated during lowering.
    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }

    /// Read-only view of completed blocks, used by lowering-time checks.
    pub fn completed_blocks(&self) -> &Vec<(BlockId, BasicBlock)> {
        &self.completed
    }

    /// Push an instruction onto the current block.
    pub fn push(&mut self, instruction: Instruction) {
        self.current.instructions.push(instruction);
    }

    /// Create a new temporary identifier.
    pub fn make_temporary(&mut self, loc: SourceLocation) -> Identifier {
        self.env.make_temporary_identifier(loc)
    }

    /// Create a temporary place.
    pub fn make_temporary_place(&mut self, loc: SourceLocation) -> Place {
        Place {
            identifier: self.make_temporary(loc.clone()),
            effect: Effect::Unknown,
            reactive: false,
            loc,
        }
    }

    /// Create a named place (for catch bindings, etc.).
    pub fn make_named_place(&mut self, name: String, loc: SourceLocation) -> Place {
        let id = IdentifierId::new(self.env.next_identifier_id());
        Place {
            identifier: Identifier {
                id,
                declaration_id: DeclarationId(id.0),
                name: Some(IdentifierName::Named(name)),
                mutable_range: MutableRange::default(),
                scope: None,
                type_: make_type(),
                loc: loc.clone(),
            },
            effect: Effect::Unknown,
            reactive: false,
            loc,
        }
    }

    /// Reserve a block that can be referenced before construction.
    pub fn reserve(&mut self, kind: BlockKind) -> WipBlock {
        let id = BlockId::new(self.env.next_block_id());
        WipBlock {
            id,
            instructions: Vec::new(),
            kind,
        }
    }

    /// Returns the kind of the currently active block.
    pub fn current_block_kind(&self) -> BlockKind {
        self.current.kind
    }

    /// Terminate the current block with the given terminal,
    /// and start a new block.
    pub fn terminate(&mut self, terminal: Terminal, next_block_kind: Option<BlockKind>) {
        let wip = std::mem::replace(
            &mut self.current,
            WipBlock {
                id: BlockId::new(u32::MAX),
                instructions: Vec::new(),
                kind: BlockKind::Block,
            },
        );
        self.completed.push((
            wip.id,
            BasicBlock {
                kind: wip.kind,
                id: wip.id,
                instructions: wip.instructions,
                terminal,
                preds: HashSet::new(),
                phis: Vec::new(),
            },
        ));
        if let Some(kind) = next_block_kind {
            let next_id = BlockId::new(self.env.next_block_id());
            self.current = WipBlock {
                id: next_id,
                instructions: Vec::new(),
                kind,
            };
        }
    }

    /// Terminate the current block and set a previously reserved block as current.
    pub fn terminate_with_continuation(&mut self, terminal: Terminal, continuation: WipBlock) {
        let wip = std::mem::replace(&mut self.current, continuation);
        self.completed.push((
            wip.id,
            BasicBlock {
                kind: wip.kind,
                id: wip.id,
                instructions: wip.instructions,
                terminal,
                preds: HashSet::new(),
                phis: Vec::new(),
            },
        ));
    }

    /// Create a new block, execute `f` to populate it, then reset to the previous block.
    /// Returns the ID of the newly created block.
    pub fn enter<F>(&mut self, kind: BlockKind, f: F) -> BlockId
    where
        F: FnOnce(&mut Self, BlockId) -> Terminal,
    {
        let wip = self.reserve(kind);
        let block_id = wip.id;
        let saved = std::mem::replace(&mut self.current, wip);
        let terminal = f(self, block_id);
        let populated = std::mem::replace(&mut self.current, saved);
        self.completed.push((
            populated.id,
            BasicBlock {
                kind: populated.kind,
                id: populated.id,
                instructions: populated.instructions,
                terminal,
                preds: HashSet::new(),
                phis: Vec::new(),
            },
        ));
        block_id
    }

    /// Populate a previously reserved block and append it to completed blocks.
    pub fn enter_reserved<F>(&mut self, reserved: WipBlock, f: F) -> BlockId
    where
        F: FnOnce(&mut Self, BlockId) -> Terminal,
    {
        let block_id = reserved.id;
        let saved = std::mem::replace(&mut self.current, reserved);
        let terminal = f(self, block_id);
        let populated = std::mem::replace(&mut self.current, saved);
        self.completed.push((
            populated.id,
            BasicBlock {
                kind: populated.kind,
                id: populated.id,
                instructions: populated.instructions,
                terminal,
                preds: HashSet::new(),
                phis: Vec::new(),
            },
        ));
        block_id
    }

    /// Push a loop scope for break/continue resolution.
    pub fn push_loop(
        &mut self,
        label: Option<String>,
        continue_block: BlockId,
        break_block: BlockId,
    ) {
        self.scopes.push(Scope::Loop {
            label,
            continue_block,
            break_block,
        });
    }

    pub fn pop_loop(&mut self) {
        self.scopes.pop();
    }

    /// Push a switch scope for break resolution.
    pub fn push_switch(&mut self, label: Option<String>, break_block: BlockId) {
        self.scopes.push(Scope::Switch { label, break_block });
    }

    pub fn pop_switch(&mut self) {
        self.scopes.pop();
    }

    /// Push a label scope.
    pub fn push_label(&mut self, label: String, break_block: BlockId) {
        self.scopes.push(Scope::Label { label, break_block });
    }

    pub fn pop_label(&mut self) {
        self.scopes.pop();
    }

    /// Lookup the break target for the given label (or innermost loop/switch if None).
    pub fn lookup_break(&self, label: Option<&str>) -> Option<BlockId> {
        for scope in self.scopes.iter().rev() {
            match scope {
                Scope::Loop {
                    label: l,
                    break_block,
                    ..
                }
                | Scope::Switch {
                    label: l,
                    break_block,
                    ..
                } => {
                    if label.is_none() || label == l.as_deref() {
                        return Some(*break_block);
                    }
                }
                Scope::Label {
                    label: l,
                    break_block,
                    ..
                } => {
                    if label == Some(l.as_str()) {
                        return Some(*break_block);
                    }
                }
            }
        }
        None
    }

    /// Lookup the continue target for the given label (or innermost loop if None).
    pub fn lookup_continue(&self, label: Option<&str>) -> Option<BlockId> {
        for scope in self.scopes.iter().rev() {
            if let Scope::Loop {
                label: l,
                continue_block,
                ..
            } = scope
                && (label.is_none() || label == l.as_deref())
            {
                return Some(*continue_block);
            }
        }
        None
    }

    /// Mark that we are lowering statements inside a try block.
    pub fn enter_try_context(&mut self) {
        self.exception_handlers.push(self.current.id);
    }

    /// Exit the current try-lowering context.
    pub fn exit_try_context(&mut self) {
        self.exception_handlers.pop();
    }

    /// True when lowering statements within a try block.
    pub fn in_try_context(&self) -> bool {
        !self.exception_handlers.is_empty()
    }

    /// Resolve a variable name to an Identifier, creating a new one if not seen before.
    pub fn resolve_binding(&mut self, name: &str, loc: SourceLocation) -> Identifier {
        if name == "fbt" {
            self.push_todo("Support local variables named `fbt`".to_string());
        }
        if let Some(entry) = self.bindings.get(name) {
            return entry.identifier.clone();
        }
        let id = IdentifierId::new(self.env.next_identifier_id());
        let decl_id = DeclarationId::new(id.0);
        let emitted_name = self.allocate_unique_binding_name(name);
        let identifier = Identifier {
            id,
            declaration_id: decl_id,
            name: Some(IdentifierName::Named(emitted_name.clone())),
            mutable_range: MutableRange::default(),
            scope: None,
            type_: make_type(),
            loc: loc.clone(),
        };
        self.bindings.insert(
            name.to_string(),
            BindingEntry {
                name: emitted_name,
                identifier: identifier.clone(),
                is_const: false,
            },
        );
        identifier
    }

    /// Enter a lexical binding scope (`let`/`const`/block function declarations).
    pub fn enter_binding_scope(&mut self) {
        self.binding_scopes.push(Vec::new());
    }

    /// Exit the current lexical binding scope and restore shadowed bindings.
    pub fn exit_binding_scope(&mut self) {
        let Some(mut changes) = self.binding_scopes.pop() else {
            return;
        };
        for (name, previous) in changes.drain(..).rev() {
            if let Some(entry) = previous {
                self.bindings.insert(name, entry);
            } else {
                self.bindings.remove(&name);
            }
        }
    }

    /// Declare a binding, optionally allowing lexical shadowing in the current scope.
    pub fn declare_binding(
        &mut self,
        name: &str,
        loc: SourceLocation,
        allow_lexical_shadowing: bool,
    ) -> Identifier {
        if allow_lexical_shadowing
            && !self.binding_scopes.is_empty()
            && !self
                .binding_scopes
                .last()
                .is_some_and(|scope| scope.iter().any(|(n, _)| n == name))
        {
            let id = IdentifierId::new(self.env.next_identifier_id());
            let decl_id = DeclarationId::new(id.0);
            let emitted_name = self.allocate_unique_binding_name(name);
            let identifier = Identifier {
                id,
                declaration_id: decl_id,
                name: Some(IdentifierName::Named(emitted_name.clone())),
                mutable_range: MutableRange::default(),
                scope: None,
                type_: make_type(),
                loc: loc.clone(),
            };
            let entry = BindingEntry {
                name: emitted_name,
                identifier: identifier.clone(),
                is_const: false,
            };
            let previous = self.bindings.insert(name.to_string(), entry);
            if let Some(scope) = self.binding_scopes.last_mut() {
                scope.push((name.to_string(), previous));
            }
            return identifier;
        }
        self.resolve_binding(name, loc)
    }

    fn allocate_unique_binding_name(&mut self, source_name: &str) -> String {
        let mut counters = self.binding_name_counters.borrow_mut();
        match counters.get_mut(source_name) {
            Some(next_suffix) => {
                let suffix = *next_suffix;
                *next_suffix += 1;
                format!("{source_name}_{suffix}")
            }
            None => {
                counters.insert(source_name.to_string(), 0);
                source_name.to_string()
            }
        }
    }

    /// Mark a binding as `const` (so reassignment can be detected as an error).
    pub fn mark_binding_const(&mut self, name: &str) {
        if let Some(entry) = self.bindings.get_mut(name) {
            entry.is_const = true;
        }
    }

    /// Check if a binding was declared as `const`.
    pub fn is_binding_const(&self, name: &str) -> bool {
        self.bindings.get(name).is_some_and(|e| e.is_const)
    }

    /// Mark an identifier as context-bound for this function.
    pub fn mark_context_identifier(&mut self, identifier: &Identifier) {
        let id = identifier.id;
        if self.context_identifier_ids.insert(id) {
            self.context_identifier_order.push(id);
        }
        self.context_identifiers.insert(id, identifier.clone());
    }

    /// True when the identifier id is context-bound in this function.
    pub fn is_context_identifier_id(&self, id: IdentifierId) -> bool {
        self.context_identifier_ids.contains(&id)
    }

    /// Materialize function context places in stable insertion order.
    pub fn context_places(&self) -> Vec<Place> {
        self.context_identifier_order
            .iter()
            .filter_map(|id| self.context_identifiers.get(id))
            .map(|identifier| Place {
                identifier: identifier.clone(),
                effect: Effect::Unknown,
                reactive: false,
                loc: identifier.loc.clone(),
            })
            .collect()
    }

    /// Build the final HIR from completed blocks.
    pub fn build(self) -> HIR {
        let mut hir = HIR {
            entry: self.entry,
            blocks: self.completed,
        };

        // Ensure RPO
        reverse_postorder_blocks(&mut hir);

        // Assign instruction IDs
        let mut instr_id = 0u32;
        for (_, block) in &mut hir.blocks {
            for instr in &mut block.instructions {
                instr_id += 1;
                instr.id = InstructionId::new(instr_id);
            }
            instr_id += 1;
            // Assign terminal ID based on terminal type
            assign_terminal_id(&mut block.terminal, InstructionId::new(instr_id));
        }

        // Mark predecessors
        mark_predecessors(&mut hir);

        hir
    }

    /// Upstream parity guard (HIRBuilder.ts:build):
    /// bail when an unreachable block still contains a FunctionExpression,
    /// because function declarations in unreachable regions may still hoist.
    pub fn detect_unreachable_hoisted_function_decls(&mut self) {
        let mut preview = HIR {
            entry: self.entry,
            blocks: self.completed.clone(),
        };
        reverse_postorder_blocks(&mut preview);
        let retained_ids: HashSet<BlockId> = preview.blocks.iter().map(|(id, _)| *id).collect();

        for (id, block) in &self.completed {
            if retained_ids.contains(id) {
                continue;
            }
            let has_function_expression = block
                .instructions
                .iter()
                .any(|instr| matches!(instr.value, InstructionValue::FunctionExpression { .. }));
            if has_function_expression {
                self.push_todo(
                    "Support functions with unreachable code that may contain hoisted declarations"
                        .to_string(),
                );
                break;
            }
        }
    }
}

fn assign_terminal_id(terminal: &mut Terminal, id: InstructionId) {
    match terminal {
        Terminal::Unsupported { id: tid, .. }
        | Terminal::Unreachable { id: tid, .. }
        | Terminal::Throw { id: tid, .. }
        | Terminal::Return { id: tid, .. }
        | Terminal::Goto { id: tid, .. }
        | Terminal::If { id: tid, .. }
        | Terminal::Branch { id: tid, .. }
        | Terminal::Switch { id: tid, .. }
        | Terminal::For { id: tid, .. }
        | Terminal::ForOf { id: tid, .. }
        | Terminal::ForIn { id: tid, .. }
        | Terminal::DoWhile { id: tid, .. }
        | Terminal::While { id: tid, .. }
        | Terminal::Logical { id: tid, .. }
        | Terminal::Ternary { id: tid, .. }
        | Terminal::Optional { id: tid, .. }
        | Terminal::Label { id: tid, .. }
        | Terminal::Sequence { id: tid, .. }
        | Terminal::Try { id: tid, .. }
        | Terminal::MaybeThrow { id: tid, .. }
        | Terminal::Scope { id: tid, .. }
        | Terminal::PrunedScope { id: tid, .. } => *tid = id,
    }
}

/// Get all successor block IDs from a terminal.
pub fn terminal_successors(terminal: &Terminal) -> Vec<BlockId> {
    match terminal {
        Terminal::Unsupported { .. } | Terminal::Unreachable { .. } => vec![],
        Terminal::Throw { .. } => vec![],
        Terminal::Return { .. } => vec![],
        Terminal::Goto { block, .. } => vec![*block],
        Terminal::If {
            consequent,
            alternate,
            ..
        } => vec![*consequent, *alternate],
        Terminal::Branch {
            consequent,
            alternate,
            ..
        } => vec![*consequent, *alternate],
        Terminal::Switch { cases, .. } => {
            // Match upstream: only case blocks are successors, NOT fallthrough.
            cases.iter().map(|c| c.block).collect()
        }
        Terminal::For {
            init,
            test,
            update,
            loop_block,
            fallthrough,
            ..
        } => {
            let mut succs = vec![*init, *test, *loop_block, *fallthrough];
            if let Some(u) = update {
                succs.push(*u);
            }
            succs
        }
        Terminal::ForOf {
            init,
            test,
            loop_block,
            fallthrough,
            ..
        } => {
            vec![*init, *test, *loop_block, *fallthrough]
        }
        Terminal::ForIn {
            init,
            loop_block,
            fallthrough,
            ..
        } => {
            vec![*init, *loop_block, *fallthrough]
        }
        Terminal::DoWhile {
            loop_block,
            test,
            fallthrough,
            ..
        } => {
            vec![*loop_block, *test, *fallthrough]
        }
        Terminal::While {
            test,
            loop_block,
            fallthrough,
            ..
        } => {
            vec![*test, *loop_block, *fallthrough]
        }
        Terminal::Logical {
            test, fallthrough, ..
        } => vec![*test, *fallthrough],
        Terminal::Ternary {
            test, fallthrough, ..
        } => vec![*test, *fallthrough],
        Terminal::Optional {
            test, fallthrough, ..
        } => vec![*test, *fallthrough],
        Terminal::Label {
            block, fallthrough, ..
        } => vec![*block, *fallthrough],
        Terminal::Sequence {
            block, fallthrough, ..
        } => vec![*block, *fallthrough],
        Terminal::Try {
            block,
            handler,
            fallthrough,
            ..
        } => {
            vec![*block, *handler, *fallthrough]
        }
        Terminal::MaybeThrow {
            continuation,
            handler,
            ..
        } => vec![*continuation, *handler],
        Terminal::Scope {
            block, fallthrough, ..
        } => vec![*block, *fallthrough],
        Terminal::PrunedScope {
            block, fallthrough, ..
        } => vec![*block, *fallthrough],
    }
}
