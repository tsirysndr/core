use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};

use knot_types::{ChangeId, CobId, UnixSeconds};

use crate::change::Change;

#[derive(Debug)]
pub struct ChangeGraph {
    root: CobId,
    changes: BTreeMap<ChangeId, Change>,
}

impl ChangeGraph {
    pub(crate) fn new(root: CobId, changes: BTreeMap<ChangeId, Change>) -> Self {
        Self { root, changes }
    }

    pub fn root(&self) -> CobId {
        self.root
    }

    pub fn len(&self) -> usize {
        self.changes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    pub fn causal_order(&self) -> Vec<ChangeId> {
        order(&self.changes)
    }

    pub(crate) fn into_ordered(self) -> Vec<Change> {
        let ordered = order(&self.changes);
        let mut changes = self.changes;
        ordered
            .into_iter()
            .map(|id| changes.remove(&id).expect("ordered id is in graph"))
            .collect()
    }
}

fn order(changes: &BTreeMap<ChangeId, Change>) -> Vec<ChangeId> {
    let mut indegree: BTreeMap<ChangeId, usize> = changes
        .values()
        .map(|change| {
            let present = change
                .parents
                .iter()
                .filter(|parent| changes.contains_key(*parent))
                .count();
            (change.id, present)
        })
        .collect();
    let children: BTreeMap<ChangeId, Vec<ChangeId>> =
        changes.values().fold(BTreeMap::new(), |mut acc, change| {
            change
                .parents
                .iter()
                .filter(|parent| changes.contains_key(*parent))
                .for_each(|parent| acc.entry(*parent).or_default().push(change.id));
            acc
        });
    let mut ready: BinaryHeap<Reverse<(UnixSeconds, ChangeId)>> = changes
        .values()
        .filter(|change| indegree[&change.id] == 0)
        .map(|change| Reverse(change.sort_key()))
        .collect();
    std::iter::from_fn(move || {
        let Reverse((_, id)) = ready.pop()?;
        children.get(&id).into_iter().flatten().for_each(|child| {
            let degree = indegree
                .get_mut(child)
                .expect("every child has an indegree entry");
            *degree -= 1;
            if *degree == 0 {
                ready.push(Reverse(changes[child].sort_key()));
            }
        });
        Some(id)
    })
    .collect()
}

#[derive(Debug)]
pub struct History {
    root: ChangeId,
    changes: Vec<Change>,
}

impl History {
    pub(crate) fn new(root: ChangeId, changes: Vec<Change>) -> Self {
        Self { root, changes }
    }

    pub fn root(&self) -> ChangeId {
        self.root
    }

    pub fn changes(&self) -> &[Change] {
        &self.changes
    }

    pub fn len(&self) -> usize {
        self.changes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    pub fn traverse<A, F: FnMut(A, &Change) -> A>(&self, init: A, f: F) -> A {
        self.changes.iter().fold(init, f)
    }

    pub fn tips(&self) -> Vec<ChangeId> {
        let referenced: BTreeSet<ChangeId> = self
            .changes
            .iter()
            .flat_map(|change| change.parents.iter().copied())
            .collect();
        self.changes
            .iter()
            .map(|change| change.id)
            .filter(|id| !referenced.contains(id))
            .collect()
    }
}
