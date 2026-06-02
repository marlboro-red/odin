//! Dependency-graph utilities over a workflow's `depends_on` edges.
//!
//! One implementation serves three consumers: the cycle check (`ODIN014`), the
//! artifact-ordering check (`ODIN015`) and template-ref checking (`ODIN017`) via
//! [`ancestor_sets`], and the future executor via [`topo_order`].

use indexmap::{IndexMap, IndexSet};

use crate::ids::StepId;
use crate::ir::Workflow;

/// Maps each step to its *declared* direct dependencies, dropping edges that point at
/// undeclared steps (those are reported separately by `ODIN012`).
fn dep_map(wf: &Workflow) -> IndexMap<StepId, Vec<StepId>> {
    let declared: IndexSet<&str> = wf.steps.iter().map(|s| s.id.as_str()).collect();
    let mut m = IndexMap::with_capacity(wf.steps.len());
    for s in &wf.steps {
        let deps = s
            .depends_on
            .iter()
            .filter(|d| declared.contains(d.as_str()))
            .cloned()
            .collect();
        m.insert(s.id.clone(), deps);
    }
    m
}

/// Returns a topological order of the steps (dependencies before dependents), with ties
/// broken by declaration order for determinism.
///
/// # Errors
/// Returns `Err(remaining)` listing the steps that could not be ordered because they lie
/// on or after a dependency cycle.
pub fn topo_order(wf: &Workflow) -> Result<Vec<StepId>, Vec<StepId>> {
    let deps = dep_map(wf);
    let mut indeg: IndexMap<StepId, usize> =
        deps.iter().map(|(k, v)| (k.clone(), v.len())).collect();

    let mut dependents: IndexMap<StepId, Vec<StepId>> = wf
        .steps
        .iter()
        .map(|s| (s.id.clone(), Vec::new()))
        .collect();
    for (step, ds) in &deps {
        for d in ds {
            if let Some(list) = dependents.get_mut(d) {
                list.push(step.clone());
            }
        }
    }

    // Seed the queue in declaration order so the output is deterministic.
    let mut queue: Vec<StepId> = wf
        .steps
        .iter()
        .map(|s| s.id.clone())
        .filter(|id| indeg.get(id).copied() == Some(0))
        .collect();

    let mut order = Vec::with_capacity(wf.steps.len());
    let mut qi = 0;
    while qi < queue.len() {
        let n = queue[qi].clone();
        qi += 1;
        order.push(n.clone());
        for dep in &dependents[&n] {
            if let Some(e) = indeg.get_mut(dep) {
                *e -= 1;
                if *e == 0 {
                    queue.push(dep.clone());
                }
            }
        }
    }

    if order.len() == wf.steps.len() {
        Ok(order)
    } else {
        let remaining = indeg
            .iter()
            .filter(|&(_, &deg)| deg > 0)
            .map(|(k, _)| k.clone())
            .collect();
        Err(remaining)
    }
}

/// Finds one concrete dependency cycle, if any, as the ordered list of step ids that
/// form it (e.g. `[a, b, a]`). Used to render a helpful `ODIN014` message.
#[must_use]
pub fn find_cycle(wf: &Workflow) -> Option<Vec<StepId>> {
    let deps = dep_map(wf);
    // 0 = unvisited, 1 = on the current DFS stack, 2 = fully explored.
    let mut color: IndexMap<StepId, u8> = deps.keys().map(|k| (k.clone(), 0u8)).collect();
    let mut stack: Vec<StepId> = Vec::new();
    for start in deps.keys() {
        if color[start] == 0 {
            if let Some(cycle) = dfs_cycle(start, &deps, &mut color, &mut stack) {
                return Some(cycle);
            }
        }
    }
    None
}

fn dfs_cycle(
    node: &StepId,
    deps: &IndexMap<StepId, Vec<StepId>>,
    color: &mut IndexMap<StepId, u8>,
    stack: &mut Vec<StepId>,
) -> Option<Vec<StepId>> {
    color.insert(node.clone(), 1);
    stack.push(node.clone());
    for d in &deps[node] {
        if d == node {
            // A pure self-loop is reported by ODIN013 (SelfDependency); don't also
            // surface it as a generic ODIN014 cycle.
            continue;
        }
        match color.get(d).copied().unwrap_or(2) {
            1 => {
                // Back-edge: the cycle is the stack slice from `d` to the top, plus `d`.
                let pos = stack.iter().position(|x| x == d).unwrap_or(0);
                let mut cyc = stack[pos..].to_vec();
                cyc.push(d.clone());
                return Some(cyc);
            }
            0 => {
                if let Some(c) = dfs_cycle(d, deps, color, stack) {
                    return Some(c);
                }
            }
            _ => {}
        }
    }
    stack.pop();
    color.insert(node.clone(), 2);
    None
}

/// For each step, the set of all steps that are transitively upstream of it (its
/// ancestors via `depends_on`). Cycle-safe: a `seen` guard prevents infinite loops.
#[must_use]
pub fn ancestor_sets(wf: &Workflow) -> IndexMap<StepId, IndexSet<StepId>> {
    let deps = dep_map(wf);
    let mut out = IndexMap::with_capacity(wf.steps.len());
    for s in &wf.steps {
        let mut seen: IndexSet<StepId> = IndexSet::new();
        let mut frontier: Vec<StepId> = deps[&s.id].clone();
        while let Some(n) = frontier.pop() {
            if seen.insert(n.clone()) {
                if let Some(nd) = deps.get(&n) {
                    frontier.extend(nd.iter().cloned());
                }
            }
        }
        out.insert(s.id.clone(), seen);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{ancestor_sets, find_cycle, topo_order};
    use crate::ir::Workflow;

    fn wf(yaml: &str) -> Workflow {
        Workflow::from_yaml_str(yaml).unwrap()
    }

    #[test]
    fn orders_a_dag() {
        let w = wf(
            "name: t\nsteps:\n  - {id: c, run: x, depends_on: [a, b]}\n  - {id: a, run: x}\n  - {id: b, run: x, depends_on: [a]}\n",
        );
        let order = topo_order(&w).unwrap();
        let pos = |id: &str| order.iter().position(|s| s.as_str() == id).unwrap();
        assert!(pos("a") < pos("b"));
        assert!(pos("b") < pos("c"));
        assert!(find_cycle(&w).is_none());
    }

    #[test]
    fn detects_a_cycle() {
        let w = wf(
            "name: t\nsteps:\n  - {id: a, run: x, depends_on: [b]}\n  - {id: b, run: x, depends_on: [a]}\n",
        );
        assert!(topo_order(&w).is_err());
        let cyc = find_cycle(&w).expect("cycle");
        assert!(cyc.len() >= 2);
    }

    #[test]
    fn detects_a_three_node_cycle() {
        let w = wf(
            "name: t\nsteps:\n  - {id: a, run: x, depends_on: [c]}\n  - {id: b, run: x, depends_on: [a]}\n  - {id: c, run: x, depends_on: [b]}\n",
        );
        let cyc = find_cycle(&w).expect("cycle");
        assert!(cyc.len() >= 3, "got {cyc:?}");
    }

    #[test]
    fn self_loop_is_not_a_cycle() {
        // A self-loop is ODIN013's job; find_cycle must not report it as a cycle.
        let w = wf("name: t\nsteps:\n  - {id: a, run: x, depends_on: [a]}\n");
        assert!(find_cycle(&w).is_none());
        // It is still unschedulable, so topo_order reports it.
        assert!(topo_order(&w).is_err());
    }

    #[test]
    fn computes_transitive_ancestors() {
        let w = wf(
            "name: t\nsteps:\n  - {id: a, run: x}\n  - {id: b, run: x, depends_on: [a]}\n  - {id: c, run: x, depends_on: [b]}\n",
        );
        let anc = ancestor_sets(&w);
        let c = &anc[&crate::ids::StepId::new("c")];
        assert!(c.contains(&crate::ids::StepId::new("a")));
        assert!(c.contains(&crate::ids::StepId::new("b")));
    }
}
