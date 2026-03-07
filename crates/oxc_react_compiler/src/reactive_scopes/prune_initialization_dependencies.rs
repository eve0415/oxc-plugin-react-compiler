//! Prune initialization-only dependencies in reactive scopes.
//!
//! Port of upstream `PruneInitializationDependencies.ts` (change-detection mode).

use std::collections::HashMap;

use crate::hir::types::*;
use crate::hir::visitors;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CreateUpdate {
    Create,
    Update,
    Unknown,
}

#[derive(Default)]
struct DisjointSet {
    parent: HashMap<IdentifierId, IdentifierId>,
}

impl DisjointSet {
    fn find_mut(&mut self, id: IdentifierId) -> IdentifierId {
        let parent = self.parent.get(&id).copied().unwrap_or(id);
        if parent == id {
            self.parent.insert(id, id);
            return id;
        }
        let root = self.find_mut(parent);
        self.parent.insert(id, root);
        root
    }

    fn find_const(&self, id: IdentifierId) -> IdentifierId {
        let mut cur = id;
        while let Some(parent) = self.parent.get(&cur).copied() {
            if parent == cur {
                return cur;
            }
            cur = parent;
        }
        id
    }

    fn union(&mut self, left: IdentifierId, right: IdentifierId) {
        let left_root = self.find_mut(left);
        let right_root = self.find_mut(right);
        if left_root != right_root {
            self.parent.insert(right_root, left_root);
        }
    }
}

struct Visitor {
    map: HashMap<IdentifierId, CreateUpdate>,
    aliases: DisjointSet,
    paths: HashMap<IdentifierId, HashMap<String, IdentifierId>>,
}

impl Visitor {
    fn new(
        aliases: DisjointSet,
        paths: HashMap<IdentifierId, HashMap<String, IdentifierId>>,
    ) -> Self {
        Self {
            map: HashMap::new(),
            aliases,
            paths,
        }
    }

    fn join(values: impl IntoIterator<Item = CreateUpdate>) -> CreateUpdate {
        values
            .into_iter()
            .fold(CreateUpdate::Unknown, |left, right| {
                if left == CreateUpdate::Update || right == CreateUpdate::Update {
                    CreateUpdate::Update
                } else if left == CreateUpdate::Create || right == CreateUpdate::Create {
                    CreateUpdate::Create
                } else {
                    CreateUpdate::Unknown
                }
            })
    }

    fn state_for_id(&self, id: IdentifierId) -> CreateUpdate {
        self.map.get(&id).copied().unwrap_or(CreateUpdate::Unknown)
    }

    fn record_place(&mut self, place: &Place, state: CreateUpdate) {
        let prior = self.state_for_id(place.identifier.id);
        let next = Self::join([state, prior]);
        self.map.insert(place.identifier.id, next);
    }

    fn record_argument(&mut self, arg: &Argument, state: CreateUpdate) {
        match arg {
            Argument::Place(place) | Argument::Spread(place) => self.record_place(place, state),
        }
    }

    fn traverse_instruction_with_state(
        &mut self,
        instr: &ReactiveInstruction,
        state: CreateUpdate,
    ) {
        for_each_reactive_instruction_lvalue(instr, |place| {
            self.record_place(place, state);
        });
        visitors::for_each_instruction_value_operand(&instr.value, |place| {
            self.record_place(place, state);
        });
    }

    fn visit_instruction(&mut self, instr: &ReactiveInstruction) {
        let mut lvalue_states: Vec<CreateUpdate> = Vec::new();
        for_each_reactive_instruction_lvalue(instr, |place| {
            lvalue_states.push(self.state_for_id(place.identifier.id));
        });
        let state = Self::join(lvalue_states);

        match &instr.value {
            InstructionValue::CallExpression { callee, args, .. } => {
                if instr
                    .lvalue
                    .as_ref()
                    .is_some_and(|lvalue| is_create_only_hook_result(&lvalue.identifier))
                {
                    for arg in args {
                        self.record_argument(arg, CreateUpdate::Create);
                    }
                    self.record_place(callee, state);
                } else {
                    let next_state = if is_hook_identifier(&callee.identifier) {
                        CreateUpdate::Update
                    } else {
                        state
                    };
                    self.traverse_instruction_with_state(instr, next_state);
                }
            }
            InstructionValue::MethodCall {
                receiver,
                property,
                args,
                ..
            } => {
                if instr
                    .lvalue
                    .as_ref()
                    .is_some_and(|lvalue| is_create_only_hook_result(&lvalue.identifier))
                {
                    for arg in args {
                        self.record_argument(arg, CreateUpdate::Create);
                    }
                    self.record_place(receiver, state);
                    self.record_place(property, state);
                } else {
                    let next_state = if is_hook_identifier(&property.identifier) {
                        CreateUpdate::Update
                    } else {
                        state
                    };
                    self.traverse_instruction_with_state(instr, next_state);
                }
            }
            _ => self.traverse_instruction_with_state(instr, state),
        }
    }

    fn dependency_is_create_only(&self, dep: &ReactiveScopeDependency) -> bool {
        let mut target = self.aliases.find_const(dep.identifier.id);
        for segment in &dep.path {
            let Some(next) = self
                .paths
                .get(&target)
                .and_then(|inner| inner.get(&segment.property))
                .copied()
            else {
                return false;
            };
            target = next;
        }
        self.state_for_id(target) == CreateUpdate::Create
    }

    fn visit_scope(&mut self, scope_block: &mut ReactiveScopeBlock) {
        let state = Self::join(
            scope_block
                .scope
                .declarations
                .keys()
                .map(|id| self.state_for_id(*id))
                .chain(
                    scope_block
                        .scope
                        .reassignments
                        .iter()
                        .map(|ident| self.state_for_id(ident.id)),
                ),
        );
        self.visit_block(&mut scope_block.instructions, state);

        let debug = std::env::var("DEBUG_SCOPE_PRUNE_REASON").is_ok();
        let mut removed: Vec<String> = Vec::new();
        scope_block.scope.dependencies.retain(|dep| {
            let create_only = self.dependency_is_create_only(dep);
            if create_only && debug {
                removed.push(format_dependency(dep));
            }
            !create_only
        });
        if debug && !removed.is_empty() {
            eprintln!(
                "[SCOPE_PRUNE_REASON] scope={} pass=prune_initialization_dependencies reason=create-only removed={}",
                scope_block.scope.id.0,
                removed.join(", ")
            );
        }
    }

    fn visit_terminal(&mut self, terminal: &mut ReactiveTerminal, state: CreateUpdate) {
        match terminal {
            ReactiveTerminal::Return { value, .. } | ReactiveTerminal::Throw { value, .. } => {
                self.record_place(value, state);
            }
            ReactiveTerminal::If {
                test,
                consequent,
                alternate,
                ..
            } => {
                self.record_place(test, state);
                self.visit_block(consequent, state);
                if let Some(alternate) = alternate {
                    self.visit_block(alternate, state);
                }
            }
            ReactiveTerminal::Switch { test, cases, .. } => {
                self.record_place(test, state);
                for case in cases {
                    if let Some(case_test) = &case.test {
                        self.record_place(case_test, state);
                    }
                    if let Some(case_block) = &mut case.block {
                        self.visit_block(case_block, state);
                    }
                }
            }
            ReactiveTerminal::DoWhile {
                loop_block, test, ..
            } => {
                self.visit_block(loop_block, state);
                self.record_place(test, state);
            }
            ReactiveTerminal::While {
                test, loop_block, ..
            } => {
                self.record_place(test, state);
                self.visit_block(loop_block, state);
            }
            ReactiveTerminal::For {
                init,
                test,
                update,
                loop_block,
                ..
            } => {
                self.visit_block(init, state);
                self.record_place(test, state);
                if let Some(update) = update {
                    self.visit_block(update, state);
                }
                self.visit_block(loop_block, state);
            }
            ReactiveTerminal::ForOf {
                init,
                test,
                loop_block,
                ..
            } => {
                self.visit_block(init, state);
                self.record_place(test, state);
                self.visit_block(loop_block, state);
            }
            ReactiveTerminal::ForIn {
                init, loop_block, ..
            } => {
                self.visit_block(init, state);
                self.visit_block(loop_block, state);
            }
            ReactiveTerminal::Label { block, .. } => {
                self.visit_block(block, state);
            }
            ReactiveTerminal::Try {
                block,
                handler_binding,
                handler,
                ..
            } => {
                self.visit_block(block, state);
                if let Some(handler_binding) = handler_binding {
                    self.record_place(handler_binding, state);
                }
                self.visit_block(handler, state);
            }
            ReactiveTerminal::Break { .. } | ReactiveTerminal::Continue { .. } => {}
        }
    }

    fn visit_block(&mut self, block: &mut ReactiveBlock, state: CreateUpdate) {
        for stmt in block.iter_mut().rev() {
            match stmt {
                ReactiveStatement::Instruction(instr) => self.visit_instruction(instr),
                ReactiveStatement::Terminal(term_stmt) => {
                    self.visit_terminal(&mut term_stmt.terminal, state)
                }
                ReactiveStatement::Scope(scope_block) => self.visit_scope(scope_block),
                ReactiveStatement::PrunedScope(scope_block) => {
                    self.visit_block(&mut scope_block.instructions, state)
                }
            }
        }
    }
}

pub fn prune_initialization_dependencies(func: &mut ReactiveFunction) {
    let (aliases, paths) = collect_aliases_and_paths(func);
    let mut visitor = Visitor::new(aliases, paths);
    visitor.visit_block(&mut func.body, CreateUpdate::Update);
}

fn collect_aliases_and_paths(
    func: &ReactiveFunction,
) -> (
    DisjointSet,
    HashMap<IdentifierId, HashMap<String, IdentifierId>>,
) {
    let mut aliases = DisjointSet::default();
    let mut raw_paths: HashMap<IdentifierId, HashMap<String, IdentifierId>> = HashMap::new();
    collect_aliases_and_paths_from_block(&func.body, &mut aliases, &mut raw_paths);

    let mut normalized_paths: HashMap<IdentifierId, HashMap<String, IdentifierId>> = HashMap::new();
    for (key, value) in raw_paths {
        let key_root = aliases.find_const(key);
        for (property, id) in value {
            let id_root = aliases.find_const(id);
            normalized_paths
                .entry(key_root)
                .or_default()
                .insert(property, id_root);
        }
    }

    (aliases, normalized_paths)
}

fn collect_aliases_and_paths_from_block(
    block: &ReactiveBlock,
    aliases: &mut DisjointSet,
    paths: &mut HashMap<IdentifierId, HashMap<String, IdentifierId>>,
) {
    for stmt in block {
        match stmt {
            ReactiveStatement::Instruction(instr) => {
                collect_aliases_and_paths_from_instruction(instr, aliases, paths);
            }
            ReactiveStatement::Terminal(term_stmt) => {
                collect_aliases_and_paths_from_terminal(&term_stmt.terminal, aliases, paths);
            }
            ReactiveStatement::Scope(scope_block) => {
                collect_aliases_and_paths_from_block(&scope_block.instructions, aliases, paths);
            }
            ReactiveStatement::PrunedScope(scope_block) => {
                collect_aliases_and_paths_from_block(&scope_block.instructions, aliases, paths);
            }
        }
    }
}

fn collect_aliases_and_paths_from_terminal(
    terminal: &ReactiveTerminal,
    aliases: &mut DisjointSet,
    paths: &mut HashMap<IdentifierId, HashMap<String, IdentifierId>>,
) {
    match terminal {
        ReactiveTerminal::If {
            consequent,
            alternate,
            ..
        } => {
            collect_aliases_and_paths_from_block(consequent, aliases, paths);
            if let Some(alternate) = alternate {
                collect_aliases_and_paths_from_block(alternate, aliases, paths);
            }
        }
        ReactiveTerminal::Switch { cases, .. } => {
            for case in cases {
                if let Some(block) = &case.block {
                    collect_aliases_and_paths_from_block(block, aliases, paths);
                }
            }
        }
        ReactiveTerminal::DoWhile { loop_block, .. }
        | ReactiveTerminal::While { loop_block, .. } => {
            collect_aliases_and_paths_from_block(loop_block, aliases, paths);
        }
        ReactiveTerminal::For {
            init,
            update,
            loop_block,
            ..
        } => {
            collect_aliases_and_paths_from_block(init, aliases, paths);
            if let Some(update) = update {
                collect_aliases_and_paths_from_block(update, aliases, paths);
            }
            collect_aliases_and_paths_from_block(loop_block, aliases, paths);
        }
        ReactiveTerminal::ForOf {
            init, loop_block, ..
        }
        | ReactiveTerminal::ForIn {
            init, loop_block, ..
        } => {
            collect_aliases_and_paths_from_block(init, aliases, paths);
            collect_aliases_and_paths_from_block(loop_block, aliases, paths);
        }
        ReactiveTerminal::Label { block, .. } => {
            collect_aliases_and_paths_from_block(block, aliases, paths);
        }
        ReactiveTerminal::Try { block, handler, .. } => {
            collect_aliases_and_paths_from_block(block, aliases, paths);
            collect_aliases_and_paths_from_block(handler, aliases, paths);
        }
        ReactiveTerminal::Break { .. }
        | ReactiveTerminal::Continue { .. }
        | ReactiveTerminal::Return { .. }
        | ReactiveTerminal::Throw { .. } => {}
    }
}

fn collect_aliases_and_paths_from_instruction(
    instr: &ReactiveInstruction,
    aliases: &mut DisjointSet,
    paths: &mut HashMap<IdentifierId, HashMap<String, IdentifierId>>,
) {
    match &instr.value {
        InstructionValue::StoreLocal { lvalue, value, .. }
        | InstructionValue::StoreContext { lvalue, value, .. } => {
            aliases.union(lvalue.place.identifier.id, value.identifier.id);
        }
        InstructionValue::LoadLocal { place, .. } | InstructionValue::LoadContext { place, .. } => {
            if let Some(lvalue) = &instr.lvalue {
                aliases.union(lvalue.identifier.id, place.identifier.id);
            }
        }
        InstructionValue::PropertyLoad {
            object, property, ..
        } => {
            if let Some(lvalue) = &instr.lvalue {
                paths
                    .entry(object.identifier.id)
                    .or_default()
                    .insert(property_literal_key(property), lvalue.identifier.id);
            }
        }
        InstructionValue::PropertyStore {
            object,
            property,
            value,
            ..
        } => {
            paths
                .entry(object.identifier.id)
                .or_default()
                .insert(property_literal_key(property), value.identifier.id);
        }
        _ => {}
    }
}

fn property_literal_key(property: &PropertyLiteral) -> String {
    match property {
        PropertyLiteral::String(value) => value.clone(),
        PropertyLiteral::Number(value) => {
            if value.fract() == 0.0 {
                (*value as i64).to_string()
            } else {
                value.to_string()
            }
        }
    }
}

fn for_each_reactive_instruction_lvalue(instr: &ReactiveInstruction, mut f: impl FnMut(&Place)) {
    if let Some(lvalue) = &instr.lvalue {
        f(lvalue);
    }
    match &instr.value {
        InstructionValue::StoreLocal { lvalue, .. }
        | InstructionValue::StoreContext { lvalue, .. }
        | InstructionValue::DeclareLocal { lvalue, .. }
        | InstructionValue::DeclareContext { lvalue, .. } => f(&lvalue.place),
        InstructionValue::Destructure { lvalue, .. } => {
            visitors::for_each_pattern_place(&lvalue.pattern, &mut f);
        }
        InstructionValue::PrefixUpdate { lvalue, .. }
        | InstructionValue::PostfixUpdate { lvalue, .. } => f(lvalue),
        _ => {}
    }
}

fn is_create_only_hook_result(id: &Identifier) -> bool {
    matches!(&id.type_, Type::Object { shape_id: Some(shape) } if shape == "BuiltInUseStateHookResult" || shape == "BuiltInUseRefId")
}

fn is_hook_identifier(id: &Identifier) -> bool {
    if matches!(
        &id.type_,
        Type::Function {
            shape_id: Some(shape),
            ..
        } if shape.ends_with("HookId")
    ) {
        return true;
    }
    id.name
        .as_ref()
        .is_some_and(|name| is_hook_or_use_name(name.value()))
}

fn is_hook_or_use_name(name: &str) -> bool {
    if name == "use" || name.starts_with("use") {
        return true;
    }
    for segment in name.split('$') {
        if segment == "use" || segment.starts_with("use") {
            return true;
        }
    }
    false
}

fn format_dependency(dep: &ReactiveScopeDependency) -> String {
    let base = dep
        .identifier
        .name
        .as_ref()
        .map(|name| name.value().to_string())
        .unwrap_or_else(|| format!("id{}", dep.identifier.id.0));
    if dep.path.is_empty() {
        return base;
    }
    let path = dep
        .path
        .iter()
        .map(|segment| segment.property.as_str())
        .collect::<Vec<_>>()
        .join(".");
    format!("{}.{}", base, path)
}
