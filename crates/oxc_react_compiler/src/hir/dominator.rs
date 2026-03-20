//! Dominator tree computation for HIR CFG.
//!
//! Port of `Dominator.ts` from upstream React Compiler (babel-plugin-react-compiler).
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.
//!
//! Implements the standard iterative dominator algorithm from
//! "A Simple, Fast Dominance Algorithm" (Cooper, Harvey, Kennedy).
//! Also provides post-dominator tree computation.

use std::collections::{HashMap, HashSet};

use super::builder::each_terminal_successor;
use super::types::*;

// ---------------------------------------------------------------------------
// Internal graph representation
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Node {
    id: BlockId,
    index: usize,
    preds: HashSet<BlockId>,
    succs: HashSet<BlockId>,
}

#[derive(Debug)]
struct Graph {
    entry: BlockId,
    /// Nodes stored in insertion order (RPO for forward graph).
    nodes: Vec<(BlockId, Node)>,
    /// Fast lookup by BlockId.
    node_index: HashMap<BlockId, usize>,
}

impl Graph {
    fn get_node(&self, id: &BlockId) -> &Node {
        let idx = self.node_index[id];
        &self.nodes[idx].1
    }
}

// ---------------------------------------------------------------------------
// Dominator tree (forward dominators)
// ---------------------------------------------------------------------------

/// A dominator tree storing the immediate dominator for each block.
#[cfg(test)]
#[derive(Debug)]
pub struct Dominator {
    entry: BlockId,
    nodes: HashMap<BlockId, BlockId>,
}

#[cfg(test)]
impl Dominator {
    /// Returns the entry node.
    pub fn entry(&self) -> BlockId {
        self.entry
    }

    /// Returns the immediate dominator of `id`, or `None` if `id` is the entry
    /// (i.e. it dominates itself).
    pub fn get(&self, id: BlockId) -> Option<BlockId> {
        let dom = self.nodes.get(&id).expect("Unknown node in dominator tree");
        if *dom == id { None } else { Some(*dom) }
    }

    /// Returns true if block `a` dominates block `b`.
    pub fn dominates(&self, a: BlockId, b: BlockId) -> bool {
        let mut current = b;
        loop {
            if current == a {
                return true;
            }
            match self.get(current) {
                Some(dom) => current = dom,
                None => return current == a,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Post-dominator tree
// ---------------------------------------------------------------------------

/// A post-dominator tree storing the immediate post-dominator for each block.
#[derive(Debug)]
pub struct PostDominator {
    exit: BlockId,
    nodes: HashMap<BlockId, BlockId>,
}

impl PostDominator {
    /// Returns the exit node (a virtual node representing the function exit).
    pub fn exit(&self) -> BlockId {
        self.exit
    }

    /// Returns the immediate post-dominator of `id`, or `None` if `id` is the
    /// exit node itself.
    pub fn get(&self, id: BlockId) -> Option<BlockId> {
        let dom = self
            .nodes
            .get(&id)
            .expect("Unknown node in post-dominator tree");
        if *dom == id { None } else { Some(*dom) }
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Options for post-dominator tree computation.
pub struct PostDominatorOptions {
    /// Whether to treat `throw` terminals as exit nodes.
    pub include_throws_as_exit_node: bool,
}

/// Computes the dominator tree of the given function.
///
/// A block X dominates block Y in the CFG if all paths to Y must flow through X.
/// The entry block dominates all other blocks.
#[cfg(test)]
pub fn compute_dominator_tree(func: &HIRFunction) -> Dominator {
    let graph = build_graph(func);
    let nodes = compute_immediate_dominators(&graph);
    Dominator {
        entry: graph.entry,
        nodes,
    }
}

/// Computes the post-dominator tree of the given function.
///
/// A block Y post-dominates block X if all paths from X to the exit must flow
/// through Y. The caller specifies whether `throw` statements count as exit nodes.
pub fn compute_post_dominator_tree(
    func: &HIRFunction,
    options: PostDominatorOptions,
) -> PostDominator {
    let graph = build_reverse_graph(func, options.include_throws_as_exit_node);
    let mut nodes = compute_immediate_dominators(&graph);

    // When include_throws_as_exit_node is false, nodes that flow into a throw
    // terminal and don't reach the exit won't be in the node map.
    // Add them with themselves as dominator.
    if !options.include_throws_as_exit_node {
        for (id, _) in &func.body.blocks {
            nodes.entry(*id).or_insert(*id);
        }
    }

    PostDominator {
        exit: graph.entry,
        nodes,
    }
}

// ---------------------------------------------------------------------------
// Graph construction
// ---------------------------------------------------------------------------

/// Build a forward graph from the HIRFunction.
#[cfg(test)]
fn build_graph(func: &HIRFunction) -> Graph {
    let mut nodes = Vec::new();
    let mut node_index = HashMap::new();

    for (index, (id, block)) in func.body.blocks.iter().enumerate() {
        let succs: HashSet<BlockId> = each_terminal_successor(&block.terminal)
            .into_iter()
            .collect();
        let node = Node {
            id: *id,
            index,
            preds: block.preds.clone(),
            succs,
        };
        node_index.insert(*id, nodes.len());
        nodes.push((*id, node));
    }

    Graph {
        entry: func.body.entry,
        nodes,
        node_index,
    }
}

/// Build a reverse graph from the HIRFunction for post-dominator computation.
/// The reversed graph is put back into RPO form.
fn build_reverse_graph(func: &HIRFunction, include_throws_as_exit_node: bool) -> Graph {
    // Find the maximum block ID to create a virtual exit node.
    let max_id = func
        .body
        .blocks
        .iter()
        .map(|(id, _)| id.0)
        .max()
        .unwrap_or(0);
    let exit_id = BlockId(max_id + 1);

    let mut raw_nodes: HashMap<BlockId, Node> = HashMap::new();

    // Create exit node
    raw_nodes.insert(
        exit_id,
        Node {
            id: exit_id,
            index: 0,
            preds: HashSet::new(),
            succs: HashSet::new(),
        },
    );

    // Build reversed edges for each block
    for (id, block) in &func.body.blocks {
        let succs_in_forward: HashSet<BlockId> = each_terminal_successor(&block.terminal)
            .into_iter()
            .collect();

        let node = Node {
            id: *id,
            index: 0,
            // In reversed graph: preds become succs of forward graph
            preds: succs_in_forward,
            // In reversed graph: succs become preds of forward graph
            succs: block.preds.clone(),
        };

        // If this block returns, add the exit node as a predecessor (in reverse graph)
        // and this block as a successor of the exit node.
        let is_return = matches!(&block.terminal, Terminal::Return { .. });
        let is_throw = matches!(&block.terminal, Terminal::Throw { .. });

        if is_return || (is_throw && include_throws_as_exit_node) {
            // In reversed graph: exit -> this block (exit is pred of this block)
            let mut node = node;
            node.preds.insert(exit_id);
            raw_nodes.insert(*id, node);
            raw_nodes.get_mut(&exit_id).unwrap().succs.insert(*id);
        } else {
            raw_nodes.insert(*id, node);
        }
    }

    // Put nodes into RPO form via DFS from exit_id
    let mut visited = HashSet::new();
    let mut postorder = Vec::new();

    fn visit(
        id: BlockId,
        raw_nodes: &HashMap<BlockId, Node>,
        visited: &mut HashSet<BlockId>,
        postorder: &mut Vec<BlockId>,
    ) {
        if visited.contains(&id) {
            return;
        }
        visited.insert(id);
        if let Some(node) = raw_nodes.get(&id) {
            for &succ in &node.succs {
                visit(succ, raw_nodes, visited, postorder);
            }
        }
        postorder.push(id);
    }

    visit(exit_id, &raw_nodes, &mut visited, &mut postorder);

    // postorder is in postorder; reverse for RPO
    postorder.reverse();

    let mut nodes = Vec::new();
    let mut node_index = HashMap::new();
    for (index, id) in postorder.iter().enumerate() {
        if let Some(mut node) = raw_nodes.remove(id) {
            node.index = index;
            node_index.insert(*id, nodes.len());
            nodes.push((*id, node));
        }
    }

    Graph {
        entry: exit_id,
        nodes,
        node_index,
    }
}

// ---------------------------------------------------------------------------
// Iterative dominator algorithm (Cooper, Harvey, Kennedy)
// ---------------------------------------------------------------------------

fn compute_immediate_dominators(graph: &Graph) -> HashMap<BlockId, BlockId> {
    let mut doms: HashMap<BlockId, BlockId> = HashMap::new();
    doms.insert(graph.entry, graph.entry);

    let mut changed = true;
    while changed {
        changed = false;

        for (id, node) in &graph.nodes {
            // Skip start node
            if *id == graph.entry {
                continue;
            }

            // Find first processed predecessor
            let mut new_idom: Option<BlockId> = None;
            for pred in &node.preds {
                if doms.contains_key(pred) {
                    new_idom = Some(*pred);
                    break;
                }
            }

            let mut new_idom =
                new_idom.unwrap_or_else(|| panic!("No processed predecessor for block {id}"));

            // For all other processed predecessors, intersect
            for pred in &node.preds {
                if *pred == new_idom {
                    continue;
                }
                if doms.contains_key(pred) {
                    new_idom = intersect(*pred, new_idom, graph, &doms);
                }
            }

            let prev = doms.get(id);
            if prev != Some(&new_idom) {
                doms.insert(*id, new_idom);
                changed = true;
            }
        }
    }

    doms
}

fn intersect(a: BlockId, b: BlockId, graph: &Graph, doms: &HashMap<BlockId, BlockId>) -> BlockId {
    let mut block1 = graph.get_node(&a);
    let mut block2 = graph.get_node(&b);

    while block1.id != block2.id {
        while block1.index > block2.index {
            let dom = doms[&block1.id];
            block1 = graph.get_node(&dom);
        }
        while block2.index > block1.index {
            let dom = doms[&block2.id];
            block2 = graph.get_node(&dom);
        }
    }

    block1.id
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// Helper to build a minimal HIRFunction from a list of blocks
    /// with specified successors and predecessors.
    fn make_test_function(blocks: Vec<(u32, HashSet<u32>, Terminal)>) -> HIRFunction {
        let entry = BlockId(blocks[0].0);
        let body_blocks: Vec<(BlockId, BasicBlock)> = blocks
            .into_iter()
            .map(|(id, preds, terminal)| {
                let block_id = BlockId(id);
                let pred_set: HashSet<BlockId> = preds.into_iter().map(BlockId).collect();
                (
                    block_id,
                    BasicBlock {
                        kind: BlockKind::Block,
                        id: block_id,
                        instructions: vec![],
                        terminal,
                        preds: pred_set,
                        phis: vec![],
                    },
                )
            })
            .collect();

        HIRFunction {
            env: crate::environment::Environment::new(crate::options::EnvironmentConfig::default()),
            id: None,
            fn_type: ReactFunctionType::Component,
            params: vec![],
            returns: Place {
                identifier: make_temporary_identifier(IdentifierId(0), SourceLocation::Generated),
                effect: Effect::Unknown,
                reactive: false,
                loc: SourceLocation::Generated,
            },
            context: vec![],
            body: HIR {
                entry,
                blocks: body_blocks,
            },
            generator: false,
            async_: false,
            directives: vec![],
            aliasing_effects: None,
        }
    }

    fn goto_terminal(target: u32) -> Terminal {
        Terminal::Goto {
            block: BlockId(target),
            variant: GotoVariant::Break,
            loc: SourceLocation::Generated,
            id: InstructionId(0),
        }
    }

    fn if_terminal(consequent: u32, alternate: u32, fallthrough: u32) -> Terminal {
        Terminal::If {
            test: Place {
                identifier: make_temporary_identifier(IdentifierId(0), SourceLocation::Generated),
                effect: Effect::Unknown,
                reactive: false,
                loc: SourceLocation::Generated,
            },
            loc: SourceLocation::Generated,
            consequent: BlockId(consequent),
            alternate: BlockId(alternate),
            fallthrough: BlockId(fallthrough),
            id: InstructionId(0),
        }
    }

    fn return_terminal() -> Terminal {
        Terminal::Return {
            value: Place {
                identifier: make_temporary_identifier(IdentifierId(0), SourceLocation::Generated),
                effect: Effect::Unknown,
                reactive: false,
                loc: SourceLocation::Generated,
            },
            return_variant: ReturnVariant::Explicit,
            loc: SourceLocation::Generated,
            id: InstructionId(0),
        }
    }

    #[test]
    fn test_linear_dominator_tree() {
        // bb0 -> bb1 -> bb2 (return)
        let func = make_test_function(vec![
            (0, HashSet::new(), goto_terminal(1)),
            (1, HashSet::from([0]), goto_terminal(2)),
            (2, HashSet::from([1]), return_terminal()),
        ]);

        let dom = compute_dominator_tree(&func);

        assert_eq!(dom.entry(), BlockId(0));
        // bb0 dominates itself (returns None)
        assert_eq!(dom.get(BlockId(0)), None);
        // bb1's immediate dominator is bb0
        assert_eq!(dom.get(BlockId(1)), Some(BlockId(0)));
        // bb2's immediate dominator is bb1
        assert_eq!(dom.get(BlockId(2)), Some(BlockId(1)));
    }

    #[test]
    fn test_diamond_dominator_tree() {
        // bb0 -> bb1, bb2 (if)
        // bb1 -> bb3 (goto)
        // bb2 -> bb3 (goto)
        // bb3 -> return
        let func = make_test_function(vec![
            (0, HashSet::new(), if_terminal(1, 2, 3)),
            (1, HashSet::from([0]), goto_terminal(3)),
            (2, HashSet::from([0]), goto_terminal(3)),
            (3, HashSet::from([1, 2]), return_terminal()),
        ]);

        let dom = compute_dominator_tree(&func);

        assert_eq!(dom.get(BlockId(0)), None);
        assert_eq!(dom.get(BlockId(1)), Some(BlockId(0)));
        assert_eq!(dom.get(BlockId(2)), Some(BlockId(0)));
        // bb3's immediate dominator is bb0 (the merge point)
        assert_eq!(dom.get(BlockId(3)), Some(BlockId(0)));
    }

    #[test]
    fn test_dominates() {
        // bb0 -> bb1 -> bb2 (return)
        let func = make_test_function(vec![
            (0, HashSet::new(), goto_terminal(1)),
            (1, HashSet::from([0]), goto_terminal(2)),
            (2, HashSet::from([1]), return_terminal()),
        ]);

        let dom = compute_dominator_tree(&func);

        assert!(dom.dominates(BlockId(0), BlockId(0)));
        assert!(dom.dominates(BlockId(0), BlockId(1)));
        assert!(dom.dominates(BlockId(0), BlockId(2)));
        assert!(dom.dominates(BlockId(1), BlockId(2)));
        assert!(!dom.dominates(BlockId(2), BlockId(0)));
        assert!(!dom.dominates(BlockId(1), BlockId(0)));
    }

    #[test]
    fn test_post_dominator_tree() {
        // bb0 -> bb1, bb2 (if)
        // bb1 -> bb3 (goto)
        // bb2 -> bb3 (goto)
        // bb3 -> return
        let func = make_test_function(vec![
            (0, HashSet::new(), if_terminal(1, 2, 3)),
            (1, HashSet::from([0]), goto_terminal(3)),
            (2, HashSet::from([0]), goto_terminal(3)),
            (3, HashSet::from([1, 2]), return_terminal()),
        ]);

        let post_dom = compute_post_dominator_tree(
            &func,
            PostDominatorOptions {
                include_throws_as_exit_node: false,
            },
        );

        // bb3 post-dominates all blocks since all paths lead to it
        assert_eq!(post_dom.get(BlockId(0)), Some(BlockId(3)));
        assert_eq!(post_dom.get(BlockId(1)), Some(BlockId(3)));
        assert_eq!(post_dom.get(BlockId(2)), Some(BlockId(3)));
        // bb3's post-dominator is the exit node
        assert_eq!(post_dom.get(BlockId(3)), Some(post_dom.exit()));
    }

    #[test]
    fn test_linear_post_dominator() {
        // bb0 -> bb1 -> bb2 (return)
        let func = make_test_function(vec![
            (0, HashSet::new(), goto_terminal(1)),
            (1, HashSet::from([0]), goto_terminal(2)),
            (2, HashSet::from([1]), return_terminal()),
        ]);

        let post_dom = compute_post_dominator_tree(
            &func,
            PostDominatorOptions {
                include_throws_as_exit_node: false,
            },
        );

        // In a linear chain, each block's post-dominator is the next block
        assert_eq!(post_dom.get(BlockId(0)), Some(BlockId(1)));
        assert_eq!(post_dom.get(BlockId(1)), Some(BlockId(2)));
        assert_eq!(post_dom.get(BlockId(2)), Some(post_dom.exit()));
    }
}
