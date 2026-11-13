//! The snapshot graph and the reconcile decision.
//!
//! History is a DAG of immutable snapshots (`parents`). Divergence is *explicit*: two heads. This
//! module is deliberately free of I/O: [`decide`] maps (local base, dirty flag, cloud heads, graph)
//! to what must happen next, so every divergence case is unit-testable.
//!
//! Policy (see `docs/architecture.md`): **no automatic merging**. If the cloud and the local copy
//! both moved, or the cloud itself has several heads, the result is `Conflict` and both versions
//! stay available until a human chooses.

use std::collections::{HashMap, HashSet, VecDeque};

use serde::{Deserialize, Serialize};

use crate::db::{Db, SnapshotRow};
use crate::error::Result;
use crate::ids::{InstanceId, SnapshotId};
use crate::manifest::SnapshotKind;

#[derive(Debug, Clone)]
pub struct Node {
    pub parents: Vec<SnapshotId>,
    /// `false` when the parents are only a hint or unknown (manifest not read yet).
    pub parents_known: bool,
    pub kind: SnapshotKind,
    pub in_cloud: bool,
}

#[derive(Debug, Default, Clone)]
pub struct Graph {
    nodes: HashMap<SnapshotId, Node>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Relation {
    Yes,
    No,
    /// Could not be decided because some node on the way has unknown parents.
    Unknown,
}

impl Graph {
    pub fn from_rows(rows: &[SnapshotRow]) -> Graph {
        let mut g = Graph::default();
        for r in rows {
            g.nodes.insert(
                r.id,
                Node {
                    parents: r.parents.clone(),
                    parents_known: r.parents_known,
                    kind: r.kind,
                    in_cloud: r.state.is_in_cloud(),
                },
            );
        }
        g
    }

    pub fn load(db: &Db, inst: InstanceId) -> Result<Graph> {
        Ok(Graph::from_rows(&db.snapshots(inst)?))
    }

    pub fn insert(&mut self, id: SnapshotId, node: Node) {
        self.nodes.insert(id, node);
    }
    pub fn get(&self, id: SnapshotId) -> Option<&Node> {
        self.nodes.get(&id)
    }
    pub fn contains(&self, id: SnapshotId) -> bool {
        self.nodes.contains_key(&id)
    }

    /// Is `anc` an ancestor of (or equal to) `desc`?
    pub fn is_ancestor(&self, anc: SnapshotId, desc: SnapshotId) -> Relation {
        if anc == desc {
            return Relation::Yes;
        }
        let mut seen = HashSet::new();
        let mut q = VecDeque::from([desc]);
        let mut unknown = false;
        while let Some(n) = q.pop_front() {
            if !seen.insert(n) {
                continue;
            }
            match self.nodes.get(&n) {
                Some(node) => {
                    if !node.parents_known {
                        unknown = true;
                    }
                    for p in &node.parents {
                        if *p == anc {
                            return Relation::Yes;
                        }
                        q.push_back(*p);
                    }
                }
                None => unknown = true, // a parent we have no row for
            }
        }
        if unknown { Relation::Unknown } else { Relation::No }
    }

    pub fn ancestors(&self, id: SnapshotId) -> HashSet<SnapshotId> {
        let mut seen = HashSet::new();
        let mut q = VecDeque::from([id]);
        while let Some(n) = q.pop_front() {
            if let Some(node) = self.nodes.get(&n) {
                for p in &node.parents {
                    if seen.insert(*p) {
                        q.push_back(*p);
                    }
                }
            }
        }
        seen
    }

    /// Normal commits in the cloud that no other cloud commit builds on. Quarantined crash
    /// commits neither count as heads nor as children.
    pub fn cloud_heads(&self) -> Vec<SnapshotId> {
        let mut has_child: HashSet<SnapshotId> = HashSet::new();
        for n in self.nodes.values().filter(|n| n.in_cloud && n.kind == SnapshotKind::Normal) {
            has_child.extend(n.parents.iter().copied());
        }
        let mut heads: Vec<SnapshotId> = self
            .nodes
            .iter()
            .filter(|(id, n)| n.in_cloud && n.kind == SnapshotKind::Normal && !has_child.contains(*id))
            .map(|(id, _)| *id)
            .collect();
        heads.sort();
        heads
    }

    /// A normal cloud commit whose parents are not known yet. While one exists the set of heads is
    /// unreliable (that commit might be a child of another), so decisions must wait for its manifest.
    pub fn first_unknown_cloud_node(&self) -> Option<SnapshotId> {
        let mut v: Vec<SnapshotId> = self
            .nodes
            .iter()
            .filter(|(_, n)| n.in_cloud && n.kind == SnapshotKind::Normal && !n.parents_known)
            .map(|(id, _)| *id)
            .collect();
        v.sort();
        v.first().copied()
    }

    /// Quarantined crash saves in the cloud.
    pub fn cloud_crash_saves(&self) -> Vec<SnapshotId> {
        let mut v: Vec<SnapshotId> =
            self.nodes.iter().filter(|(_, n)| n.in_cloud && n.kind == SnapshotKind::Crash).map(|(id, _)| *id).collect();
        v.sort();
        v
    }

    /// Local snapshots that are not in the cloud, parents before children.
    pub fn unpublished_in_order(&self) -> Vec<SnapshotId> {
        let un: HashSet<SnapshotId> = self.nodes.iter().filter(|(_, n)| !n.in_cloud).map(|(id, _)| *id).collect();
        let mut out = vec![];
        let mut done: HashSet<SnapshotId> = HashSet::new();
        let mut remaining: Vec<SnapshotId> = un.iter().copied().collect();
        remaining.sort();
        while !remaining.is_empty() {
            let before = remaining.len();
            remaining.retain(|id| {
                let ready = self.nodes[id].parents.iter().all(|p| !un.contains(p) || done.contains(p));
                if ready {
                    out.push(*id);
                    done.insert(*id);
                }
                !ready
            });
            if remaining.len() == before {
                // cycle (corrupt data): emit the rest deterministically rather than loop forever
                out.append(&mut remaining);
            }
        }
        out
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictKind {
    /// Local state and the cloud both advanced from a common ancestor (or share none).
    Diverged,
    /// The cloud itself has more than one head (two devices published independently).
    MultipleCloudHeads,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Decision {
    /// Local base equals the single cloud head and there are no local changes.
    UpToDate {
        head: SnapshotId,
    },
    /// Nothing in the cloud for this instance yet.
    CloudEmpty,
    /// Local snapshots (and/or local changes that must be snapshotted first) are ahead of the cloud.
    LocalAhead,
    /// The cloud is ahead and the local copy is clean: adopt the cloud head.
    FastForward {
        to: SnapshotId,
    },
    Conflict {
        kind: ConflictKind,
        local: Option<SnapshotId>,
        local_dirty: bool,
        cloud_heads: Vec<SnapshotId>,
    },
    /// Ancestry could not be established from the data at hand; read manifests and ask again.
    NeedMoreHistory {
        of: SnapshotId,
    },
}

/// The pure reconcile decision. `base` is the snapshot the working tree last matched; `dirty`
/// says the working tree has changes that are not snapshotted yet.
pub fn decide(graph: &Graph, base: Option<SnapshotId>, dirty: bool) -> Decision {
    if let Some(of) = graph.first_unknown_cloud_node() {
        return Decision::NeedMoreHistory { of };
    }
    let heads = graph.cloud_heads();
    let Some(base) = base else {
        return if heads.is_empty() {
            Decision::CloudEmpty
        } else {
            // A local copy with no recorded base against a populated cloud: never overwrite.
            Decision::Conflict { kind: ConflictKind::Diverged, local: None, local_dirty: dirty, cloud_heads: heads }
        };
    };
    if heads.is_empty() {
        return Decision::LocalAhead;
    }
    if heads.len() > 1 {
        return Decision::Conflict {
            kind: ConflictKind::MultipleCloudHeads,
            local: Some(base),
            local_dirty: dirty,
            cloud_heads: heads,
        };
    }
    let h = heads[0];
    if base == h {
        return if dirty { Decision::LocalAhead } else { Decision::UpToDate { head: h } };
    }
    // Is the cloud head built on the local base (cloud ahead) or the base on the head (local ahead)?
    match graph.is_ancestor(base, h) {
        Relation::Yes => {
            return if dirty {
                Decision::Conflict {
                    kind: ConflictKind::Diverged,
                    local: Some(base),
                    local_dirty: true,
                    cloud_heads: heads,
                }
            } else {
                Decision::FastForward { to: h }
            };
        }
        Relation::Unknown => return Decision::NeedMoreHistory { of: h },
        Relation::No => {}
    }
    match graph.is_ancestor(h, base) {
        Relation::Yes => Decision::LocalAhead,
        Relation::Unknown => Decision::NeedMoreHistory { of: base },
        Relation::No => Decision::Conflict {
            kind: ConflictKind::Diverged,
            local: Some(base),
            local_dirty: dirty,
            cloud_heads: heads,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::Hash;

    fn id(n: u8) -> SnapshotId {
        SnapshotId(Hash([n; 32]))
    }
    fn node(parents: &[u8], in_cloud: bool) -> Node {
        Node {
            parents: parents.iter().map(|p| id(*p)).collect(),
            parents_known: true,
            kind: SnapshotKind::Normal,
            in_cloud,
        }
    }
    fn graph(nodes: &[(u8, &[u8], bool)]) -> Graph {
        let mut g = Graph::default();
        for (n, p, c) in nodes {
            g.insert(id(*n), node(p, *c));
        }
        g
    }

    #[test]
    fn heads_ancestry_and_crash_quarantine() {
        // 1 ← 2 ← 3 ; 2 ← 4 (second head) ; 5 is a crash save on top of 3
        let mut g = graph(&[(1, &[], true), (2, &[1], true), (3, &[2], true), (4, &[2], true)]);
        g.insert(id(5), Node { parents: vec![id(3)], parents_known: true, kind: SnapshotKind::Crash, in_cloud: true });
        assert_eq!(g.cloud_heads(), vec![id(3), id(4)], "a crash child does not make 3 a non-head");
        assert_eq!(g.cloud_crash_saves(), vec![id(5)]);
        assert_eq!(g.is_ancestor(id(1), id(3)), Relation::Yes);
        assert_eq!(g.is_ancestor(id(3), id(1)), Relation::No);
        assert_eq!(g.is_ancestor(id(3), id(4)), Relation::No);
        assert_eq!(g.is_ancestor(id(3), id(3)), Relation::Yes);
        assert_eq!(g.ancestors(id(4)), HashSet::from([id(1), id(2)]));
    }

    #[test]
    fn unknown_parents_make_ancestry_undecidable_not_false() {
        let mut g = graph(&[(1, &[], true)]);
        g.insert(id(2), Node { parents: vec![], parents_known: false, kind: SnapshotKind::Normal, in_cloud: true });
        assert_eq!(g.is_ancestor(id(1), id(2)), Relation::Unknown);
        let g2 = graph(&[(2, &[9], true)]); // parent 9 has no row at all
        assert_eq!(g2.is_ancestor(id(1), id(2)), Relation::Unknown);
    }

    #[test]
    fn unpublished_snapshots_come_parents_first() {
        let g = graph(&[(1, &[], true), (2, &[1], false), (3, &[2], false), (4, &[3, 2], false)]);
        let order = g.unpublished_in_order();
        let pos = |n: u8| order.iter().position(|x| *x == id(n)).unwrap();
        assert!(pos(2) < pos(3) && pos(3) < pos(4));
        assert_eq!(order.len(), 3);
    }

    #[test]
    fn decision_table() {
        let up = |g: &Graph, base: u8, dirty: bool| decide(g, Some(id(base)), dirty);
        // cloud: 1 ← 2
        let g = graph(&[(1, &[], true), (2, &[1], true)]);
        assert_eq!(up(&g, 2, false), Decision::UpToDate { head: id(2) });
        assert_eq!(up(&g, 2, true), Decision::LocalAhead, "local changes on top of the head are simply ahead");
        assert_eq!(up(&g, 1, false), Decision::FastForward { to: id(2) });
        assert_eq!(
            up(&g, 1, true),
            Decision::Conflict {
                kind: ConflictKind::Diverged,
                local: Some(id(1)),
                local_dirty: true,
                cloud_heads: vec![id(2)]
            },
            "dirty local + cloud advanced is NEVER auto-resolved"
        );

        // local snapshot 3 (unpublished) on top of cloud head 2
        let g = graph(&[(1, &[], true), (2, &[1], true), (3, &[2], false)]);
        assert_eq!(up(&g, 3, false), Decision::LocalAhead);

        // offline play: local 3 built on 1, meanwhile another device published 2
        let g = graph(&[(1, &[], true), (2, &[1], true), (3, &[1], false)]);
        assert_eq!(
            up(&g, 3, false),
            Decision::Conflict {
                kind: ConflictKind::Diverged,
                local: Some(id(3)),
                local_dirty: false,
                cloud_heads: vec![id(2)]
            }
        );

        // two cloud heads (simultaneous publication from one parent)
        let g = graph(&[(1, &[], true), (2, &[1], true), (3, &[1], true)]);
        match up(&g, 2, false) {
            Decision::Conflict { kind: ConflictKind::MultipleCloudHeads, cloud_heads, .. } => {
                assert_eq!(cloud_heads, vec![id(2), id(3)])
            }
            other => panic!("{other:?}"),
        }
        // after a merge commit the divergence is closed
        let g = graph(&[(1, &[], true), (2, &[1], true), (3, &[1], true), (4, &[2, 3], true)]);
        assert_eq!(up(&g, 2, false), Decision::FastForward { to: id(4) });
        assert_eq!(up(&g, 4, false), Decision::UpToDate { head: id(4) });

        // empty cloud
        let g = graph(&[(1, &[], false)]);
        assert_eq!(up(&g, 1, false), Decision::LocalAhead);
        assert_eq!(decide(&Graph::default(), None, false), Decision::CloudEmpty);
        // no base vs populated cloud must not overwrite anything
        let g = graph(&[(1, &[], true)]);
        assert!(matches!(decide(&g, None, false), Decision::Conflict { .. }));
    }

    #[test]
    fn undecidable_ancestry_asks_for_more_history() {
        let mut g = graph(&[(1, &[], true)]);
        g.insert(id(2), Node { parents: vec![], parents_known: false, kind: SnapshotKind::Normal, in_cloud: true });
        assert_eq!(decide(&g, Some(id(1)), false), Decision::NeedMoreHistory { of: id(2) });
    }

    #[test]
    fn disjoint_histories_are_conflicts() {
        let g = graph(&[(1, &[], true), (2, &[], false)]);
        assert!(matches!(decide(&g, Some(id(2)), false), Decision::Conflict { kind: ConflictKind::Diverged, .. }));
    }
}
