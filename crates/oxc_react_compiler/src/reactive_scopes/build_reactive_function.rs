//! BuildReactiveFunction — convert HIR CFG into a tree-shaped ReactiveFunction.
//!
//! Port of `BuildReactiveFunction.ts` from upstream React Compiler.
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.
//!
//! This pass walks the HIR control-flow graph depth-first, converting each
//! terminal into the corresponding tree node. It restores control flow constructs
//! (if/while/for/switch/try-catch/scope) from the CFG representation back to
//! tree form.

use std::collections::{HashMap, HashSet};

use crate::hir::types::{
    BasicBlock, BlockId, DeclarationId, Effect, GotoVariant, HIRFunction, Identifier,
    InstructionId, InstructionValue, Place, PrunedReactiveScopeBlock, ReactiveBlock,
    ReactiveFunction, ReactiveInstruction, ReactiveLabel, ReactiveScopeBlock, ReactiveStatement,
    ReactiveSwitchCase, ReactiveTerminal, ReactiveTerminalStatement, ReactiveTerminalTargetKind,
    SourceLocation, Terminal,
};

// ---------------------------------------------------------------------------
// Control flow target (schedule stack entry)
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum ControlFlowTarget {
    If {
        block: BlockId,
        id: usize,
    },
    Switch {
        block: BlockId,
        id: usize,
    },
    Case {
        block: BlockId,
        id: usize,
    },
    Loop {
        block: BlockId,
        owns_block: bool,
        continue_block: BlockId,
        loop_block: Option<BlockId>,
        owns_loop: bool,
        id: usize,
    },
}

impl ControlFlowTarget {
    fn block(&self) -> BlockId {
        match self {
            Self::If { block, .. }
            | Self::Switch { block, .. }
            | Self::Case { block, .. }
            | Self::Loop { block, .. } => *block,
        }
    }

    fn id(&self) -> usize {
        match self {
            Self::If { id, .. }
            | Self::Switch { id, .. }
            | Self::Case { id, .. }
            | Self::Loop { id, .. } => *id,
        }
    }

    fn is_loop(&self) -> bool {
        matches!(self, Self::Loop { .. })
    }
}

// ---------------------------------------------------------------------------
// Context — tracks scheduling state
// ---------------------------------------------------------------------------

struct Context {
    /// The HIR blocks, keyed by BlockId for O(1) lookup. We take ownership so
    /// we can move instructions out of blocks.
    blocks: HashMap<BlockId, BasicBlock>,

    /// Counter for generating unique schedule IDs.
    next_schedule_id: usize,

    /// Blocks that have already been emitted (for double-emit detection).
    emitted: HashSet<BlockId>,

    /// Fallthrough blocks belonging to scopes. When a goto targets one of these,
    /// it's an implicit break that should be elided.
    scope_fallthroughs: HashSet<BlockId>,

    /// Blocks that are currently scheduled (break targets).
    scheduled: HashSet<BlockId>,

    /// Catch handlers that are scheduled (treated as scheduled for `isScheduled`).
    catch_handlers: HashSet<BlockId>,

    /// The control flow stack. The innermost scope is last. This determines
    /// whether break/continue needs a label.
    control_flow_stack: Vec<ControlFlowTarget>,
}

impl Context {
    fn new(blocks: HashMap<BlockId, BasicBlock>) -> Self {
        Self {
            blocks,
            next_schedule_id: 0,
            emitted: HashSet::new(),
            scope_fallthroughs: HashSet::new(),
            scheduled: HashSet::new(),
            catch_handlers: HashSet::new(),
            control_flow_stack: Vec::new(),
        }
    }

    /// Check if a block's terminal is not `Unreachable`.
    fn reachable(&self, id: BlockId) -> bool {
        if let Some(block) = self.blocks.get(&id) {
            !matches!(block.terminal, Terminal::Unreachable { .. })
        } else {
            false
        }
    }

    /// Check if the given block is scheduled or is a catch handler.
    fn is_scheduled(&self, block: BlockId) -> bool {
        self.scheduled.contains(&block) || self.catch_handlers.contains(&block)
    }

    /// Schedule a catch handler block.
    fn schedule_catch_handler(&mut self, block: BlockId) {
        self.catch_handlers.insert(block);
    }

    /// Schedule a block as a break target (for if/switch/case).
    fn schedule(&mut self, block: BlockId, type_: &str) -> usize {
        let id = self.next_schedule_id;
        self.next_schedule_id += 1;
        assert!(
            !self.scheduled.contains(&block),
            "Break block is already scheduled: bb{}",
            block
        );
        self.scheduled.insert(block);
        let target = match type_ {
            "if" => ControlFlowTarget::If { block, id },
            "switch" => ControlFlowTarget::Switch { block, id },
            "case" => ControlFlowTarget::Case { block, id },
            _ => panic!("Unknown schedule type: {type_}"),
        };
        self.control_flow_stack.push(target);
        id
    }

    /// Schedule a loop's break/continue targets.
    fn schedule_loop(
        &mut self,
        fallthrough_block: BlockId,
        continue_block: BlockId,
        loop_block: Option<BlockId>,
    ) -> usize {
        let id = self.next_schedule_id;
        self.next_schedule_id += 1;

        let owns_block = !self.scheduled.contains(&fallthrough_block);
        self.scheduled.insert(fallthrough_block);

        assert!(
            !self.scheduled.contains(&continue_block),
            "Continue block is already scheduled: bb{}",
            continue_block
        );
        self.scheduled.insert(continue_block);

        let mut owns_loop = false;
        if let Some(lb) = loop_block {
            owns_loop = !self.scheduled.contains(&lb);
            self.scheduled.insert(lb);
        }

        self.control_flow_stack.push(ControlFlowTarget::Loop {
            block: fallthrough_block,
            owns_block,
            continue_block,
            loop_block,
            owns_loop,
            id,
        });
        id
    }

    /// Unschedule the most recently scheduled entry. Must match the given schedule_id.
    fn unschedule(&mut self, schedule_id: usize) {
        let last = self
            .control_flow_stack
            .pop()
            .expect("Cannot unschedule: control flow stack is empty");
        assert_eq!(
            last.id(),
            schedule_id,
            "Can only unschedule the last target (expected {}, got {})",
            schedule_id,
            last.id()
        );
        match &last {
            ControlFlowTarget::Loop {
                block,
                owns_block,
                continue_block,
                loop_block,
                owns_loop,
                ..
            } => {
                if *owns_block {
                    self.scheduled.remove(block);
                }
                self.scheduled.remove(continue_block);
                if *owns_loop && let Some(lb) = loop_block {
                    self.scheduled.remove(lb);
                }
            }
            other => {
                self.scheduled.remove(&other.block());
            }
        }
    }

    /// Unschedule multiple entries in reverse order (most recently scheduled first).
    fn unschedule_all(&mut self, schedule_ids: &[usize]) {
        for &id in schedule_ids.iter().rev() {
            self.unschedule(id);
        }
    }

    fn debug_control_flow_stack(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        for target in &self.control_flow_stack {
            match target {
                ControlFlowTarget::If { block, id } => {
                    parts.push(format!("if(bb{},id={})", block.0, id));
                }
                ControlFlowTarget::Switch { block, id } => {
                    parts.push(format!("switch(bb{},id={})", block.0, id));
                }
                ControlFlowTarget::Case { block, id } => {
                    parts.push(format!("case(bb{},id={})", block.0, id));
                }
                ControlFlowTarget::Loop {
                    block,
                    continue_block,
                    loop_block,
                    id,
                    ..
                } => {
                    let loop_block_str = loop_block.map_or("none".to_string(), |b| b.0.to_string());
                    parts.push(format!(
                        "loop(fallthrough=bb{},continue=bb{},loop=bb{},id={})",
                        block.0, continue_block.0, loop_block_str, id
                    ));
                }
            }
        }
        format!("[{}]", parts.join(" -> "))
    }

    /// Determine how a `break` to the given block must be emitted.
    /// Returns `None` if the block is not found on the control flow stack
    /// (e.g., when the target was never scheduled as a break target by any
    /// enclosing terminal, such as when scope boundaries produce gotos to
    /// blocks that are simple sequential fallthroughs).
    fn get_break_target(&self, block: BlockId) -> Option<(BlockId, ReactiveTerminalTargetKind)> {
        let mut has_preceding_loop = false;
        for i in (0..self.control_flow_stack.len()).rev() {
            let target = &self.control_flow_stack[i];
            let target_block = target.block();
            if target_block == block {
                let kind = if target.is_loop() {
                    if has_preceding_loop {
                        ReactiveTerminalTargetKind::Labeled
                    } else {
                        ReactiveTerminalTargetKind::Unlabeled
                    }
                } else if i == self.control_flow_stack.len() - 1 {
                    ReactiveTerminalTargetKind::Implicit
                } else {
                    ReactiveTerminalTargetKind::Labeled
                };
                if std::env::var("DEBUG_BREAK_TARGET").is_ok() {
                    eprintln!(
                        "[BREAK_TARGET] resolve target=bb{} via=bb{} kind={:?} stack={}",
                        block.0,
                        target_block.0,
                        kind,
                        self.debug_control_flow_stack()
                    );
                }
                return Some((target_block, kind));
            }
            if target.is_loop() {
                has_preceding_loop = true;
            }
        }
        // Handle intermediate break targets that alias through synthetic goto(Break) blocks.
        // Prefer outer labels so `break label` inside nested conditionals does not collapse
        // into an implicit break of the innermost `if`.
        let mut deferred_scope_fallthrough_alias: Option<(
            BlockId,
            ReactiveTerminalTargetKind,
            BlockId,
        )> = None;
        for i in 0..self.control_flow_stack.len() {
            let target = &self.control_flow_stack[i];
            let target_block = target.block();
            if !self.break_target_aliases_loop_fallthrough(target_block, block) {
                continue;
            }
            let has_preceding_loop = self.control_flow_stack[(i + 1)..]
                .iter()
                .any(ControlFlowTarget::is_loop);
            let (resolved_block, kind) = if matches!(target, ControlFlowTarget::Switch { .. }) {
                // Preserve explicit "break label" targets that exit a switch through
                // a synthetic switch-fallthrough goto chain.
                (block, ReactiveTerminalTargetKind::Labeled)
            } else if target.is_loop() {
                if has_preceding_loop {
                    (target_block, ReactiveTerminalTargetKind::Labeled)
                } else {
                    (target_block, ReactiveTerminalTargetKind::Unlabeled)
                }
            } else {
                (target_block, ReactiveTerminalTargetKind::Labeled)
            };
            if self.scope_fallthroughs.contains(&resolved_block) {
                // Prefer deeper alias candidates when the current outer candidate
                // is a scope fallthrough. Picking the outer fallthrough can
                // collapse labeled break structure and force return inlining.
                deferred_scope_fallthrough_alias.get_or_insert((
                    resolved_block,
                    kind,
                    target_block,
                ));
                continue;
            }
            if std::env::var("DEBUG_BREAK_TARGET").is_ok() {
                eprintln!(
                    "[BREAK_TARGET] resolve alias target=bb{} via=bb{} emit=bb{} kind={:?} stack={}",
                    block.0,
                    target_block.0,
                    resolved_block.0,
                    kind,
                    self.debug_control_flow_stack()
                );
            }
            return Some((resolved_block, kind));
        }
        if let Some((resolved_block, kind, via_block)) = deferred_scope_fallthrough_alias {
            if std::env::var("DEBUG_BREAK_TARGET").is_ok() {
                eprintln!(
                    "[BREAK_TARGET] defer alias(scope-fallthrough) target=bb{} via=bb{} emit=bb{} kind={:?} stack={}",
                    block.0,
                    via_block.0,
                    resolved_block.0,
                    kind,
                    self.debug_control_flow_stack()
                );
            }
            return None;
        }
        if std::env::var("DEBUG_BREAK_TARGET").is_ok() {
            eprintln!(
                "[BREAK_TARGET] unresolved target=bb{} stack={}",
                block.0,
                self.debug_control_flow_stack()
            );
        }
        None
    }

    /// Returns true when `from` reaches `target` by following only
    /// `goto(Break)` edges. This allows break-target resolution to treat
    /// simple loop-fallthrough aliases as equivalent break destinations.
    fn break_target_aliases_loop_fallthrough(&self, from: BlockId, target: BlockId) -> bool {
        if from == target {
            return true;
        }
        let mut current = from;
        let mut hops = 0usize;
        while hops < 8 {
            let Some(block) = self.blocks.get(&current) else {
                return false;
            };
            let Terminal::Goto {
                block: next,
                variant: GotoVariant::Break,
                ..
            } = block.terminal
            else {
                return false;
            };
            if next == target {
                return true;
            }
            current = next;
            hops += 1;
        }
        false
    }

    /// Follow synthetic empty `goto(Break)` chains and return the terminal
    /// destination block to use as a switch label.
    fn resolve_switch_label_block(&self, block: BlockId) -> BlockId {
        let mut current = block;
        let mut hops = 0usize;
        while hops < 8 {
            let Some(cur_block) = self.blocks.get(&current) else {
                break;
            };
            if !cur_block.instructions.is_empty() {
                break;
            }
            let Terminal::Goto {
                block: next,
                variant: GotoVariant::Break,
                ..
            } = cur_block.terminal
            else {
                break;
            };
            current = next;
            hops += 1;
        }
        current
    }

    /// Determine how a `continue` to the given block must be emitted.
    fn get_continue_target(&self, block: BlockId) -> Option<(BlockId, ReactiveTerminalTargetKind)> {
        let mut has_preceding_loop = false;
        for i in (0..self.control_flow_stack.len()).rev() {
            let target = &self.control_flow_stack[i];
            if let ControlFlowTarget::Loop {
                block: fallthrough_block,
                continue_block,
                ..
            } = target
                && *continue_block == block
            {
                let kind = if has_preceding_loop {
                    ReactiveTerminalTargetKind::Labeled
                } else if i == self.control_flow_stack.len() - 1 {
                    ReactiveTerminalTargetKind::Implicit
                } else {
                    ReactiveTerminalTargetKind::Unlabeled
                };
                return Some((*fallthrough_block, kind));
            }
            if target.is_loop() {
                has_preceding_loop = true;
            }
        }
        None
    }
}

// ---------------------------------------------------------------------------
// Value block result — used when extracting test/init/update values
// ---------------------------------------------------------------------------

/// The result of visiting a value block. Contains instructions that compute
/// a value, and the Place that holds the final result.
struct ValueBlockResult {
    /// The BlockId of the visited block.
    block_id: BlockId,
    /// Instructions that should be prepended (for sequence expressions).
    instructions: Vec<ReactiveInstruction>,
    /// The reconstructed value for the block, following upstream
    /// BuildReactiveFunction behavior.
    value: InstructionValue,
    /// The final place holding the computed value.
    place: Place,
    /// The instruction ID that produced the value.
    id: InstructionId,
    /// Branch targets for the final block when the reconstructed value still
    /// depends on a branch terminal. We must carry this through because
    /// `visit_value_block` consumes blocks as it traverses them.
    branch_targets: Option<(BlockId, BlockId)>,
}

/// The result of visiting a value block terminal (logical, ternary, optional, sequence).
struct ValueBlockTerminalResult {
    /// Instructions that must be emitted before the reconstructed terminal value.
    instructions: Vec<ReactiveInstruction>,
    /// The reconstructed terminal value instruction when upstream models the
    /// terminal as a value node rather than flattening it away.
    final_instruction: Option<ReactiveInstruction>,
    /// The fallthrough block to visit next.
    fallthrough: BlockId,
}

// ---------------------------------------------------------------------------
// Driver — the main traversal engine
// ---------------------------------------------------------------------------

struct Driver {
    cx: Context,
}

impl Driver {
    fn new(cx: Context) -> Self {
        Self { cx }
    }

    fn fallback_dummy_place() -> Place {
        Place {
            identifier: Identifier {
                id: crate::hir::types::make_identifier_id(0),
                declaration_id: DeclarationId::new(0),
                name: None,
                mutable_range: crate::hir::types::MutableRange::default(),
                scope: None,
                type_: crate::hir::types::Type::Primitive,
                loc: SourceLocation::default(),
            },
            effect: Effect::Unknown,
            reactive: false,
            loc: SourceLocation::default(),
        }
    }

    fn effective_value_place(result: &ValueBlockResult) -> Place {
        let Some(last) = result.instructions.last() else {
            return result.place.clone();
        };
        let Some(lvalue) = &last.lvalue else {
            return result.place.clone();
        };
        if lvalue.identifier.declaration_id != result.place.identifier.declaration_id {
            return result.place.clone();
        }
        match &last.value {
            InstructionValue::LoadLocal { place, .. }
            | InstructionValue::LoadContext { place, .. } => place.clone(),
            InstructionValue::TypeCastExpression { value, .. } => value.clone(),
            _ => result.place.clone(),
        }
    }

    fn last_emitted_instruction_place(block_value: &ReactiveBlock) -> Option<Place> {
        block_value.iter().rev().find_map(|stmt| {
            if let ReactiveStatement::Instruction(instr) = stmt {
                return instr.lvalue.clone();
            }
            None
        })
    }

    /// Traverse a block and all its successors, producing a ReactiveBlock.
    fn traverse_block(&mut self, block_id: BlockId) -> ReactiveBlock {
        let mut block_value: ReactiveBlock = Vec::new();
        self.visit_block(block_id, &mut block_value);
        block_value
    }

    /// Visit a block: emit its instructions, then process its terminal.
    fn visit_block(&mut self, block_id: BlockId, block_value: &mut ReactiveBlock) {
        if self.cx.emitted.contains(&block_id) {
            // Scope fallthrough blocks may be visited by both a path inside
            // the scope body and the scope's own fallthrough handler.  This is
            // safe to skip — the block was already emitted in the right place.
            if self.cx.scope_fallthroughs.contains(&block_id) {
                return;
            }
            panic!("Cannot emit the same block twice: bb{}", block_id);
        }

        // Take the block out of the map so we can move instructions.
        let Some(block) = self.cx.blocks.remove(&block_id) else {
            // Be tolerant to CFGs where a previously scheduled block was pruned
            // by earlier HIR transforms; upstream JS implementation skips these.
            return;
        };
        self.cx.emitted.insert(block_id);

        // Emit all instructions from this block.
        for instr in block.instructions {
            block_value.push(ReactiveStatement::Instruction(Box::new(
                ReactiveInstruction {
                    id: instr.id,
                    lvalue: Some(instr.lvalue),
                    value: instr.value,
                    loc: instr.loc,
                },
            )));
        }

        // Process the terminal.
        let terminal = block.terminal;
        let mut schedule_ids: Vec<usize> = Vec::new();

        match terminal {
            Terminal::Return { value, id, loc, .. } => {
                block_value.push(ReactiveStatement::Terminal(ReactiveTerminalStatement {
                    terminal: ReactiveTerminal::Return { value, id, loc },
                    label: None,
                }));
            }

            Terminal::Throw { value, id, loc } => {
                block_value.push(ReactiveStatement::Terminal(ReactiveTerminalStatement {
                    terminal: ReactiveTerminal::Throw { value, id, loc },
                    label: None,
                }));
            }

            Terminal::If {
                test,
                consequent,
                alternate,
                fallthrough,
                id,
                loc,
            } => {
                let fallthrough_id =
                    if self.cx.reachable(fallthrough) && !self.cx.is_scheduled(fallthrough) {
                        Some(fallthrough)
                    } else {
                        None
                    };
                let alternate_id = if alternate != fallthrough {
                    Some(alternate)
                } else {
                    None
                };

                if let Some(ft) = fallthrough_id {
                    let sid = self.cx.schedule(ft, "if");
                    schedule_ids.push(sid);
                }

                let consequent_block = if self.cx.is_scheduled(consequent) {
                    panic!("Unexpected 'if' where the consequent is already scheduled");
                } else {
                    self.traverse_block(consequent)
                };

                let alternate_block = if let Some(alt_id) = alternate_id {
                    if self.cx.is_scheduled(alt_id) {
                        panic!("Unexpected 'if' where the alternate is already scheduled");
                    } else {
                        Some(self.traverse_block(alt_id))
                    }
                } else {
                    None
                };

                self.cx.unschedule_all(&schedule_ids);
                block_value.push(ReactiveStatement::Terminal(ReactiveTerminalStatement {
                    terminal: ReactiveTerminal::If {
                        test,
                        consequent: consequent_block,
                        alternate: alternate_block,
                        id,
                        loc,
                    },
                    label: fallthrough_id.map(|ft| ReactiveLabel {
                        id: ft,
                        implicit: false,
                    }),
                }));
                if let Some(ft) = fallthrough_id {
                    self.visit_block(ft, block_value);
                }
            }

            Terminal::Switch {
                test,
                cases,
                fallthrough,
                id,
                loc,
            } => {
                let fallthrough_id =
                    if self.cx.reachable(fallthrough) && !self.cx.is_scheduled(fallthrough) {
                        Some(fallthrough)
                    } else {
                        None
                    };
                let outer_switch_label_id =
                    fallthrough_id.map(|ft| self.cx.resolve_switch_label_block(ft));
                if let Some(ft) = fallthrough_id {
                    let sid = self.cx.schedule(ft, "switch");
                    schedule_ids.push(sid);
                }

                let mut reactive_cases: Vec<ReactiveSwitchCase> = Vec::new();
                // Process cases in reverse order (upstream pattern).
                let mut reversed_cases: Vec<_> = cases.into_iter().collect();
                reversed_cases.reverse();
                for case in reversed_cases {
                    let case_test = case.test;
                    if self.cx.is_scheduled(case.block) {
                        assert_eq!(
                            case.block, fallthrough,
                            "Unexpected 'switch' where a case is already scheduled and block is not the fallthrough"
                        );
                        continue;
                    }
                    let consequent = self.traverse_block(case.block);
                    let sid = self.cx.schedule(case.block, "case");
                    schedule_ids.push(sid);
                    reactive_cases.push(ReactiveSwitchCase {
                        test: case_test,
                        block: Some(consequent),
                    });
                }
                reactive_cases.reverse();

                self.cx.unschedule_all(&schedule_ids);
                let inner_switch_stmt = ReactiveStatement::Terminal(ReactiveTerminalStatement {
                    terminal: ReactiveTerminal::Switch {
                        test,
                        cases: reactive_cases,
                        id,
                        loc: loc.clone(),
                    },
                    label: fallthrough_id.map(|ft| ReactiveLabel {
                        id: ft,
                        implicit: false,
                    }),
                });
                if let (Some(outer_label), Some(inner_label)) =
                    (outer_switch_label_id, fallthrough_id)
                    && outer_label != inner_label
                {
                    block_value.push(ReactiveStatement::Terminal(ReactiveTerminalStatement {
                        terminal: ReactiveTerminal::Label {
                            block: vec![inner_switch_stmt],
                            id,
                            loc,
                        },
                        label: Some(ReactiveLabel {
                            id: outer_label,
                            implicit: false,
                        }),
                    }));
                } else {
                    block_value.push(inner_switch_stmt);
                }
                if let Some(ft) = fallthrough_id {
                    self.visit_block(ft, block_value);
                }
            }

            Terminal::DoWhile {
                loop_block,
                test,
                fallthrough,
                id,
                loc,
            } => {
                let fallthrough_id = if !self.cx.is_scheduled(fallthrough) {
                    Some(fallthrough)
                } else {
                    None
                };
                let loop_id = if !self.cx.is_scheduled(loop_block) && loop_block != fallthrough {
                    Some(loop_block)
                } else {
                    None
                };

                let sid = self.cx.schedule_loop(fallthrough, test, Some(loop_block));
                schedule_ids.push(sid);

                let loop_body = if let Some(lid) = loop_id {
                    self.traverse_block(lid)
                } else {
                    panic!("Unexpected 'do-while' where the loop is already scheduled");
                };

                let test_result = self.visit_value_block(test, &loc);

                self.cx.unschedule_all(&schedule_ids);

                let test_place = Self::effective_value_place(&test_result);

                // Emit test instructions so they populate the temp map for inlining.
                for instr in test_result.instructions {
                    block_value.push(ReactiveStatement::Instruction(Box::new(instr)));
                }

                block_value.push(ReactiveStatement::Terminal(ReactiveTerminalStatement {
                    terminal: ReactiveTerminal::DoWhile {
                        loop_block: loop_body,
                        test: test_place,
                        id,
                        loc,
                    },
                    label: fallthrough_id.map(|ft| ReactiveLabel {
                        id: ft,
                        implicit: false,
                    }),
                }));
                if let Some(ft) = fallthrough_id {
                    self.visit_block(ft, block_value);
                }
            }

            Terminal::While {
                test,
                loop_block,
                fallthrough,
                id,
                loc,
            } => {
                let fallthrough_id =
                    if self.cx.reachable(fallthrough) && !self.cx.is_scheduled(fallthrough) {
                        Some(fallthrough)
                    } else {
                        None
                    };
                let loop_id = if !self.cx.is_scheduled(loop_block) && loop_block != fallthrough {
                    Some(loop_block)
                } else {
                    None
                };

                let sid = self.cx.schedule_loop(fallthrough, test, Some(loop_block));
                schedule_ids.push(sid);

                let test_result = self.visit_value_block(test, &loc);

                let loop_body = if let Some(lid) = loop_id {
                    self.traverse_block(lid)
                } else {
                    panic!("Unexpected 'while' where the loop is already scheduled");
                };

                self.cx.unschedule_all(&schedule_ids);

                let mut test_place = Self::effective_value_place(&test_result);

                // Emit test instructions as leading statements (they populate
                // the temp map so codegen can inline the test expression).
                // Also detect orphaned side-effecting temps (from sequence
                // expressions like `while ((foo(), true))`) and wrap them
                // into a ReactiveSequenceExpression that codegen inlines.
                let mut test_instrs = test_result.instructions;
                if test_instrs.len() > 1 {
                    let last = test_instrs.last().unwrap().clone();
                    let prefix_slice = &test_instrs[..test_instrs.len() - 1];
                    let has_orphaned = prefix_slice.iter().any(|instr| {
                        if let Some(lv) = &instr.lvalue
                            && lv.identifier.name.is_none()
                            && matches!(
                                instr.value,
                                InstructionValue::CallExpression { .. }
                                    | InstructionValue::MethodCall { .. }
                                    | InstructionValue::PostfixUpdate { .. }
                                    | InstructionValue::PrefixUpdate { .. }
                            )
                        {
                            // Check against ALL other instructions (prefix + last),
                            // not just the last, to avoid false positives when a
                            // prefix instruction is consumed by another prefix instruction.
                            let others: Vec<_> = prefix_slice
                                .iter()
                                .chain(std::iter::once(&last))
                                .filter(|i| {
                                    i.lvalue.as_ref().map(|ilv| ilv.identifier.declaration_id)
                                        != Some(lv.identifier.declaration_id)
                                })
                                .cloned()
                                .collect();
                            !is_decl_id_referenced_in_instructions(
                                lv.identifier.declaration_id,
                                &others,
                            )
                        } else {
                            false
                        }
                    });
                    if has_orphaned {
                        let prefix: Vec<_> = prefix_slice.to_vec();
                        let seq_value = InstructionValue::ReactiveSequenceExpression {
                            instructions: prefix,
                            id: last.id,
                            value: Box::new(last.value.clone()),
                            loc: loc.clone(),
                        };
                        // Replace last instruction with the sequence
                        // and update test_place to match its lvalue so
                        // codegen picks up the comma expression.
                        let seq_lvalue = last.lvalue.clone().unwrap_or_else(|| test_place.clone());
                        let seq_instr = ReactiveInstruction {
                            id: last.id,
                            lvalue: Some(seq_lvalue.clone()),
                            value: seq_value,
                            loc: loc.clone(),
                        };
                        // Remove the flat prefix instructions and the
                        // original last; emit only the sequence wrapper.
                        test_instrs.clear();
                        test_instrs.push(seq_instr);
                        // Update test_place to point to the sequence.
                        test_place = seq_lvalue;
                    }
                }
                for instr in test_instrs {
                    block_value.push(ReactiveStatement::Instruction(Box::new(instr)));
                }

                block_value.push(ReactiveStatement::Terminal(ReactiveTerminalStatement {
                    terminal: ReactiveTerminal::While {
                        test: test_place,
                        loop_block: loop_body,
                        id,
                        loc,
                    },
                    label: fallthrough_id.map(|ft| ReactiveLabel {
                        id: ft,
                        implicit: false,
                    }),
                }));
                if let Some(ft) = fallthrough_id {
                    self.visit_block(ft, block_value);
                }
            }

            Terminal::For {
                init,
                test,
                update,
                loop_block,
                fallthrough,
                id,
                loc,
            } => {
                let loop_id = if !self.cx.is_scheduled(loop_block) && loop_block != fallthrough {
                    Some(loop_block)
                } else {
                    None
                };
                let fallthrough_id = if !self.cx.is_scheduled(fallthrough) {
                    Some(fallthrough)
                } else {
                    None
                };

                let continue_target = update.unwrap_or(test);
                let sid = self
                    .cx
                    .schedule_loop(fallthrough, continue_target, Some(loop_block));
                schedule_ids.push(sid);

                let init_result = self.visit_value_block(init, &loc);
                let mut test_result = if test == block_id {
                    // Some lowered loop forms keep the test computation in the same
                    // header block that owns this terminal. That block has already been
                    // removed/emitted above, so reading it again as a value block would
                    // return the dummy place and break `for (...; test; ...)` codegen.
                    let place = Self::last_emitted_instruction_place(block_value)
                        .unwrap_or_else(Self::fallback_dummy_place);
                    ValueBlockResult {
                        block_id: test,
                        instructions: Vec::new(),
                        value: InstructionValue::LoadLocal {
                            place: place.clone(),
                            loc: place.loc.clone(),
                        },
                        place,
                        id,
                        branch_targets: None,
                    }
                } else {
                    self.visit_value_block(test, &loc)
                };
                if test_result.place.identifier.id == crate::hir::types::make_identifier_id(0)
                    && let Some(place) = test_result
                        .instructions
                        .iter()
                        .rev()
                        .find_map(|instr| instr.lvalue.clone())
                        .or_else(|| Self::last_emitted_instruction_place(block_value))
                {
                    test_result.place = place;
                }

                let (update_block, update_value) = if let Some(upd) = update {
                    let upd_result = self.visit_value_block(upd, &loc);
                    let mut stmts: ReactiveBlock = Vec::new();
                    for instr in upd_result.instructions {
                        stmts.push(ReactiveStatement::Instruction(Box::new(instr)));
                    }
                    (Some(stmts), Some(Box::new(upd_result.value)))
                } else {
                    (None, None)
                };

                let loop_body = if let Some(lid) = loop_id {
                    self.traverse_block(lid)
                } else {
                    panic!("Unexpected 'for' where the loop is already scheduled");
                };

                self.cx.unschedule_all(&schedule_ids);

                // Build init block from instructions
                let mut init_block: ReactiveBlock = Vec::new();
                for instr in init_result.instructions {
                    init_block.push(ReactiveStatement::Instruction(Box::new(instr)));
                }

                let test_place = Self::effective_value_place(&test_result);

                // Emit test instructions as leading statements for temp map inlining.
                for instr in test_result.instructions {
                    block_value.push(ReactiveStatement::Instruction(Box::new(instr)));
                }

                block_value.push(ReactiveStatement::Terminal(ReactiveTerminalStatement {
                    terminal: ReactiveTerminal::For {
                        init: init_block,
                        test: test_place,
                        update: update_block,
                        update_value,
                        loop_block: loop_body,
                        id,
                        loc,
                    },
                    label: fallthrough_id.map(|ft| ReactiveLabel {
                        id: ft,
                        implicit: false,
                    }),
                }));
                if let Some(ft) = fallthrough_id {
                    self.visit_block(ft, block_value);
                }
            }

            Terminal::ForOf {
                init,
                test,
                loop_block,
                fallthrough,
                id,
                loc,
            } => {
                let loop_id = if !self.cx.is_scheduled(loop_block) && loop_block != fallthrough {
                    Some(loop_block)
                } else {
                    None
                };
                let fallthrough_id = if !self.cx.is_scheduled(fallthrough) {
                    Some(fallthrough)
                } else {
                    None
                };

                let sid = self.cx.schedule_loop(fallthrough, init, Some(loop_block));
                schedule_ids.push(sid);

                let init_result = self.visit_value_block(init, &loc);
                let test_result = self.visit_value_block(test, &loc);

                let loop_body = if let Some(lid) = loop_id {
                    self.traverse_block(lid)
                } else {
                    panic!("Unexpected 'for-of' where the loop is already scheduled");
                };

                self.cx.unschedule_all(&schedule_ids);

                let mut init_block: ReactiveBlock = Vec::new();
                for instr in init_result.instructions {
                    init_block.push(ReactiveStatement::Instruction(Box::new(instr)));
                }
                // Keep the for-of test sequence attached to the terminal payload,
                // matching upstream semantics where both init/test contribute to
                // the loop header reconstruction during codegen.
                let test_place = Self::effective_value_place(&test_result);
                for instr in test_result.instructions {
                    init_block.push(ReactiveStatement::Instruction(Box::new(instr)));
                }

                block_value.push(ReactiveStatement::Terminal(ReactiveTerminalStatement {
                    terminal: ReactiveTerminal::ForOf {
                        init: init_block,
                        test: test_place,
                        loop_block: loop_body,
                        id,
                        loc,
                    },
                    label: fallthrough_id.map(|ft| ReactiveLabel {
                        id: ft,
                        implicit: false,
                    }),
                }));
                if let Some(ft) = fallthrough_id {
                    self.visit_block(ft, block_value);
                }
            }

            Terminal::ForIn {
                init,
                loop_block,
                fallthrough,
                id,
                loc,
            } => {
                let loop_id = if !self.cx.is_scheduled(loop_block) && loop_block != fallthrough {
                    Some(loop_block)
                } else {
                    None
                };
                let fallthrough_id = if !self.cx.is_scheduled(fallthrough) {
                    Some(fallthrough)
                } else {
                    None
                };

                let sid = self.cx.schedule_loop(fallthrough, init, Some(loop_block));
                schedule_ids.push(sid);

                let init_result = self.visit_value_block(init, &loc);

                let loop_body = if let Some(lid) = loop_id {
                    self.traverse_block(lid)
                } else {
                    panic!("Unexpected 'for-in' where the loop is already scheduled");
                };

                self.cx.unschedule_all(&schedule_ids);

                let mut init_block: ReactiveBlock = Vec::new();
                for instr in init_result.instructions {
                    init_block.push(ReactiveStatement::Instruction(Box::new(instr)));
                }

                block_value.push(ReactiveStatement::Terminal(ReactiveTerminalStatement {
                    terminal: ReactiveTerminal::ForIn {
                        init: init_block,
                        loop_block: loop_body,
                        id,
                        loc,
                    },
                    label: fallthrough_id.map(|ft| ReactiveLabel {
                        id: ft,
                        implicit: false,
                    }),
                }));
                if let Some(ft) = fallthrough_id {
                    self.visit_block(ft, block_value);
                }
            }

            Terminal::Branch {
                test,
                consequent,
                alternate,
                id,
                loc,
                ..
            } => {
                let consequent_block = if self.cx.is_scheduled(consequent) {
                    let break_ = self.visit_break(consequent, id, &loc);
                    break_.map(|b| vec![b])
                } else {
                    Some(self.traverse_block(consequent))
                };

                if self.cx.is_scheduled(alternate) {
                    panic!("Unexpected 'branch' where the alternate is already scheduled");
                }
                let alternate_block = self.traverse_block(alternate);

                block_value.push(ReactiveStatement::Terminal(ReactiveTerminalStatement {
                    terminal: ReactiveTerminal::If {
                        test,
                        consequent: consequent_block.unwrap_or_default(),
                        alternate: Some(alternate_block),
                        id,
                        loc,
                    },
                    label: None,
                }));
            }

            Terminal::Label {
                block: label_block,
                fallthrough,
                id,
                loc,
            } => {
                let fallthrough_id =
                    if self.cx.reachable(fallthrough) && !self.cx.is_scheduled(fallthrough) {
                        Some(fallthrough)
                    } else {
                        None
                    };
                if let Some(ft) = fallthrough_id {
                    let sid = self.cx.schedule(ft, "if");
                    schedule_ids.push(sid);
                }

                let label_body = if self.cx.is_scheduled(label_block) {
                    panic!("Unexpected 'label' where the block is already scheduled");
                } else {
                    self.traverse_block(label_block)
                };

                self.cx.unschedule_all(&schedule_ids);
                block_value.push(ReactiveStatement::Terminal(ReactiveTerminalStatement {
                    terminal: ReactiveTerminal::Label {
                        block: label_body,
                        id,
                        loc,
                    },
                    label: fallthrough_id.map(|ft| ReactiveLabel {
                        id: ft,
                        implicit: false,
                    }),
                }));
                if let Some(ft) = fallthrough_id {
                    self.visit_block(ft, block_value);
                }
            }

            Terminal::Sequence {
                block: _seq_block,
                fallthrough,
                id: terminal_id,
                loc: ref terminal_loc,
            } => {
                let loc = terminal_loc.clone();
                let fallthrough_id = if !self.cx.is_scheduled(fallthrough) {
                    Some(fallthrough)
                } else {
                    None
                };
                if let Some(ft) = fallthrough_id {
                    let sid = self.cx.schedule(ft, "if");
                    schedule_ids.push(sid);
                }

                let vbt_result = self.visit_value_block_terminal_owned(terminal, block_id);

                self.cx.unschedule_all(&schedule_ids);

                // Emit flat instructions for reactive scope analysis.
                for instr in &vbt_result.instructions {
                    block_value.push(ReactiveStatement::Instruction(Box::new(instr.clone())));
                }

                // For multi-instruction sequences at statement level, also
                // emit a ReactiveSequenceExpression so codegen can
                // reconstruct the comma expression — but ONLY when the
                // flat codegen would lose side effects (orphaned
                // side-effecting temp instructions).
                if vbt_result.instructions.len() > 1 {
                    let mut prefix = vbt_result.instructions;
                    let last = prefix.pop().unwrap();

                    // Check if any prefix instruction is a side-effecting
                    // temp whose result is never referenced by another
                    // instruction.  Such temps are "orphaned" and would
                    // be silently dropped by the flat codegen.
                    let has_orphaned = prefix.iter().any(|instr| {
                        if let Some(lv) = &instr.lvalue
                            && lv.identifier.name.is_none()
                            && matches!(
                                instr.value,
                                InstructionValue::CallExpression { .. }
                                    | InstructionValue::MethodCall { .. }
                                    | InstructionValue::PostfixUpdate { .. }
                                    | InstructionValue::PrefixUpdate { .. }
                                    | InstructionValue::StoreLocal { .. }
                            )
                        {
                            let others: Vec<_> = prefix
                                .iter()
                                .chain(std::iter::once(&last))
                                .filter(|i| !std::ptr::eq(*i, instr))
                                .cloned()
                                .collect();
                            !is_decl_id_referenced_in_instructions(
                                lv.identifier.declaration_id,
                                &others,
                            )
                        } else {
                            false
                        }
                    });

                    if has_orphaned {
                        let seq_value = InstructionValue::ReactiveSequenceExpression {
                            instructions: prefix,
                            id: last.id,
                            value: Box::new(last.value),
                            loc: loc.clone(),
                        };
                        block_value.push(ReactiveStatement::Instruction(Box::new(
                            ReactiveInstruction {
                                id: terminal_id,
                                lvalue: last.lvalue,
                                value: seq_value,
                                loc,
                            },
                        )));
                    }
                }

                if let Some(ft) = fallthrough_id {
                    self.visit_block(ft, block_value);
                }
            }

            terminal @ (Terminal::Optional { .. }
            | Terminal::Ternary { .. }
            | Terminal::Logical { .. }) => {
                let fallthrough = match &terminal {
                    Terminal::Optional { fallthrough, .. }
                    | Terminal::Ternary { fallthrough, .. }
                    | Terminal::Logical { fallthrough, .. } => *fallthrough,
                    _ => unreachable!(),
                };

                let fallthrough_id = if !self.cx.is_scheduled(fallthrough) {
                    Some(fallthrough)
                } else {
                    None
                };
                if let Some(ft) = fallthrough_id {
                    let sid = self.cx.schedule(ft, "if");
                    schedule_ids.push(sid);
                }

                let vbt_result = self.visit_value_block_terminal_owned(terminal, block_id);

                self.cx.unschedule_all(&schedule_ids);

                for instr in vbt_result.instructions {
                    block_value.push(ReactiveStatement::Instruction(Box::new(instr)));
                }
                if let Some(instr) = vbt_result.final_instruction {
                    block_value.push(ReactiveStatement::Instruction(Box::new(instr)));
                }

                if let Some(ft) = fallthrough_id {
                    self.visit_block(ft, block_value);
                }
            }

            Terminal::Goto {
                block: goto_block,
                variant,
                id,
                loc,
            } => {
                match variant {
                    GotoVariant::Break => {
                        if let Some(break_stmt) = self.visit_break(goto_block, id, &loc) {
                            block_value.push(break_stmt);
                        } else if !self.cx.is_scheduled(goto_block)
                            && !self.cx.emitted.contains(&goto_block)
                        {
                            // Fallback for unresolved break targets: emit target block inline once.
                            // This preserves control flow instead of dropping a terminal entirely.
                            self.visit_block(goto_block, block_value);
                        }
                    }
                    GotoVariant::Continue => {
                        if let Some(continue_stmt) = self.visit_continue(goto_block, id, &loc) {
                            block_value.push(continue_stmt);
                        }
                    }
                    GotoVariant::Try => {
                        // noop
                    }
                }
            }

            Terminal::MaybeThrow { continuation, .. } => {
                // ReactiveFunction does not explicitly model maybe-throw semantics,
                // so these terminals flatten away.
                if !self.cx.is_scheduled(continuation) {
                    self.visit_block(continuation, block_value);
                }
            }

            Terminal::Try {
                block: try_block,
                handler_binding,
                handler,
                fallthrough,
                id,
                loc,
            } => {
                let fallthrough_id =
                    if self.cx.reachable(fallthrough) && !self.cx.is_scheduled(fallthrough) {
                        Some(fallthrough)
                    } else {
                        None
                    };
                if let Some(ft) = fallthrough_id {
                    let sid = self.cx.schedule(ft, "if");
                    schedule_ids.push(sid);
                }
                self.cx.schedule_catch_handler(handler);

                let try_body = self.traverse_block(try_block);
                let handler_body = self.traverse_block(handler);

                self.cx.unschedule_all(&schedule_ids);
                block_value.push(ReactiveStatement::Terminal(ReactiveTerminalStatement {
                    terminal: ReactiveTerminal::Try {
                        block: try_body,
                        handler_binding,
                        handler: handler_body,
                        id,
                        loc,
                    },
                    label: fallthrough_id.map(|ft| ReactiveLabel {
                        id: ft,
                        implicit: false,
                    }),
                }));
                if let Some(ft) = fallthrough_id {
                    self.visit_block(ft, block_value);
                }
            }

            Terminal::Scope {
                block: scope_block,
                fallthrough,
                scope,
                id: _,
                loc: _,
            } => {
                let fallthrough_id = if !self.cx.is_scheduled(fallthrough) {
                    Some(fallthrough)
                } else {
                    None
                };
                if let Some(ft) = fallthrough_id {
                    let sid = self.cx.schedule(ft, "if");
                    schedule_ids.push(sid);
                    self.cx.scope_fallthroughs.insert(ft);
                }

                if self.cx.is_scheduled(scope_block) {
                    panic!("Unexpected 'scope' where the block is already scheduled");
                }
                let body = self.traverse_block(scope_block);

                self.cx.unschedule_all(&schedule_ids);
                block_value.push(ReactiveStatement::Scope(ReactiveScopeBlock {
                    scope,
                    instructions: body,
                }));
                if let Some(ft) = fallthrough_id {
                    self.visit_block(ft, block_value);
                }
            }

            Terminal::PrunedScope {
                block: scope_block,
                fallthrough,
                scope,
                id: _,
                loc: _,
            } => {
                let fallthrough_id = if !self.cx.is_scheduled(fallthrough) {
                    Some(fallthrough)
                } else {
                    None
                };
                if let Some(ft) = fallthrough_id {
                    let sid = self.cx.schedule(ft, "if");
                    schedule_ids.push(sid);
                    self.cx.scope_fallthroughs.insert(ft);
                }

                if self.cx.is_scheduled(scope_block) {
                    panic!("Unexpected 'scope' where the block is already scheduled");
                }
                let body = self.traverse_block(scope_block);

                self.cx.unschedule_all(&schedule_ids);
                block_value.push(ReactiveStatement::PrunedScope(PrunedReactiveScopeBlock {
                    scope,
                    instructions: body,
                }));
                if let Some(ft) = fallthrough_id {
                    self.visit_block(ft, block_value);
                }
            }

            Terminal::Unreachable { .. } => {
                // noop
            }

            Terminal::Unsupported { .. } => {
                panic!("Unexpected unsupported terminal");
            }
        }
    }

    /// Visit a "value block" — a block whose purpose is to compute a value.
    /// Returns the instructions needed and the Place holding the result.
    ///
    /// This corresponds to upstream `visitValueBlock`. Value blocks typically
    /// end in a `Branch` terminal (the value is used as the branch test) or
    /// a `Goto` terminal (the value is in the last instruction).
    fn visit_value_block(&mut self, block_id: BlockId, _loc: &SourceLocation) -> ValueBlockResult {
        // Remove the block to take ownership.
        let Some(block) = self.cx.blocks.remove(&block_id) else {
            // Block was already consumed (e.g., by a prior visit_block).
            // Return an empty value block result.
            let place = Place {
                identifier: Identifier {
                    id: crate::hir::types::make_identifier_id(0),
                    declaration_id: DeclarationId::new(0),
                    name: None,
                    mutable_range: crate::hir::types::MutableRange::default(),
                    scope: None,
                    type_: crate::hir::types::Type::Primitive,
                    loc: SourceLocation::default(),
                },
                effect: Effect::Unknown,
                reactive: false,
                loc: SourceLocation::default(),
            };
            return ValueBlockResult {
                block_id,
                instructions: vec![],
                value: InstructionValue::LoadLocal {
                    place: place.clone(),
                    loc: place.loc.clone(),
                },
                place,
                id: InstructionId::new(0),
                branch_targets: None,
            };
        };

        // Capture the terminal ID before destructuring.
        let terminal_id = block.terminal.id();

        match block.terminal {
            Terminal::Branch {
                test: branch_test,
                consequent,
                alternate,
                ..
            } => {
                let instructions = block.instructions;
                if instructions.is_empty() {
                    ValueBlockResult {
                        block_id,
                        instructions: Vec::new(),
                        value: InstructionValue::LoadLocal {
                            place: branch_test.clone(),
                            loc: branch_test.loc.clone(),
                        },
                        place: branch_test,
                        id: terminal_id,
                        branch_targets: Some((consequent, alternate)),
                    }
                } else if instructions.len() == 1 {
                    let instr = instructions.into_iter().next().unwrap();
                    assert_eq!(
                        instr.lvalue.identifier.id, branch_test.identifier.id,
                        "Expected branch block to end in an instruction that sets the test value"
                    );
                    ValueBlockResult {
                        block_id,
                        value: instr.value.clone(),
                        instructions: vec![ReactiveInstruction {
                            id: instr.id,
                            lvalue: Some(instr.lvalue),
                            value: instr.value,
                            loc: instr.loc,
                        }],
                        place: branch_test,
                        id: terminal_id,
                        branch_targets: Some((consequent, alternate)),
                    }
                } else {
                    // Multiple instructions — emit all.
                    let reactive_instrs: Vec<ReactiveInstruction> = instructions
                        .into_iter()
                        .map(|i| ReactiveInstruction {
                            id: i.id,
                            lvalue: Some(i.lvalue),
                            value: i.value,
                            loc: i.loc,
                        })
                        .collect();
                    ValueBlockResult {
                        block_id,
                        instructions: reactive_instrs,
                        value: InstructionValue::LoadLocal {
                            place: branch_test.clone(),
                            loc: branch_test.loc.clone(),
                        },
                        place: branch_test,
                        id: terminal_id,
                        branch_targets: Some((consequent, alternate)),
                    }
                }
            }

            Terminal::Goto { .. } => {
                let mut instructions: Vec<_> = block.instructions.into_iter().collect();
                if instructions.is_empty() {
                    // Empty goto value block — return a dummy undefined value
                    let place = Place {
                        identifier: Identifier {
                            id: crate::hir::types::make_identifier_id(0),
                            declaration_id: DeclarationId::new(0),
                            name: None,
                            mutable_range: crate::hir::types::MutableRange::default(),
                            scope: None,
                            type_: crate::hir::types::Type::Primitive,
                            loc: SourceLocation::default(),
                        },
                        effect: Effect::Unknown,
                        reactive: false,
                        loc: SourceLocation::default(),
                    };
                    return ValueBlockResult {
                        block_id,
                        instructions: vec![],
                        value: InstructionValue::LoadLocal {
                            place: place.clone(),
                            loc: place.loc.clone(),
                        },
                        place,
                        id: InstructionId::new(0),
                        branch_targets: None,
                    };
                }

                let last = instructions.pop().unwrap();
                let mut place = last.lvalue;
                let mut value = last.value;
                let last_id = last.id;
                let last_loc = last.loc;

                // Check if the last instruction is a StoreLocal for a temporary.
                // If so, unwrap it (the store is only needed in the CFG for phi nodes).
                if let crate::hir::types::InstructionValue::StoreLocal {
                    lvalue: ref store_lvalue,
                    value: ref store_value,
                    ..
                } = value
                    && store_lvalue.place.identifier.name.is_none()
                {
                    place = store_lvalue.place.clone();
                    value = crate::hir::types::InstructionValue::LoadLocal {
                        place: store_value.clone(),
                        loc: store_value.loc.clone(),
                    };
                }

                let mut reactive_instrs: Vec<ReactiveInstruction> = instructions
                    .into_iter()
                    .map(|i| ReactiveInstruction {
                        id: i.id,
                        lvalue: Some(i.lvalue),
                        value: i.value,
                        loc: i.loc,
                    })
                    .collect();
                let final_instruction = ReactiveInstruction {
                    id: last_id,
                    lvalue: Some(place.clone()),
                    value: value.clone(),
                    loc: last_loc,
                };
                reactive_instrs.push(final_instruction);
                ValueBlockResult {
                    block_id,
                    instructions: reactive_instrs,
                    value: value.clone(),
                    place,
                    id: last_id,
                    branch_targets: None,
                }
            }

            Terminal::Return { value, id, .. } => {
                // A Return in a value block means the code path returns early.
                // Collect the block's instructions and use the return value as the place.
                let reactive_instrs: Vec<ReactiveInstruction> = block
                    .instructions
                    .into_iter()
                    .map(|i| ReactiveInstruction {
                        id: i.id,
                        lvalue: Some(i.lvalue),
                        value: i.value,
                        loc: i.loc,
                    })
                    .collect();
                ValueBlockResult {
                    block_id,
                    instructions: reactive_instrs,
                    value: InstructionValue::LoadLocal {
                        place: value.clone(),
                        loc: value.loc.clone(),
                    },
                    place: value,
                    id,
                    branch_targets: None,
                }
            }

            Terminal::Throw { value, id, .. } => {
                // Similar to Return: collect instructions, use the throw value as the place.
                let reactive_instrs: Vec<ReactiveInstruction> = block
                    .instructions
                    .into_iter()
                    .map(|i| ReactiveInstruction {
                        id: i.id,
                        lvalue: Some(i.lvalue),
                        value: i.value,
                        loc: i.loc,
                    })
                    .collect();
                ValueBlockResult {
                    block_id,
                    instructions: reactive_instrs,
                    value: InstructionValue::LoadLocal {
                        place: value.clone(),
                        loc: value.loc.clone(),
                    },
                    place: value,
                    id,
                    branch_targets: None,
                }
            }

            Terminal::Unreachable { id, .. } => {
                // Unreachable in a value block — collect instructions, return a dummy place.
                let reactive_instrs: Vec<ReactiveInstruction> = block
                    .instructions
                    .into_iter()
                    .map(|i| ReactiveInstruction {
                        id: i.id,
                        lvalue: Some(i.lvalue),
                        value: i.value,
                        loc: i.loc,
                    })
                    .collect();
                let place = Place {
                    identifier: Identifier {
                        id: crate::hir::types::make_identifier_id(0),
                        declaration_id: DeclarationId::new(0),
                        name: None,
                        mutable_range: crate::hir::types::MutableRange::default(),
                        scope: None,
                        type_: crate::hir::types::Type::Primitive,
                        loc: SourceLocation::default(),
                    },
                    effect: Effect::Unknown,
                    reactive: false,
                    loc: SourceLocation::default(),
                };
                ValueBlockResult {
                    block_id,
                    instructions: reactive_instrs,
                    value: InstructionValue::LoadLocal {
                        place: place.clone(),
                        loc: place.loc.clone(),
                    },
                    place,
                    id,
                    branch_targets: None,
                }
            }

            other => {
                let other_instructions = block.instructions;

                let vbt = self.visit_value_block_terminal_owned(other, block_id);
                let final_result = self.visit_value_block(vbt.fallthrough, _loc);

                let mut all_instrs: Vec<ReactiveInstruction> = other_instructions
                    .into_iter()
                    .map(|i| ReactiveInstruction {
                        id: i.id,
                        lvalue: Some(i.lvalue),
                        value: i.value,
                        loc: i.loc,
                    })
                    .collect();
                all_instrs.extend(vbt.instructions);
                if let Some(instr) = vbt.final_instruction {
                    all_instrs.push(instr);
                }
                all_instrs.extend(final_result.instructions.clone());

                ValueBlockResult {
                    block_id: vbt.fallthrough,
                    instructions: all_instrs,
                    value: final_result.value.clone(),
                    place: final_result.place,
                    id: final_result.id,
                    branch_targets: final_result.branch_targets,
                }
            }
        }
    }

    /// Visit a value block terminal from an owned Terminal (used in visit_value_block
    /// when the block's terminal is a value terminal like sequence/logical/ternary/optional).
    fn visit_value_block_terminal_owned(
        &mut self,
        terminal: Terminal,
        _block_id: BlockId,
    ) -> ValueBlockTerminalResult {
        match terminal {
            Terminal::Sequence {
                block: seq_block,
                fallthrough,
                id: _,
                loc,
            } => {
                let result = self.visit_value_block(seq_block, &loc);
                ValueBlockTerminalResult {
                    instructions: result.instructions,
                    final_instruction: None,
                    fallthrough,
                }
            }
            Terminal::Optional {
                test: opt_test,
                fallthrough,
                id: _,
                loc,
                optional: _,
            } => {
                let debug_optional = std::env::var("DEBUG_CODEGEN_EXPR").is_ok()
                    || std::env::var("DEBUG_REACTIVE_OPTIONAL").is_ok();
                let test = self.visit_value_block(opt_test, &loc);
                let consequent_block = test.branch_targets.map(|(consequent, _)| consequent);
                if debug_optional {
                    eprintln!(
                        "[REACTIVE_OPTIONAL] test_block=bb{} test_place=id{} test_instrs={}",
                        test.block_id.0,
                        test.place.identifier.id.0,
                        test.instructions.len()
                    );
                }
                if let Some(cons_id) = consequent_block {
                    let consequent_result = self.visit_value_block(cons_id, &loc);
                    if debug_optional {
                        eprintln!(
                            "[REACTIVE_OPTIONAL] consequent_block=bb{} place=id{} result_instrs={}",
                            cons_id.0,
                            consequent_result.place.identifier.id.0,
                            consequent_result.instructions.len()
                        );
                        for (idx, instr) in consequent_result.instructions.iter().enumerate() {
                            let lvalue_id = instr.lvalue.as_ref().map(|lv| lv.identifier.id.0);
                            eprintln!(
                                "[REACTIVE_OPTIONAL]   instr[{idx}] lvalue={lvalue_id:?} value={:?}",
                                std::mem::discriminant(&instr.value)
                            );
                        }
                    }

                    enum OptionalRewriteMode {
                        ReplaceWithSingle(Box<ReactiveInstruction>),
                        KeepConsequent,
                    }

                    let mut rewrite_mode: Option<OptionalRewriteMode> = None;
                    let mut consequent_instructions = consequent_result.instructions;
                    let load_local_id =
                        consequent_instructions
                            .last()
                            .and_then(|instr| match &instr.value {
                                InstructionValue::LoadLocal { place, .. } => {
                                    Some(place.identifier.id)
                                }
                                _ => None,
                            });
                    if let Some(load_local_id) = load_local_id {
                        for instr in &mut consequent_instructions {
                            if instr.lvalue.as_ref().map(|lv| lv.identifier.id)
                                != Some(load_local_id)
                            {
                                continue;
                            }
                            match &mut instr.value {
                                InstructionValue::PropertyLoad {
                                    object, property, ..
                                } if object.identifier.id == test.place.identifier.id => {
                                    if debug_optional {
                                        eprintln!(
                                            "[REACTIVE_OPTIONAL] rewrite property object=id{} -> optional .{:?}",
                                            object.identifier.id.0, property
                                        );
                                    }
                                    rewrite_mode = Some(OptionalRewriteMode::ReplaceWithSingle(
                                        Box::new(ReactiveInstruction {
                                            id: consequent_result.id,
                                            lvalue: Some(consequent_result.place.clone()),
                                            value: InstructionValue::PropertyLoad {
                                                object: object.clone(),
                                                property: property.clone(),
                                                optional: true,
                                                loc: loc.clone(),
                                            },
                                            loc: loc.clone(),
                                        }),
                                    ));
                                }
                                InstructionValue::ComputedLoad {
                                    object, optional, ..
                                } if object.identifier.id == test.place.identifier.id => {
                                    if debug_optional {
                                        eprintln!(
                                            "[REACTIVE_OPTIONAL] rewrite computed object=id{} keep_consequent=true",
                                            object.identifier.id.0
                                        );
                                    }
                                    *optional = true;
                                    rewrite_mode = Some(OptionalRewriteMode::KeepConsequent);
                                }
                                _ => {}
                            }
                            if rewrite_mode.is_some() {
                                break;
                            }
                        }
                    }

                    let mut all_instrs = test.instructions;
                    match rewrite_mode {
                        Some(OptionalRewriteMode::ReplaceWithSingle(rewrite)) => {
                            if debug_optional {
                                eprintln!("[REACTIVE_OPTIONAL] rewrite_applied=true");
                            }
                            all_instrs.push(*rewrite);
                            return ValueBlockTerminalResult {
                                instructions: all_instrs,
                                final_instruction: None,
                                fallthrough,
                            };
                        }
                        Some(OptionalRewriteMode::KeepConsequent) => {
                            if debug_optional {
                                eprintln!(
                                    "[REACTIVE_OPTIONAL] rewrite_applied=true keep_consequent=true"
                                );
                            }
                            all_instrs.extend(consequent_instructions);
                        }
                        None => {
                            if debug_optional {
                                eprintln!("[REACTIVE_OPTIONAL] rewrite_applied=false");
                            }
                            all_instrs.extend(consequent_instructions);
                        }
                    }
                    return ValueBlockTerminalResult {
                        instructions: all_instrs,
                        final_instruction: None,
                        fallthrough,
                    };
                }
                if debug_optional {
                    eprintln!(
                        "[REACTIVE_OPTIONAL] missing_branch_targets test_block=bb{}",
                        test.block_id.0
                    );
                }
                ValueBlockTerminalResult {
                    instructions: test.instructions,
                    final_instruction: None,
                    fallthrough,
                }
            }
            Terminal::Logical {
                test: log_test,
                fallthrough,
                id,
                loc,
                operator,
            } => {
                let test = self.visit_value_block(log_test, &loc);
                if let Some((cons_id, alt_id)) = test.branch_targets {
                    let left = self.visit_value_block(cons_id, &loc);
                    let right = self.visit_value_block(alt_id, &loc);
                    let left_place = Self::effective_value_place(&left);
                    let right_place = Self::effective_value_place(&right);
                    let mut all_instrs = test.instructions;
                    all_instrs.extend(left.instructions);
                    all_instrs.extend(right.instructions);
                    let final_instruction = ReactiveInstruction {
                        id,
                        lvalue: Some(left.place.clone()),
                        value: InstructionValue::LogicalExpression {
                            operator,
                            left: left_place,
                            right: right_place,
                            loc: loc.clone(),
                        },
                        loc: loc.clone(),
                    };
                    return ValueBlockTerminalResult {
                        instructions: all_instrs,
                        final_instruction: Some(final_instruction),
                        fallthrough,
                    };
                }
                ValueBlockTerminalResult {
                    instructions: test.instructions,
                    final_instruction: None,
                    fallthrough,
                }
            }
            Terminal::Ternary {
                test: tern_test,
                fallthrough,
                id,
                loc,
            } => {
                let test = self.visit_value_block(tern_test, &loc);
                if let Some((cons_id, alt_id)) = test.branch_targets {
                    let consequent_result = self.visit_value_block(cons_id, &loc);
                    let alternate_result = self.visit_value_block(alt_id, &loc);
                    let consequent_place = Self::effective_value_place(&consequent_result);
                    let alternate_place = Self::effective_value_place(&alternate_result);
                    let mut all_instrs = test.instructions;
                    all_instrs.extend(consequent_result.instructions);
                    all_instrs.extend(alternate_result.instructions);
                    let final_instruction = ReactiveInstruction {
                        id,
                        lvalue: Some(consequent_result.place.clone()),
                        value: InstructionValue::Ternary {
                            test: test.place,
                            consequent: consequent_place,
                            alternate: alternate_place,
                            loc: loc.clone(),
                        },
                        loc: loc.clone(),
                    };
                    return ValueBlockTerminalResult {
                        instructions: all_instrs,
                        final_instruction: Some(final_instruction),
                        fallthrough,
                    };
                }
                ValueBlockTerminalResult {
                    instructions: test.instructions,
                    final_instruction: None,
                    fallthrough,
                }
            }
            Terminal::MaybeThrow { .. } => {
                // Upstream throws a TODO: "Support value blocks (conditional, logical,
                // optional chaining, etc) within a try/catch statement"
                panic!(
                    "Value blocks within try/catch (MaybeThrow in value block) not yet supported"
                );
            }
            Terminal::Scope {
                block: scope_block,
                fallthrough,
                id: _,
                loc,
                ..
            } => {
                // Scope terminals in value blocks: process the scope body as a
                // value block, flattening the scope boundary. The scope's
                // memoization hints don't affect the value computation.
                let result = self.visit_value_block(scope_block, &loc);
                ValueBlockTerminalResult {
                    instructions: result.instructions,
                    final_instruction: None,
                    fallthrough,
                }
            }
            Terminal::PrunedScope {
                block: scope_block,
                fallthrough,
                id: _,
                loc,
                ..
            } => {
                // PrunedScope terminals in value blocks: same as Scope —
                // flatten and process the body as a value block.
                let result = self.visit_value_block(scope_block, &loc);
                ValueBlockTerminalResult {
                    instructions: result.instructions,
                    final_instruction: None,
                    fallthrough,
                }
            }
            Terminal::Label {
                block: label_block,
                fallthrough,
                id: _,
                loc,
            } => {
                // Label terminals in value blocks: process the label body as a
                // value block, flattening the label boundary.
                let result = self.visit_value_block(label_block, &loc);
                ValueBlockTerminalResult {
                    instructions: result.instructions,
                    final_instruction: None,
                    fallthrough,
                }
            }
            Terminal::If {
                consequent,
                alternate,
                fallthrough,
                id: _,
                loc,
                ..
            } => {
                // If terminal in a value block: visit both branches as value blocks.
                // This can happen when our HIR lowering produces If instead of Ternary.
                let cons_result = self.visit_value_block(consequent, &loc);
                let alt_result = self.visit_value_block(alternate, &loc);
                let mut all_instrs = cons_result.instructions;
                all_instrs.extend(alt_result.instructions);
                ValueBlockTerminalResult {
                    instructions: all_instrs,
                    final_instruction: None,
                    fallthrough,
                }
            }
            Terminal::Goto {
                block: goto_block,
                id: _,
                loc,
                ..
            } => {
                // Goto in a value block: follow through to target block.
                let result = self.visit_value_block(goto_block, &loc);
                ValueBlockTerminalResult {
                    instructions: result.instructions,
                    final_instruction: None,
                    fallthrough: goto_block,
                }
            }
            other => {
                panic!(
                    "Unsupported terminal kind {:?} in visit_value_block_terminal_owned",
                    std::mem::discriminant(&other)
                );
            }
        }
    }

    /// Emit a break statement for a goto that targets a scheduled block.
    fn visit_break(
        &self,
        block: BlockId,
        id: InstructionId,
        loc: &SourceLocation,
    ) -> Option<ReactiveStatement> {
        if std::env::var("DEBUG_BREAK_TARGET").is_ok() {
            eprintln!(
                "[VISIT_BREAK] goto-break target=bb{} id={} stack={}",
                block.0,
                id.0,
                self.cx.debug_control_flow_stack()
            );
        }
        let resolved_target = self.cx.get_break_target(block);

        let (target_block, target_kind) = if let Some(target) = resolved_target {
            target
        } else if let Some(ControlFlowTarget::If { block, .. }) = self.cx.control_flow_stack.last()
        {
            // For unresolved breaks nested in conditional flow, preserve the
            // enclosing conditional boundary with a labeled break.
            (*block, ReactiveTerminalTargetKind::Labeled)
        } else {
            // For unresolved loop/switch/case exits, let the caller inline the
            // target block when it is safe to do so.
            return None;
        };

        if std::env::var("DEBUG_BREAK_TARGET").is_ok() {
            eprintln!(
                "[VISIT_BREAK] emit break target=bb{} kind={:?} stack={}",
                target_block.0,
                target_kind,
                self.cx.debug_control_flow_stack()
            );
        }

        // If the target is a scope fallthrough, elide the break.
        // Upstream expects implicit target_kind here, but our HIR may not match exactly.
        if self.cx.scope_fallthroughs.contains(&target_block) {
            return None;
        }

        Some(ReactiveStatement::Terminal(ReactiveTerminalStatement {
            terminal: ReactiveTerminal::Break {
                target: target_block,
                target_kind,
                id,
                loc: loc.clone(),
            },
            label: None,
        }))
    }

    /// Emit a continue statement for a goto that targets a scheduled continue block.
    fn visit_continue(
        &self,
        block: BlockId,
        id: InstructionId,
        loc: &SourceLocation,
    ) -> Option<ReactiveStatement> {
        let Some((target_block, target_kind)) = self.cx.get_continue_target(block) else {
            // Continue target not found — skip
            return None;
        };

        Some(ReactiveStatement::Terminal(ReactiveTerminalStatement {
            terminal: ReactiveTerminal::Continue {
                target: target_block,
                target_kind,
                id,
                loc: loc.clone(),
            },
            label: None,
        }))
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Convert an HIR function (CFG) into a ReactiveFunction (tree).
///
/// This consumes the HIRFunction because it moves instructions out of the
/// CFG blocks into the tree nodes.
pub fn build_reactive_function(func: HIRFunction) -> ReactiveFunction {
    let entry = func.body.entry;

    // Build a HashMap from the blocks Vec for O(1) lookup.
    let blocks: HashMap<BlockId, BasicBlock> = func.body.blocks.into_iter().collect();

    let cx = Context::new(blocks);
    let mut driver = Driver::new(cx);
    let body = driver.traverse_block(entry);

    ReactiveFunction {
        loc: func.loc,
        id: func.id,
        name_hint: None,
        params: func.params,
        generator: func.generator,
        async_: func.async_,
        body,
        directives: func.directives,
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Check if a given declaration ID is referenced as an operand in any of the
/// provided instructions' values. This uses a simple check of the most common
/// value variants (StoreLocal, StoreContext, LoadLocal, CallExpression, etc.)
/// to detect when a temp's result is consumed by a subsequent instruction.
fn is_decl_id_referenced_in_instructions(
    decl_id: crate::hir::types::DeclarationId,
    instructions: &[ReactiveInstruction],
) -> bool {
    fn place_matches(place: &Place, target: crate::hir::types::DeclarationId) -> bool {
        place.identifier.declaration_id == target
    }

    for instr in instructions {
        match &instr.value {
            InstructionValue::StoreLocal { value: v, .. }
            | InstructionValue::StoreContext { value: v, .. } => {
                if place_matches(v, decl_id) {
                    return true;
                }
            }
            InstructionValue::LoadLocal { place, .. }
            | InstructionValue::LoadContext { place, .. } => {
                if place_matches(place, decl_id) {
                    return true;
                }
            }
            InstructionValue::PropertyLoad { object, .. } => {
                if place_matches(object, decl_id) {
                    return true;
                }
            }
            InstructionValue::ComputedLoad {
                object, property, ..
            } => {
                if place_matches(object, decl_id) || place_matches(property, decl_id) {
                    return true;
                }
            }
            InstructionValue::CallExpression { callee, args, .. } => {
                if place_matches(callee, decl_id) {
                    return true;
                }
                for arg in args {
                    let p = match arg {
                        crate::hir::types::Argument::Place(p)
                        | crate::hir::types::Argument::Spread(p) => p,
                    };
                    if place_matches(p, decl_id) {
                        return true;
                    }
                }
            }
            InstructionValue::MethodCall { receiver, args, .. } => {
                if place_matches(receiver, decl_id) {
                    return true;
                }
                for arg in args {
                    let p = match arg {
                        crate::hir::types::Argument::Place(p)
                        | crate::hir::types::Argument::Spread(p) => p,
                    };
                    if place_matches(p, decl_id) {
                        return true;
                    }
                }
            }
            InstructionValue::BinaryExpression { left, right, .. } => {
                if place_matches(left, decl_id) || place_matches(right, decl_id) {
                    return true;
                }
            }
            InstructionValue::UnaryExpression { value: v, .. } => {
                if place_matches(v, decl_id) {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hir::types::*;

    /// Helper to create a minimal Place for testing.
    fn make_place(id: u32) -> Place {
        Place {
            identifier: Identifier {
                id: IdentifierId(id),
                declaration_id: DeclarationId(id),
                name: None,
                mutable_range: MutableRange::default(),
                scope: None,
                type_: Type::Poly,
                loc: SourceLocation::Generated,
            },
            effect: Effect::Unknown,
            reactive: false,
            loc: SourceLocation::Generated,
        }
    }

    /// Helper to create a Primitive instruction.
    fn make_primitive(instr_id: u32, place_id: u32, val: PrimitiveValue) -> Instruction {
        Instruction {
            id: InstructionId(instr_id),
            lvalue: make_place(place_id),
            value: InstructionValue::Primitive {
                value: val,
                loc: SourceLocation::Generated,
            },
            loc: SourceLocation::Generated,
            effects: None,
        }
    }

    #[test]
    fn test_simple_return() {
        // A single block that just returns a value.
        //   bb0: instructions=[LoadLocal], terminal=Return
        let func = HIRFunction {
            env: crate::environment::Environment::new(crate::options::EnvironmentConfig::default()),
            loc: SourceLocation::Generated,
            id: Some("test".to_string()),
            fn_type: ReactFunctionType::Component,
            params: vec![],
            returns: make_place(0),
            context: vec![],
            body: HIR {
                entry: BlockId(0),
                blocks: vec![(
                    BlockId(0),
                    BasicBlock {
                        kind: BlockKind::Block,
                        id: BlockId(0),
                        instructions: vec![make_primitive(1, 10, PrimitiveValue::Number(42.0))],
                        terminal: Terminal::Return {
                            value: make_place(10),
                            return_variant: ReturnVariant::Explicit,
                            id: InstructionId(2),
                            loc: SourceLocation::Generated,
                        },
                        preds: std::collections::HashSet::new(),
                        phis: vec![],
                    },
                )],
            },
            generator: false,
            async_: false,
            directives: vec![],
            aliasing_effects: None,
        };

        let reactive = build_reactive_function(func);
        assert_eq!(reactive.body.len(), 2); // 1 instruction + 1 return terminal

        // First statement should be the instruction.
        assert!(matches!(
            &reactive.body[0],
            ReactiveStatement::Instruction(_)
        ));

        // Second statement should be the return terminal.
        assert!(matches!(
            &reactive.body[1],
            ReactiveStatement::Terminal(ReactiveTerminalStatement {
                terminal: ReactiveTerminal::Return { .. },
                label: None,
            })
        ));
    }

    #[test]
    fn test_if_then_else() {
        // bb0: instructions=[], terminal=If(test, consequent=bb1, alternate=bb2, fallthrough=bb3)
        // bb1: instructions=[Primitive(true)], terminal=Goto(bb3, Break)
        // bb2: instructions=[Primitive(false)], terminal=Goto(bb3, Break)
        // bb3: instructions=[], terminal=Return

        let mut preds_bb3 = std::collections::HashSet::new();
        preds_bb3.insert(BlockId(1));
        preds_bb3.insert(BlockId(2));

        let func = HIRFunction {
            env: crate::environment::Environment::new(crate::options::EnvironmentConfig::default()),
            loc: SourceLocation::Generated,
            id: Some("test_if".to_string()),
            fn_type: ReactFunctionType::Component,
            params: vec![],
            returns: make_place(0),
            context: vec![],
            body: HIR {
                entry: BlockId(0),
                blocks: vec![
                    (
                        BlockId(0),
                        BasicBlock {
                            kind: BlockKind::Block,
                            id: BlockId(0),
                            instructions: vec![],
                            terminal: Terminal::If {
                                test: make_place(1),
                                consequent: BlockId(1),
                                alternate: BlockId(2),
                                fallthrough: BlockId(3),
                                id: InstructionId(10),
                                loc: SourceLocation::Generated,
                            },
                            preds: std::collections::HashSet::new(),
                            phis: vec![],
                        },
                    ),
                    (
                        BlockId(1),
                        BasicBlock {
                            kind: BlockKind::Block,
                            id: BlockId(1),
                            instructions: vec![make_primitive(
                                11,
                                20,
                                PrimitiveValue::Boolean(true),
                            )],
                            terminal: Terminal::Goto {
                                block: BlockId(3),
                                variant: GotoVariant::Break,
                                id: InstructionId(12),
                                loc: SourceLocation::Generated,
                            },
                            preds: std::collections::HashSet::new(),
                            phis: vec![],
                        },
                    ),
                    (
                        BlockId(2),
                        BasicBlock {
                            kind: BlockKind::Block,
                            id: BlockId(2),
                            instructions: vec![make_primitive(
                                13,
                                21,
                                PrimitiveValue::Boolean(false),
                            )],
                            terminal: Terminal::Goto {
                                block: BlockId(3),
                                variant: GotoVariant::Break,
                                id: InstructionId(14),
                                loc: SourceLocation::Generated,
                            },
                            preds: preds_bb3.clone(),
                            phis: vec![],
                        },
                    ),
                    (
                        BlockId(3),
                        BasicBlock {
                            kind: BlockKind::Block,
                            id: BlockId(3),
                            instructions: vec![],
                            terminal: Terminal::Return {
                                value: make_place(99),
                                return_variant: ReturnVariant::Explicit,
                                id: InstructionId(20),
                                loc: SourceLocation::Generated,
                            },
                            preds: preds_bb3,
                            phis: vec![],
                        },
                    ),
                ],
            },
            generator: false,
            async_: false,
            directives: vec![],
            aliasing_effects: None,
        };

        let reactive = build_reactive_function(func);

        // Should have: If terminal + Return terminal (fallthrough)
        assert_eq!(reactive.body.len(), 2);

        // First is the If terminal.
        match &reactive.body[0] {
            ReactiveStatement::Terminal(term_stmt) => {
                assert!(matches!(&term_stmt.terminal, ReactiveTerminal::If { .. }));
                // The fallthrough label should point to bb3.
                assert!(term_stmt.label.is_some());
                assert_eq!(term_stmt.label.as_ref().unwrap().id, BlockId(3));
            }
            _ => panic!("Expected terminal statement"),
        }

        // Second is the Return from bb3.
        assert!(matches!(
            &reactive.body[1],
            ReactiveStatement::Terminal(ReactiveTerminalStatement {
                terminal: ReactiveTerminal::Return { .. },
                ..
            })
        ));
    }

    #[test]
    fn test_scope_with_fallthrough() {
        // bb0: instructions=[Primitive(1)], terminal=Scope(block=bb1, fallthrough=bb2, scope=...)
        // bb1: instructions=[Primitive(2)], terminal=Goto(bb2, Break)
        // bb2: instructions=[], terminal=Return

        let scope = ReactiveScope {
            id: ScopeId(0),
            range: MutableRange {
                start: InstructionId(0),
                end: InstructionId(10),
            },
            dependencies: vec![],
            declarations: indexmap::IndexMap::new(),
            reassignments: vec![],
            merged_id: None,
            early_return_value: None,
        };

        let func = HIRFunction {
            env: crate::environment::Environment::new(crate::options::EnvironmentConfig::default()),
            loc: SourceLocation::Generated,
            id: Some("test_scope".to_string()),
            fn_type: ReactFunctionType::Component,
            params: vec![],
            returns: make_place(0),
            context: vec![],
            body: HIR {
                entry: BlockId(0),
                blocks: vec![
                    (
                        BlockId(0),
                        BasicBlock {
                            kind: BlockKind::Block,
                            id: BlockId(0),
                            instructions: vec![make_primitive(1, 10, PrimitiveValue::Number(1.0))],
                            terminal: Terminal::Scope {
                                block: BlockId(1),
                                fallthrough: BlockId(2),
                                scope,
                                id: InstructionId(2),
                                loc: SourceLocation::Generated,
                            },
                            preds: std::collections::HashSet::new(),
                            phis: vec![],
                        },
                    ),
                    (
                        BlockId(1),
                        BasicBlock {
                            kind: BlockKind::Block,
                            id: BlockId(1),
                            instructions: vec![make_primitive(3, 11, PrimitiveValue::Number(2.0))],
                            terminal: Terminal::Goto {
                                block: BlockId(2),
                                variant: GotoVariant::Break,
                                id: InstructionId(4),
                                loc: SourceLocation::Generated,
                            },
                            preds: std::collections::HashSet::new(),
                            phis: vec![],
                        },
                    ),
                    (
                        BlockId(2),
                        BasicBlock {
                            kind: BlockKind::Block,
                            id: BlockId(2),
                            instructions: vec![],
                            terminal: Terminal::Return {
                                value: make_place(99),
                                return_variant: ReturnVariant::Explicit,
                                id: InstructionId(5),
                                loc: SourceLocation::Generated,
                            },
                            preds: std::collections::HashSet::new(),
                            phis: vec![],
                        },
                    ),
                ],
            },
            generator: false,
            async_: false,
            directives: vec![],
            aliasing_effects: None,
        };

        let reactive = build_reactive_function(func);

        // Should have: 1 instruction (from bb0) + Scope + Return (from bb2)
        assert_eq!(reactive.body.len(), 3);

        // First is the Primitive(1) instruction.
        assert!(matches!(
            &reactive.body[0],
            ReactiveStatement::Instruction(_)
        ));

        // Second is the Scope.
        match &reactive.body[1] {
            ReactiveStatement::Scope(scope_block) => {
                assert_eq!(scope_block.scope.id, ScopeId(0));
                // The scope body should have 1 instruction (Primitive(2)).
                // The goto to bb2 is elided because bb2 is a scope fallthrough.
                assert_eq!(scope_block.instructions.len(), 1);
                assert!(matches!(
                    &scope_block.instructions[0],
                    ReactiveStatement::Instruction(_)
                ));
            }
            _ => panic!("Expected scope statement"),
        }

        // Third is the Return.
        assert!(matches!(
            &reactive.body[2],
            ReactiveStatement::Terminal(ReactiveTerminalStatement {
                terminal: ReactiveTerminal::Return { .. },
                ..
            })
        ));
    }

    #[test]
    fn test_while_loop() {
        // bb0: terminal=While(test=bb1, loop=bb2, fallthrough=bb3)
        // bb1: instructions=[LoadLocal], terminal=Branch(test=place(1), consequent=bb2, alternate=bb3)
        // bb2: instructions=[Primitive(42)], terminal=Goto(bb1, Continue)
        // bb3: instructions=[], terminal=Return

        let func = HIRFunction {
            env: crate::environment::Environment::new(crate::options::EnvironmentConfig::default()),
            loc: SourceLocation::Generated,
            id: Some("test_while".to_string()),
            fn_type: ReactFunctionType::Component,
            params: vec![],
            returns: make_place(0),
            context: vec![],
            body: HIR {
                entry: BlockId(0),
                blocks: vec![
                    (
                        BlockId(0),
                        BasicBlock {
                            kind: BlockKind::Block,
                            id: BlockId(0),
                            instructions: vec![],
                            terminal: Terminal::While {
                                test: BlockId(1),
                                loop_block: BlockId(2),
                                fallthrough: BlockId(3),
                                id: InstructionId(10),
                                loc: SourceLocation::Generated,
                            },
                            preds: std::collections::HashSet::new(),
                            phis: vec![],
                        },
                    ),
                    (
                        BlockId(1),
                        BasicBlock {
                            kind: BlockKind::Value,
                            id: BlockId(1),
                            instructions: vec![],
                            terminal: Terminal::Branch {
                                test: make_place(1),
                                consequent: BlockId(2),
                                alternate: BlockId(3),
                                fallthrough: BlockId(3),
                                id: InstructionId(11),
                                loc: SourceLocation::Generated,
                            },
                            preds: std::collections::HashSet::new(),
                            phis: vec![],
                        },
                    ),
                    (
                        BlockId(2),
                        BasicBlock {
                            kind: BlockKind::Block,
                            id: BlockId(2),
                            instructions: vec![make_primitive(
                                12,
                                20,
                                PrimitiveValue::Number(42.0),
                            )],
                            terminal: Terminal::Goto {
                                block: BlockId(1),
                                variant: GotoVariant::Continue,
                                id: InstructionId(13),
                                loc: SourceLocation::Generated,
                            },
                            preds: std::collections::HashSet::new(),
                            phis: vec![],
                        },
                    ),
                    (
                        BlockId(3),
                        BasicBlock {
                            kind: BlockKind::Block,
                            id: BlockId(3),
                            instructions: vec![],
                            terminal: Terminal::Return {
                                value: make_place(99),
                                return_variant: ReturnVariant::Explicit,
                                id: InstructionId(20),
                                loc: SourceLocation::Generated,
                            },
                            preds: std::collections::HashSet::new(),
                            phis: vec![],
                        },
                    ),
                ],
            },
            generator: false,
            async_: false,
            directives: vec![],
            aliasing_effects: None,
        };

        let reactive = build_reactive_function(func);

        // Should have: While terminal + Return
        assert_eq!(reactive.body.len(), 2);

        match &reactive.body[0] {
            ReactiveStatement::Terminal(term_stmt) => {
                match &term_stmt.terminal {
                    ReactiveTerminal::While {
                        test,
                        loop_block: body,
                        ..
                    } => {
                        // Test should reference place(1).
                        assert_eq!(test.identifier.id, IdentifierId(1));
                        // Loop body should have at least the Primitive(42) instruction.
                        assert!(!body.is_empty());
                    }
                    other => panic!("Expected While terminal, got {:?}", other),
                }
                // Should have label pointing to fallthrough bb3.
                assert!(term_stmt.label.is_some());
                assert_eq!(term_stmt.label.as_ref().unwrap().id, BlockId(3));
            }
            _ => panic!("Expected terminal statement"),
        }

        assert!(matches!(
            &reactive.body[1],
            ReactiveStatement::Terminal(ReactiveTerminalStatement {
                terminal: ReactiveTerminal::Return { .. },
                ..
            })
        ));
    }

    #[test]
    fn test_throw() {
        // bb0: terminal=Throw(value)
        let func = HIRFunction {
            env: crate::environment::Environment::new(crate::options::EnvironmentConfig::default()),
            loc: SourceLocation::Generated,
            id: Some("test_throw".to_string()),
            fn_type: ReactFunctionType::Component,
            params: vec![],
            returns: make_place(0),
            context: vec![],
            body: HIR {
                entry: BlockId(0),
                blocks: vec![(
                    BlockId(0),
                    BasicBlock {
                        kind: BlockKind::Block,
                        id: BlockId(0),
                        instructions: vec![],
                        terminal: Terminal::Throw {
                            value: make_place(1),
                            id: InstructionId(1),
                            loc: SourceLocation::Generated,
                        },
                        preds: std::collections::HashSet::new(),
                        phis: vec![],
                    },
                )],
            },
            generator: false,
            async_: false,
            directives: vec![],
            aliasing_effects: None,
        };

        let reactive = build_reactive_function(func);
        assert_eq!(reactive.body.len(), 1);
        assert!(matches!(
            &reactive.body[0],
            ReactiveStatement::Terminal(ReactiveTerminalStatement {
                terminal: ReactiveTerminal::Throw { .. },
                label: None,
            })
        ));
    }
}
