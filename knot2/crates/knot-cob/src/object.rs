use knot_types::{ActorId, ChangeId, CobId, TypeName};

use crate::change::{Change, ChangePayload};
use crate::error::CobError;
use crate::graph::{ChangeGraph, History};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryModel {
    Linear,
    Convergent,
}

pub trait Evaluate {
    type State;
    type Change: ChangePayload;

    const HISTORY: HistoryModel;

    fn initial() -> Self::State;
    fn apply(state: Self::State, change: Self::Change, author: &ActorId) -> Self::State;
}

knot_types::scalar_newtype! {
    pub struct SnapshotStride(usize);
    pub struct StateSize(usize);
}

pub trait Checkpoint: Evaluate {
    const SNAPSHOT_STRIDE: SnapshotStride;
    fn checkpoint_size(state: &Self::State) -> StateSize;
}

pub(crate) fn fold_changes<E: Evaluate>(
    state: E::State,
    changes: &[Change],
    expected: &TypeName,
) -> Result<E::State, CobError> {
    changes.iter().try_fold(state, |state, change| {
        if change.type_name != *expected {
            return Err(CobError::UnexpectedChangeType {
                change: change.id,
                expected: expected.clone(),
                found: change.type_name.clone(),
            });
        }
        let payload =
            E::Change::decode(change.payload()).map_err(|error| CobError::UndecodableChange {
                change: change.id,
                reason: error.to_string(),
            })?;
        Ok(E::apply(state, payload, &change.author))
    })
}

pub(crate) fn evaluate<E: Evaluate>(
    graph: ChangeGraph,
    expected: &TypeName,
) -> Result<(E::State, History), CobError> {
    let root = ChangeId::new(graph.root().oid());
    let ordered = graph.into_ordered();
    let state = fold_changes::<E>(E::initial(), &ordered, expected)?;
    Ok((state, History::new(root, ordered)))
}

#[derive(Debug)]
pub struct Object<S> {
    id: CobId,
    type_name: TypeName,
    state: S,
    history: History,
}

impl<S> Object<S> {
    pub(crate) fn new(id: CobId, type_name: TypeName, state: S, history: History) -> Self {
        Self {
            id,
            type_name,
            state,
            history,
        }
    }

    pub fn id(&self) -> CobId {
        self.id
    }

    pub fn type_name(&self) -> &TypeName {
        &self.type_name
    }

    pub fn state(&self) -> &S {
        &self.state
    }

    pub fn into_state(self) -> S {
        self.state
    }

    pub fn history(&self) -> &History {
        &self.history
    }
}
