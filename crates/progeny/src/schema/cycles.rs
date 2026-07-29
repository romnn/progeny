//! Strongly connected components over a directed graph of node indices.
//!
//! Recursion in a description is not an error and not rare: a comment has replies, a folder holds
//! folders. What matters is knowing *which* nodes take part in a cycle, because Rust needs an
//! indirection somewhere on every cycle and the choice of where has to be deterministic.
//!
//! Tarjan's algorithm, written **iteratively**. A recursive implementation would be shorter and
//! would overflow the stack on a document whose schemas form one long reference chain — the
//! corpus already holds 38,269 schemas in a single document, and the fuzz targets construct worse.
//! "The generator must not panic on any input" makes an explicit work stack the only option.

/// Which component each node belongs to, and therefore which nodes are recursive.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Sccs {
    /// The component index of each node, in reverse topological order of the components.
    component: Vec<u32>,
    /// How many nodes each component holds.
    size: Vec<u32>,
    /// Whether the node has an edge to itself, which is a cycle a component of one hides.
    self_loop: Vec<bool>,
}

/// A node index whose depth-first traversal has not started.
const UNVISITED: u32 = u32::MAX;

impl Sccs {
    /// Compute the components of a graph with `nodes` nodes and these edges.
    ///
    /// Edges naming a node outside the graph are ignored rather than rejected: the callers build
    /// edge lists out of resolved references, and one that does not resolve is a diagnosed
    /// finding, not a reason to abandon the whole analysis.
    pub(crate) fn of(nodes: usize, edges: &[(u32, u32)]) -> Self {
        let graph = Csr::new(nodes, edges);
        let mut state = Tarjan {
            index: 0,
            next_component: 0,
            order: vec![UNVISITED; nodes],
            low: vec![0; nodes],
            component: vec![UNVISITED; nodes],
            on_stack: vec![false; nodes],
            stack: Vec::new(),
            work: Vec::new(),
            size: Vec::new(),
        };
        for root in 0..nodes {
            state.run(&graph, root);
        }

        let mut self_loop = vec![false; nodes];
        for &(from, to) in edges {
            if from == to
                && let Some(slot) = self_loop.get_mut(from as usize)
            {
                *slot = true;
            }
        }
        Self {
            component: state.component,
            size: state.size,
            self_loop,
        }
    }

    /// Whether the node lies on a cycle.
    ///
    /// True when its component holds more than one node, and also when it merely points at
    /// itself — a component of one still closes a cycle, and that is exactly the shape a
    /// self-referential schema takes.
    pub(crate) fn recursive(&self, node: usize) -> bool {
        self.self_loop.get(node).copied().unwrap_or(false) || self.component_size(node) > 1
    }

    /// Whether both nodes are on the same cycle, which is what makes an edge between them a
    /// closing edge rather than a step outward.
    pub(crate) fn together(&self, from: usize, to: usize) -> bool {
        match (self.component.get(from), self.component.get(to)) {
            (Some(a), Some(b)) => a == b,
            _ => false,
        }
    }

    fn component_size(&self, node: usize) -> u32 {
        self.component
            .get(node)
            .and_then(|&index| self.size.get(index as usize))
            .copied()
            .unwrap_or(0)
    }

    /// How many components hold more than one node. The count the corpus reports.
    pub(crate) fn recursive_groups(&self) -> usize {
        self.size.iter().filter(|&&size| size > 1).count()
    }

    /// The size of the largest component.
    pub(crate) fn largest_group(&self) -> u32 {
        self.size.iter().copied().max().unwrap_or(0)
    }
}

/// The graph in compressed adjacency form: one allocation for the offsets, one for the targets.
struct Csr {
    offsets: Vec<u32>,
    targets: Vec<u32>,
}

impl Csr {
    fn new(nodes: usize, edges: &[(u32, u32)]) -> Self {
        let mut offsets = vec![0_u32; nodes + 1];
        for &(from, _) in edges {
            if let Some(slot) = offsets.get_mut(from as usize + 1) {
                *slot = slot.saturating_add(1);
            }
        }
        for index in 1..=nodes {
            let running = offsets.get(index - 1).copied().unwrap_or(0);
            if let Some(slot) = offsets.get_mut(index) {
                *slot = slot.saturating_add(running);
            }
        }
        let mut targets = vec![0_u32; edges.len()];
        let mut cursor = offsets.clone();
        for &(from, to) in edges {
            let Some(position) = cursor.get_mut(from as usize) else {
                continue;
            };
            let at = *position as usize;
            *position += 1;
            if let Some(slot) = targets.get_mut(at) {
                *slot = to;
            }
        }
        Self { offsets, targets }
    }

    fn neighbours(&self, node: usize) -> &[u32] {
        let start = self.offsets.get(node).copied().unwrap_or(0) as usize;
        let end = self.offsets.get(node + 1).copied().unwrap_or(0) as usize;
        self.targets.get(start..end).unwrap_or_default()
    }
}

/// Tarjan's state, with the recursion turned into `work`.
struct Tarjan {
    index: u32,
    next_component: u32,
    order: Vec<u32>,
    low: Vec<u32>,
    component: Vec<u32>,
    on_stack: Vec<bool>,
    stack: Vec<u32>,
    /// The explicit call stack: which node, and how many of its neighbours have been taken.
    work: Vec<(u32, u32)>,
    size: Vec<u32>,
}

impl Tarjan {
    fn run(&mut self, graph: &Csr, root: usize) {
        if self.order.get(root).copied().unwrap_or(UNVISITED) != UNVISITED {
            return;
        }
        let Ok(root) = u32::try_from(root) else {
            return;
        };
        self.work.push((root, 0));
        self.enter(root);

        while let Some(&(node, taken)) = self.work.last() {
            let neighbours = graph.neighbours(node as usize);
            let mut descended = false;
            let mut cursor = taken as usize;
            while let Some(&next) = neighbours.get(cursor) {
                cursor += 1;
                if self.order.get(next as usize).copied().unwrap_or(UNVISITED) == UNVISITED {
                    // Remember how far this node got before descending, or its remaining
                    // neighbours would be visited twice when the traversal comes back.
                    self.set_taken(cursor);
                    self.work.push((next, 0));
                    self.enter(next);
                    descended = true;
                    break;
                }
                if self.on_stack.get(next as usize).copied().unwrap_or(false) {
                    let seen = self.order.get(next as usize).copied().unwrap_or(UNVISITED);
                    self.lower(node, seen);
                }
            }
            if descended {
                continue;
            }
            self.set_taken(cursor);
            if self.low.get(node as usize) == self.order.get(node as usize) {
                self.close(node);
            }
            self.work.pop();
            // The parent's low-link is the smallest of its own and its children's.
            if let Some(&(parent, _)) = self.work.last() {
                let reached = self.low.get(node as usize).copied().unwrap_or(UNVISITED);
                self.lower(parent, reached);
            }
        }
    }

    fn enter(&mut self, node: u32) {
        if let Some(slot) = self.order.get_mut(node as usize) {
            *slot = self.index;
        }
        if let Some(slot) = self.low.get_mut(node as usize) {
            *slot = self.index;
        }
        self.index = self.index.saturating_add(1);
        self.stack.push(node);
        if let Some(slot) = self.on_stack.get_mut(node as usize) {
            *slot = true;
        }
    }

    fn set_taken(&mut self, cursor: usize) {
        if let Some((_, taken)) = self.work.last_mut() {
            *taken = u32::try_from(cursor).unwrap_or(u32::MAX);
        }
    }

    fn lower(&mut self, node: u32, candidate: u32) {
        if let Some(slot) = self.low.get_mut(node as usize) {
            *slot = (*slot).min(candidate);
        }
    }

    /// `node` is the root of a component: everything above it on the stack belongs to it.
    fn close(&mut self, node: u32) {
        let index = self.next_component;
        self.next_component = self.next_component.saturating_add(1);
        let mut size = 0_u32;
        while let Some(member) = self.stack.pop() {
            if let Some(slot) = self.on_stack.get_mut(member as usize) {
                *slot = false;
            }
            if let Some(slot) = self.component.get_mut(member as usize) {
                *slot = index;
            }
            size = size.saturating_add(1);
            if member == node {
                break;
            }
        }
        self.size.push(size);
    }
}

#[cfg(test)]
mod tests {
    use color_eyre::eyre;

    use super::Sccs;

    #[test_util::test]
    fn a_graph_with_no_edges_has_no_cycles() {
        let sccs = Sccs::of(3, &[]);
        assert!(!sccs.recursive(0));
        assert_eq!(sccs.recursive_groups(), 0);
        assert_eq!(sccs.largest_group(), 1);
    }

    #[test_util::test]
    fn a_node_pointing_at_itself_is_recursive() {
        let sccs = Sccs::of(2, &[(0, 0)]);
        assert!(sccs.recursive(0));
        assert!(!sccs.recursive(1));
        // A self-loop is a cycle inside a component of one, which is why the size test alone
        // would miss it.
        assert_eq!(sccs.recursive_groups(), 0);
    }

    #[test_util::test]
    fn mutual_references_land_in_one_component() {
        let sccs = Sccs::of(4, &[(0, 1), (1, 0), (1, 2), (2, 3)]);
        assert!(sccs.recursive(0));
        assert!(sccs.recursive(1));
        assert!(sccs.together(0, 1));
        assert!(!sccs.recursive(2));
        assert!(!sccs.together(1, 2));
        assert_eq!(sccs.recursive_groups(), 1);
        assert_eq!(sccs.largest_group(), 2);
    }

    #[test_util::test]
    fn a_long_cycle_is_one_component() {
        let edges: Vec<(u32, u32)> = (0..1000).map(|node| (node, (node + 1) % 1000)).collect();
        let sccs = Sccs::of(1000, &edges);
        assert_eq!(sccs.recursive_groups(), 1);
        assert_eq!(sccs.largest_group(), 1000);
        assert!(sccs.together(0, 999));
    }

    #[test_util::test]
    fn a_deep_chain_does_not_overflow_the_stack() {
        // The whole reason this is iterative. A recursive Tarjan dies somewhere around here.
        const NODES: usize = 400_000;
        let last = u32::try_from(NODES)? - 1;
        let edges: Vec<(u32, u32)> = (0..last).map(|node| (node, node + 1)).collect();
        let sccs = Sccs::of(NODES, &edges);
        assert_eq!(sccs.recursive_groups(), 0);
        assert!(!sccs.recursive(0));
    }

    #[test_util::test]
    fn edges_leaving_the_graph_are_ignored() {
        let sccs = Sccs::of(2, &[(0, 7), (9, 0), (0, 1)]);
        assert!(!sccs.recursive(0));
        assert!(!sccs.together(0, 1));
    }

    #[test_util::test]
    fn two_separate_cycles_stay_separate() {
        let sccs = Sccs::of(4, &[(0, 1), (1, 0), (2, 3), (3, 2)]);
        assert_eq!(sccs.recursive_groups(), 2);
        assert!(sccs.together(0, 1));
        assert!(sccs.together(2, 3));
        assert!(!sccs.together(0, 2));
    }
}
