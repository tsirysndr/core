mod backend;
mod change;
mod error;
mod graph;
#[cfg(feature = "instrument")]
pub mod instrument;
mod object;

pub use change::{Change, ChangePayload, CobHome, Payload};
pub use error::{CobError, PayloadError};
pub use graph::{ChangeGraph, History};
pub use knot_types::{ActorId, ChangeId, CobId, TypeName};
pub use object::{Checkpoint, Evaluate, HistoryModel, Object, SnapshotStride, StateSize};

pub use backend::parse_cob_ref;

use knot_git::{RefUpdate, Repo};
use knot_runtime::Signer;
use knot_types::{Oid, UnixSeconds};
use serde::Serialize;
use serde::de::DeserializeOwned;

const MAX_CAS_RETRIES: usize = 16;
const CHECKPOINT_GROWTH_DIVISOR: usize = 16;
// don't ask

fn checkpoint_stride(snapshot_stride: SnapshotStride, size: StateSize) -> usize {
    snapshot_stride
        .get()
        .max(size.get() / CHECKPOINT_GROWTH_DIVISOR)
        .min(backend::MAX_GRAPH_CHANGES)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Created {
    pub object: CobId,
    pub tip: ChangeId,
}

#[derive(Debug)]
pub struct Delta {
    pub changes: Vec<Change>,
    pub tip: ChangeId,
}

const CHECKPOINT_FORMAT: u16 = 3;

#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct CheckpointDigest(Oid);

fn checkpoint_digest<S: Serialize>(
    object: CobId,
    tip: ChangeId,
    state: &S,
) -> Result<CheckpointDigest, CobError> {
    let bytes = serde_ipld_dagcbor::to_vec(&(object, tip, state))
        .map_err(|error| CobError::Write(error.to_string()))?;
    let mut hasher = gix_hash::hasher(gix_hash::Kind::Sha1);
    hasher.update(&bytes);
    hasher
        .try_finalize()
        .map(|id| CheckpointDigest(Oid::from(id)))
        .map_err(|error| CobError::Write(error.to_string()))
}

fn encode_checkpoint<S: Serialize>(
    object: CobId,
    tip: ChangeId,
    state: &S,
) -> Result<Vec<u8>, CobError> {
    let digest = checkpoint_digest(object, tip, state)?;
    serde_ipld_dagcbor::to_vec(&(CHECKPOINT_FORMAT, object, tip, digest, state))
        .map_err(|error| CobError::Write(error.to_string()))
}

fn decode_checkpoint<S: DeserializeOwned + Serialize>(
    object: CobId,
    bytes: &[u8],
) -> Option<(ChangeId, S)> {
    let (version, decoded_object, tip, digest, state): (u16, CobId, ChangeId, CheckpointDigest, S) =
        serde_ipld_dagcbor::from_slice(bytes).ok()?;
    (version == CHECKPOINT_FORMAT).then_some(())?;
    (decoded_object == object).then_some(())?;
    (checkpoint_digest(object, tip, &state).ok()? == digest).then_some(())?;
    Some((tip, state))
}

pub struct CobStore<'r> {
    repo: &'r Repo,
}

impl<'r> CobStore<'r> {
    pub fn new(repo: &'r Repo) -> Self {
        Self { repo }
    }

    pub fn create<P: ChangePayload>(
        &self,
        home: &CobHome,
        payload: &P,
        signer: &dyn Signer,
        timestamp: UnixSeconds,
    ) -> Result<Created, CobError> {
        let type_name = P::type_name();
        let bytes = payload.encode()?;
        let tip = backend::write_change(
            home,
            self.repo,
            &type_name,
            &bytes,
            &[],
            None,
            signer,
            timestamp,
        )?;
        let object = CobId::new(tip.oid());
        let name = backend::cob_ref_name(&type_name, object)?;
        self.repo.update_ref(&RefUpdate::Create {
            name,
            new: tip.oid(),
        })?;
        Ok(Created { object, tip })
    }

    pub fn update<P: ChangePayload>(
        &self,
        home: &CobHome,
        object: CobId,
        payload: &P,
        signer: &dyn Signer,
        timestamp: UnixSeconds,
    ) -> Result<ChangeId, CobError> {
        let type_name = P::type_name();
        let expected = backend::resolve_tip(self.repo, &type_name, object)?
            .map(ChangeId::new)
            .ok_or(CobError::NoSuchObject(object))?;
        self.append(
            home, object, &type_name, expected, payload, signer, timestamp,
        )
    }

    pub fn extend<'p, P: ChangePayload + 'p>(
        &self,
        home: &CobHome,
        object: CobId,
        changes: impl IntoIterator<Item = (&'p P, UnixSeconds)>,
        signer: &dyn Signer,
    ) -> Result<Option<ChangeId>, CobError> {
        let type_name = P::type_name();
        let expected = backend::resolve_tip(self.repo, &type_name, object)?
            .map(ChangeId::new)
            .ok_or(CobError::NoSuchObject(object))?;
        self.chain(home, object, &type_name, expected, changes, signer)
    }

    #[allow(clippy::too_many_arguments)]
    fn append<P: ChangePayload>(
        &self,
        home: &CobHome,
        object: CobId,
        type_name: &TypeName,
        expected: ChangeId,
        payload: &P,
        signer: &dyn Signer,
        timestamp: UnixSeconds,
    ) -> Result<ChangeId, CobError> {
        self.chain(
            home,
            object,
            type_name,
            expected,
            std::iter::once((payload, timestamp)),
            signer,
        )
        .map(|tip| tip.expect("one change chains onto one tip"))
    }

    fn chain<'p, P: ChangePayload + 'p>(
        &self,
        home: &CobHome,
        object: CobId,
        type_name: &TypeName,
        expected: ChangeId,
        changes: impl IntoIterator<Item = (&'p P, UnixSeconds)>,
        signer: &dyn Signer,
    ) -> Result<Option<ChangeId>, CobError> {
        let written = match changes.into_iter().try_fold(
            Vec::<ChangeId>::new(),
            |mut written, (payload, timestamp)| {
                let parent = written.last().copied().unwrap_or(expected);
                let write = || -> Result<ChangeId, CobError> {
                    let bytes = payload.encode()?;
                    backend::write_change(
                        home,
                        self.repo,
                        type_name,
                        &bytes,
                        &[parent],
                        Some(object),
                        signer,
                        timestamp,
                    )
                };
                match write() {
                    Ok(tip) => {
                        written.push(tip);
                        Ok(written)
                    }
                    Err(error) => Err((written, error)),
                }
            },
        ) {
            Ok(written) => written,
            Err((partial, error)) => {
                partial
                    .iter()
                    .try_for_each(|change| self.repo.remove_loose_object(change.oid()))?;
                return Err(error);
            }
        };
        written.last().copied().map_or(Ok(None), |tip| {
            self.publish(type_name, object, expected, tip, &written)
                .map(Some)
        })
    }

    fn publish(
        &self,
        type_name: &TypeName,
        object: CobId,
        expected: ChangeId,
        tip: ChangeId,
        written: &[ChangeId],
    ) -> Result<ChangeId, CobError> {
        let name = backend::cob_ref_name(type_name, object)?;
        match self.repo.update_ref(&RefUpdate::Update {
            name,
            old: expected.oid(),
            new: tip.oid(),
        }) {
            Ok(()) => Ok(tip),
            Err(error) => {
                let actual = backend::resolve_tip(self.repo, type_name, object)?;
                if actual != Some(tip.oid()) {
                    written
                        .iter()
                        .try_for_each(|change| self.repo.remove_loose_object(change.oid()))?;
                }
                match actual {
                    actual if actual != Some(expected.oid()) => {
                        Err(CobError::StaleTip { object, expected })
                    }
                    _ => Err(CobError::Git(error)),
                }
            }
        }
    }

    pub fn update_with<E, D>(
        &self,
        home: &CobHome,
        object: CobId,
        signer: &dyn Signer,
        timestamp: UnixSeconds,
        decide: impl Fn(&E::State) -> Result<E::Change, D>,
    ) -> Result<ChangeId, D>
    where
        E: Evaluate,
        D: From<CobError>,
    {
        self.update_maybe::<E, D>(home, object, signer, timestamp, |state| {
            decide(state).map(Some)
        })
        .map(|tip| tip.expect("update_with always yields change to append"))
    }

    pub fn update_maybe<E, D>(
        &self,
        home: &CobHome,
        object: CobId,
        signer: &dyn Signer,
        timestamp: UnixSeconds,
        decide: impl Fn(&E::State) -> Result<Option<E::Change>, D>,
    ) -> Result<Option<ChangeId>, D>
    where
        E: Evaluate,
        D: From<CobError>,
    {
        let type_name = E::Change::type_name();
        let attempt = || -> Result<Option<Option<ChangeId>>, D> {
            let (graph, expected) = backend::load_graph(
                self.repo,
                &type_name,
                object,
                E::HISTORY,
                backend::MAX_GRAPH_CHANGES,
            )?;
            let (state, _history) = object::evaluate::<E>(graph, &type_name)?;
            match decide(&state)? {
                None => Ok(Some(None)),
                Some(change) => {
                    match self.append(
                        home, object, &type_name, expected, &change, signer, timestamp,
                    ) {
                        Ok(id) => Ok(Some(Some(id))),
                        Err(CobError::StaleTip { .. }) => Ok(None),
                        Err(other) => Err(D::from(other)),
                    }
                }
            }
        };
        (0..MAX_CAS_RETRIES)
            .find_map(|_| attempt().transpose())
            .unwrap_or_else(|| Err(D::from(CobError::Contended(object))))
    }

    pub fn graph<E: Evaluate>(&self, object: CobId) -> Result<ChangeGraph, CobError> {
        Ok(backend::load_graph(
            self.repo,
            &E::Change::type_name(),
            object,
            E::HISTORY,
            backend::MAX_GRAPH_CHANGES,
        )?
        .0)
    }

    pub fn get<E: Evaluate>(&self, object: CobId) -> Result<Object<E::State>, CobError> {
        let type_name = E::Change::type_name();
        let (graph, _tip) = backend::load_graph(
            self.repo,
            &type_name,
            object,
            E::HISTORY,
            backend::MAX_GRAPH_CHANGES,
        )?;
        let (state, history) = object::evaluate::<E>(graph, &type_name)?;
        Ok(Object::new(object, type_name, state, history))
    }

    pub fn verify<E: Evaluate>(
        &self,
        home: &CobHome,
        object: CobId,
        owner: &ActorId,
    ) -> Result<(), CobError> {
        let type_name = E::Change::type_name();
        let (graph, _tip) = backend::load_graph(
            self.repo,
            &type_name,
            object,
            E::HISTORY,
            backend::MAX_GRAPH_CHANGES,
        )?;
        graph.into_ordered().into_iter().try_for_each(|change| {
            if change.type_name != type_name {
                return Err(CobError::UnexpectedChangeType {
                    change: change.id,
                    expected: type_name.clone(),
                    found: change.type_name,
                });
            }
            change
                .verify(home, owner, Some(object))
                .then_some(())
                .ok_or(CobError::UnverifiedChange { change: change.id })
        })
    }

    pub fn list<E: Evaluate>(&self) -> Result<Vec<CobId>, CobError> {
        backend::list_objects(self.repo, &E::Change::type_name())
    }

    pub fn changes_since<E: Evaluate>(
        &self,
        object: CobId,
        since: Option<ChangeId>,
    ) -> Result<Delta, CobError> {
        let type_name = E::Change::type_name();
        let tip = backend::resolve_tip(self.repo, &type_name, object)?
            .map(ChangeId::new)
            .ok_or(CobError::NoSuchObject(object))?;
        let collected =
            backend::collect(self.repo, tip, object, backend::MAX_GRAPH_CHANGES, since)?;
        match since {
            None => backend::check_full_shape(&collected, object, E::HISTORY)?,
            Some(since) => backend::check_delta_shape(&collected, object, since, E::HISTORY)?,
        }
        let changes = ChangeGraph::new(object, collected).into_ordered();
        Ok(Delta { changes, tip })
    }

    pub fn update_with_checkpointed<E, D>(
        &self,
        home: &CobHome,
        object: CobId,
        signer: &dyn Signer,
        timestamp: UnixSeconds,
        decide: impl Fn(&E::State) -> Result<E::Change, D>,
    ) -> Result<ChangeId, D>
    where
        E: Checkpoint,
        E::State: Serialize + DeserializeOwned,
        D: From<CobError>,
    {
        self.update_maybe_checkpointed::<E, D>(home, object, signer, timestamp, |state| {
            decide(state).map(Some)
        })
        .map(|tip| tip.expect("update_with_checkpointed always yields change to append"))
    }

    pub fn update_maybe_checkpointed<E, D>(
        &self,
        home: &CobHome,
        object: CobId,
        signer: &dyn Signer,
        timestamp: UnixSeconds,
        decide: impl Fn(&E::State) -> Result<Option<E::Change>, D>,
    ) -> Result<Option<ChangeId>, D>
    where
        E: Checkpoint,
        E::State: Serialize + DeserializeOwned,
        D: From<CobError>,
    {
        let type_name = E::Change::type_name();
        let attempt = || -> Result<Option<Option<ChangeId>>, D> {
            let (state, expected, suffix) = self.checkpointed_state::<E>(object)?;
            match decide(&state)? {
                None => Ok(Some(None)),
                Some(change) => match self.append(
                    home, object, &type_name, expected, &change, signer, timestamp,
                ) {
                    Ok(tip) => {
                        let stride =
                            checkpoint_stride(E::SNAPSHOT_STRIDE, E::checkpoint_size(&state));
                        if suffix.saturating_add(1) >= stride {
                            let author = ActorId::from_secp256k1(signer.public_key().as_bytes());
                            let folded = E::apply(state, change, &author);
                            if let Err(error) = self.write_checkpoint::<E>(object, tip, &folded) {
                                tracing::warn!(
                                    cob = type_name.as_str(),
                                    object = %object.oid().to_hex(),
                                    %error,
                                    "checkpoint write failed"
                                );
                            }
                        }
                        Ok(Some(Some(tip)))
                    }
                    Err(CobError::StaleTip { .. }) => Ok(None),
                    Err(other) => Err(D::from(other)),
                },
            }
        };
        (0..MAX_CAS_RETRIES)
            .find_map(|_| attempt().transpose())
            .unwrap_or_else(|| Err(D::from(CobError::Contended(object))))
    }

    pub fn materialize<E>(&self, object: CobId) -> Result<(E::State, ChangeId), CobError>
    where
        E: Checkpoint,
        E::State: Serialize + DeserializeOwned,
    {
        self.checkpointed_state::<E>(object)
            .map(|(state, tip, _)| (state, tip))
    }

    fn checkpointed_state<E>(&self, object: CobId) -> Result<(E::State, ChangeId, usize), CobError>
    where
        E: Checkpoint,
        E::State: Serialize + DeserializeOwned,
    {
        let type_name = E::Change::type_name();
        let tip = backend::resolve_tip(self.repo, &type_name, object)?
            .map(ChangeId::new)
            .ok_or(CobError::NoSuchObject(object))?;
        match self.load_checkpoint::<E>(object)? {
            Some((checkpoint_tip, state)) if checkpoint_tip == tip => Ok((state, tip, 0)),
            Some((checkpoint_tip, state)) => {
                match self.changes_since::<E>(object, Some(checkpoint_tip)) {
                    Ok(delta) => {
                        let folded = object::fold_changes::<E>(state, &delta.changes, &type_name)?;
                        Ok((folded, tip, delta.changes.len()))
                    }
                    Err(_) => self
                        .full_state::<E>(object)
                        .map(|state| (state, tip, usize::MAX)),
                }
            }
            None => self
                .full_state::<E>(object)
                .map(|state| (state, tip, usize::MAX)),
        }
    }

    fn full_state<E: Evaluate>(&self, object: CobId) -> Result<E::State, CobError> {
        let type_name = E::Change::type_name();
        let (graph, _tip) = backend::load_graph(
            self.repo,
            &type_name,
            object,
            E::HISTORY,
            backend::rebuild_graph_limit(),
        )?;
        object::evaluate::<E>(graph, &type_name).map(|(state, _history)| state)
    }

    fn load_checkpoint<E>(&self, object: CobId) -> Result<Option<(ChangeId, E::State)>, CobError>
    where
        E: Checkpoint,
        E::State: DeserializeOwned + Serialize,
    {
        let name = backend::checkpoint_ref_name(&E::Change::type_name(), object)?;
        let Some(blob) = self.repo.find_ref(&name)? else {
            return Ok(None);
        };
        let Ok(bytes) = self.repo.read_blob(blob) else {
            return Ok(None);
        };
        Ok(decode_checkpoint::<E::State>(object, &bytes))
    }

    fn write_checkpoint<E>(
        &self,
        object: CobId,
        tip: ChangeId,
        state: &E::State,
    ) -> Result<(), CobError>
    where
        E: Checkpoint,
        E::State: Serialize,
    {
        let bytes = encode_checkpoint(object, tip, state)?;
        let blob = Oid::from(
            self.repo
                .git()
                .write_blob(&bytes)
                .map_err(|error| CobError::Write(error.to_string()))?
                .detach(),
        );
        let name = backend::checkpoint_ref_name(&E::Change::type_name(), object)?;
        let update = match self.repo.find_ref(&name)? {
            Some(old) => RefUpdate::Update {
                name,
                old,
                new: blob,
            },
            None => RefUpdate::Create { name, new: blob },
        };
        self.repo.update_ref(&update).map_err(CobError::from)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use knot_git::{Layout, Repo};
    use knot_runtime::{K256Signer, SeededEntropy, Signature, Signer};
    use knot_types::{RepoDid, UnixSeconds};
    use proptest::prelude::*;
    use serde::{Deserialize, Serialize};
    use tempfile::TempDir;

    use super::*;

    #[derive(Debug, Serialize, Deserialize)]
    #[serde(tag = "op", content = "subject")]
    enum Tag {
        Add(String),
        Remove(String),
    }

    impl ChangePayload for Tag {
        const TYPE: &'static str = "sh.tangled.test.tag";
    }

    #[test]
    fn checkpoint_encoding_stays_byte_stable() {
        let object = CobId::new(Oid::from_hex("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap());
        let tip = ChangeId::new(Oid::from_hex("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").unwrap());
        let mut state = BTreeSet::new();
        state.insert("kelp".to_string());
        state.insert("squid".to_string());
        let bytes = encode_checkpoint(object, tip, &state).unwrap();
        assert_eq!(
            knot_types::lowercase_hex(&bytes),
            "850378286161616161616161616161616161616161616161616161616161616161616161616161616161616178286262626262626262626262626262626262626262626262626262626262626262626262626262626278283861623531376164633234616530313138383962643634343930663836633562646333333632333782646b656c70657371756964"
        );
    }

    struct Tags;

    impl Evaluate for Tags {
        type State = BTreeSet<String>;
        type Change = Tag;

        const HISTORY: HistoryModel = HistoryModel::Convergent;

        fn initial() -> Self::State {
            BTreeSet::new()
        }

        fn apply(mut state: Self::State, change: Self::Change, _author: &ActorId) -> Self::State {
            match change {
                Tag::Add(subject) => {
                    state.insert(subject);
                }
                Tag::Remove(subject) => {
                    state.remove(&subject);
                }
            }
            state
        }
    }

    #[derive(Debug, Serialize, Deserialize)]
    #[serde(tag = "op", content = "subject")]
    enum NameChange {
        Claim(String),
    }

    impl ChangePayload for NameChange {
        const TYPE: &'static str = "sh.tangled.test.name";
    }

    struct Names;

    impl Evaluate for Names {
        type State = BTreeSet<String>;
        type Change = NameChange;

        const HISTORY: HistoryModel = HistoryModel::Linear;

        fn initial() -> Self::State {
            BTreeSet::new()
        }

        fn apply(mut state: Self::State, change: Self::Change, _author: &ActorId) -> Self::State {
            match change {
                NameChange::Claim(subject) => {
                    state.insert(subject);
                }
            }
            state
        }
    }

    impl Checkpoint for Names {
        const SNAPSHOT_STRIDE: SnapshotStride = SnapshotStride::new(4);
        fn checkpoint_size(state: &Self::State) -> StateSize {
            StateSize::new(state.len())
        }
    }

    #[derive(Debug)]
    enum NameError {
        Taken,
        Cob(CobError),
    }

    impl From<CobError> for NameError {
        fn from(error: CobError) -> Self {
            NameError::Cob(error)
        }
    }

    fn claim(
        store: &CobStore,
        object: CobId,
        key: &K256Signer,
        who: &str,
        when: i64,
    ) -> Result<Option<ChangeId>, NameError> {
        store.update_maybe_checkpointed::<Names, NameError>(
            &cob_home(),
            object,
            key,
            at(when),
            |state| match state.contains(who) {
                true => Err(NameError::Taken),
                false => Ok(Some(NameChange::Claim(who.to_string()))),
            },
        )
    }

    fn appended(result: Result<Option<ChangeId>, NameError>) -> ChangeId {
        match result {
            Ok(Some(id)) => id,
            Ok(None) => panic!("distinct claim should append a change"),
            Err(NameError::Taken) => panic!("name was unexpectedly already taken"),
            Err(NameError::Cob(error)) => panic!("checkpointed write failed: {error:?}"),
        }
    }

    fn fixture() -> (TempDir, Repo) {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path());
        let repo = layout
            .create(&RepoDid::new("did:plc:squid").unwrap())
            .unwrap();
        (dir, repo)
    }

    fn signer(seed: u64) -> K256Signer {
        K256Signer::generate(&SeededEntropy::new(seed))
    }

    fn cob_home() -> CobHome {
        CobHome::from(&RepoDid::new("did:plc:squid").unwrap())
    }

    fn at(seconds: i64) -> UnixSeconds {
        UnixSeconds::new(seconds)
    }

    fn tag_root(repo: &Repo, key: &K256Signer, subject: &str, when: i64) -> ChangeId {
        backend::write_change(
            &cob_home(),
            repo,
            &Tag::type_name(),
            &Tag::Add(subject.into()).encode().unwrap(),
            &[],
            None,
            key,
            at(when),
        )
        .unwrap()
    }

    fn tag_child(
        repo: &Repo,
        key: &K256Signer,
        subject: &str,
        parents: &[ChangeId],
        object: CobId,
        when: i64,
    ) -> ChangeId {
        backend::write_change(
            &cob_home(),
            repo,
            &Tag::type_name(),
            &Tag::Add(subject.into()).encode().unwrap(),
            parents,
            Some(object),
            key,
            at(when),
        )
        .unwrap()
    }

    fn publish(repo: &Repo, object: CobId, tip: ChangeId) {
        let name = backend::cob_ref_name(&Tag::type_name(), object).unwrap();
        repo.update_ref(&RefUpdate::Create {
            name,
            new: tip.oid(),
        })
        .unwrap();
    }

    fn forked_tag_object(
        repo: &Repo,
        key: &K256Signer,
        left: &str,
        right: &str,
        merge: &str,
    ) -> CobId {
        let root = tag_root(repo, key, "base", 1);
        let object = CobId::new(root.oid());
        let left = tag_child(repo, key, left, &[root], object, 2);
        let right = tag_child(repo, key, right, &[root], object, 3);
        let merge = tag_child(repo, key, merge, &[left, right], object, 4);
        publish(repo, object, merge);
        object
    }

    fn linear_chain(repo: &Repo, key: &K256Signer, subjects: &[&str]) -> CobId {
        let (first, rest) = subjects.split_first().expect("chain needs a root subject");
        let root = tag_root(repo, key, first, 1);
        let object = CobId::new(root.oid());
        let tip = rest
            .iter()
            .enumerate()
            .fold(root, |parent, (index, subject)| {
                tag_child(repo, key, subject, &[parent], object, index as i64 + 2)
            });
        publish(repo, object, tip);
        object
    }

    #[test]
    fn extend_chains_the_same_tip_as_sequential_updates() {
        let (_sequential_dir, sequential_repo) = fixture();
        let (_batched_dir, batched_repo) = fixture();
        let key = signer(3);
        let changes = [
            NameChange::Claim("kelp".into()),
            NameChange::Claim("squid".into()),
            NameChange::Claim("whelk".into()),
        ];
        let sequential = CobStore::new(&sequential_repo);
        let batched = CobStore::new(&batched_repo);
        let root = |store: &CobStore| {
            store
                .create(&cob_home(), &NameChange::Claim("uni".into()), &key, at(0))
                .unwrap()
                .object
        };
        let one_by_one = root(&sequential);
        changes.iter().zip(1..).for_each(|(change, when)| {
            sequential
                .update(&cob_home(), one_by_one, change, &key, at(when))
                .unwrap();
        });
        let in_one_go = root(&batched);
        let tip = batched
            .extend(
                &cob_home(),
                in_one_go,
                changes.iter().zip((1..).map(at)),
                &key,
            )
            .unwrap();

        let resolved = |repo: &Repo, object| {
            backend::resolve_tip(repo, &NameChange::type_name(), object)
                .unwrap()
                .map(ChangeId::new)
        };
        assert_eq!(one_by_one, in_one_go);
        assert_eq!(tip, resolved(&batched_repo, in_one_go));
        assert_eq!(tip, resolved(&sequential_repo, one_by_one));
        assert_eq!(
            sequential.materialize::<Names>(one_by_one).unwrap().0,
            batched.materialize::<Names>(in_one_go).unwrap().0
        );
        assert_eq!(
            batched
                .extend::<NameChange>(&cob_home(), in_one_go, std::iter::empty(), &key)
                .unwrap(),
            None
        );
        assert_eq!(tip, resolved(&batched_repo, in_one_go));
    }

    #[test]
    fn checkpointed_writes_match_a_full_fold_and_leave_a_snapshot() {
        let (_dir, repo) = fixture();
        let store = CobStore::new(&repo);
        let key = signer(1);
        let object = store
            .create(
                &cob_home(),
                &NameChange::Claim("name0000".into()),
                &key,
                at(0),
            )
            .unwrap()
            .object;

        let total = Names::SNAPSHOT_STRIDE.get() * 3;
        (1..total).for_each(|index| {
            appended(claim(
                &store,
                object,
                &key,
                &format!("name{index:04}"),
                index as i64,
            ));
        });

        let folded = store.get::<Names>(object).unwrap();
        assert_eq!(
            folded.state().len(),
            total,
            "every checkpointed write landed and full fold agrees"
        );
        let snapshot = backend::checkpoint_ref_name(&NameChange::type_name(), object).unwrap();
        assert!(
            repo.find_ref(&snapshot).unwrap().is_some(),
            "snapshot ref was written once stride was crossed"
        );
    }

    #[test]
    fn checkpoint_stride_grows_with_state_so_large_cobs_snapshot_less_often() {
        assert_eq!(
            checkpoint_stride(SnapshotStride::new(256), StateSize::new(0)),
            256
        );
        assert_eq!(
            checkpoint_stride(
                SnapshotStride::new(256),
                StateSize::new(256 * CHECKPOINT_GROWTH_DIVISOR)
            ),
            256,
            "up to stride*divisor entries the fixed floor governs, so small cobs snapshot exactly as before"
        );
        assert_eq!(
            checkpoint_stride(SnapshotStride::new(256), StateSize::new(1_600_000)),
            100_000,
            "a large cob snapshots at a fixed fraction of its size, so total snapshot bytes stay linear in the change count"
        );
        assert_eq!(
            checkpoint_stride(SnapshotStride::new(256), StateSize::new(4_000_000)),
            backend::MAX_GRAPH_CHANGES,
            "the stride stops at the serving limit so the boot tail always folds incrementally, never overflowing into a full rebuild"
        );
    }

    #[test]
    fn a_large_checkpointed_cob_bounds_its_boot_suffix_to_a_fraction_of_its_size() {
        let (_dir, repo) = fixture();
        let store = CobStore::new(&repo);
        let key = signer(2);
        let object = store
            .create(
                &cob_home(),
                &NameChange::Claim("name0000".into()),
                &key,
                at(0),
            )
            .unwrap()
            .object;

        let total = Names::SNAPSHOT_STRIDE.get() * CHECKPOINT_GROWTH_DIVISOR * 4;
        (1..total).for_each(|index| {
            claim(
                &store,
                object,
                &key,
                &format!("name{index:04}"),
                index as i64,
            )
            .unwrap();
        });

        assert_eq!(
            store.get::<Names>(object).unwrap().state().len(),
            total,
            "full fold still agrees after adaptive snapshotting"
        );

        let (_state, _tip, suffix) = store.checkpointed_state::<Names>(object).unwrap();
        let bound = checkpoint_stride(Names::SNAPSHOT_STRIDE, StateSize::new(total));
        assert!(
            suffix <= bound && bound < total,
            "boot replays only the {suffix}-change tail since the last snapshot, bounded by the adaptive stride {bound}, never the whole {total}-change history"
        );
    }

    #[test]
    fn a_corrupt_checkpoint_never_serves_a_wrong_answer_and_heals_at_the_live_tip() {
        type Forge = fn(CobId, Oid, &BTreeSet<String>) -> Vec<u8>;
        let cases: &[Forge] = &[
            |_object, _tip, _state| b"not a valid checkpoint envelope".to_vec(),
            |object, tip, state| {
                serde_ipld_dagcbor::to_vec(&(
                    CHECKPOINT_FORMAT + 1,
                    object.oid().to_hex(),
                    tip.to_hex(),
                    checkpoint_digest(object, ChangeId::new(tip), state).unwrap(),
                    state,
                ))
                .unwrap()
            },
            |object, tip, state| {
                let mut tampered = state.clone();
                tampered.insert("intruder".to_string());
                serde_ipld_dagcbor::to_vec(&(
                    CHECKPOINT_FORMAT,
                    object.oid().to_hex(),
                    tip.to_hex(),
                    checkpoint_digest(object, ChangeId::new(tip), state).unwrap(),
                    tampered,
                ))
                .unwrap()
            },
        ];

        cases.iter().enumerate().for_each(|(index, forge)| {
            let (_dir, repo) = fixture();
            let store = CobStore::new(&repo);
            let key = signer(200 + index as u64);
            let object = store
                .create(&cob_home(), &NameChange::Claim("squid".into()), &key, at(0))
                .unwrap()
                .object;
            (1..=Names::SNAPSHOT_STRIDE.get()).for_each(|step| {
                claim(&store, object, &key, &format!("name{step:04}"), step as i64).unwrap();
            });

            let real_state = store.get::<Names>(object).unwrap().state().clone();
            let real_tip = backend::resolve_tip(&repo, &NameChange::type_name(), object)
                .unwrap()
                .unwrap();
            let forged = forge(object, real_tip, &real_state);

            let snapshot = backend::checkpoint_ref_name(&NameChange::type_name(), object).unwrap();
            let live = repo.find_ref(&snapshot).unwrap().expect("snapshot exists");
            let blob = Oid::from(repo.git().write_blob(&forged).unwrap().detach());
            repo.update_ref(&RefUpdate::Update {
                name: snapshot.clone(),
                old: live,
                new: blob,
            })
            .unwrap();

            let duplicate = claim(&store, object, &key, "squid", 100);
            assert!(
                matches!(duplicate, Err(NameError::Taken)),
                "corrupt checkpoint falls back to real state instead of waving a duplicate through"
            );
            appended(claim(&store, object, &key, "intruder", 101));

            let healed = repo.find_ref(&snapshot).unwrap().unwrap();
            let bytes = repo.read_blob(healed).unwrap();
            let (tip, state) = decode_checkpoint::<BTreeSet<String>>(object, &bytes)
                .expect("healed snapshot decodes");
            let live_tip = backend::resolve_tip(&repo, &NameChange::type_name(), object)
                .unwrap()
                .unwrap();
            assert_eq!(
                tip,
                ChangeId::new(live_tip),
                "healed snapshot sits at the live tip"
            );
            assert_eq!(
                &state,
                store.get::<Names>(object).unwrap().state(),
                "healed snapshot matches the full fold"
            );
        });
    }

    #[test]
    fn a_checkpoint_envelope_is_bound_to_its_object_tip_and_format() {
        let (_dir, repo) = fixture();
        let store = CobStore::new(&repo);
        let key = signer(6);
        let a = store
            .create(&cob_home(), &NameChange::Claim("squid".into()), &key, at(0))
            .unwrap();
        let b = store
            .create(
                &cob_home(),
                &NameChange::Claim("anemone".into()),
                &key,
                at(1),
            )
            .unwrap();
        let state = BTreeSet::from(["squid".to_string()]);

        let bytes = encode_checkpoint(a.object, a.tip, &state).unwrap();
        assert!(
            decode_checkpoint::<BTreeSet<String>>(b.object, &bytes).is_none(),
            "checkpoint minted for one object mustn't decode under another"
        );
        assert_eq!(
            decode_checkpoint::<BTreeSet<String>>(a.object, &bytes),
            Some((a.tip, state.clone())),
            "checkpoint decodes to the tip and state its digest commits to under its own object"
        );

        let wrong_tip = ChangeId::new(Oid::from_hex(&"b".repeat(40)).unwrap());
        let swapped_tip = serde_ipld_dagcbor::to_vec(&(
            CHECKPOINT_FORMAT,
            a.object.oid().to_hex(),
            wrong_tip.oid().to_hex(),
            checkpoint_digest(a.object, a.tip, &state).unwrap(),
            state.clone(),
        ))
        .unwrap();
        assert!(
            decode_checkpoint::<BTreeSet<String>>(a.object, &swapped_tip).is_none(),
            "checkpoint whose tip is swapped away from the one its digest covers mustn't decode"
        );

        let future_version = serde_ipld_dagcbor::to_vec(&(
            CHECKPOINT_FORMAT + 1,
            a.object.oid().to_hex(),
            a.tip.oid().to_hex(),
            checkpoint_digest(a.object, a.tip, &state).unwrap(),
            state,
        ))
        .unwrap();
        assert!(
            decode_checkpoint::<BTreeSet<String>>(a.object, &future_version).is_none(),
            "envelope whose format version isn't the current one mustn't decode"
        );
    }

    #[test]
    fn the_rebuild_walk_floors_at_the_serving_limit_and_scales_above_it_with_headroom() {
        assert_eq!(
            backend::rebuild_change_limit_for(None),
            2_000_000,
            "an unmeasurable host rebuilds up to the generous fixed ceiling"
        );
        let available = |bytes| Some(knot_resource::AvailableBytes::new(bytes));
        assert_eq!(
            backend::rebuild_change_limit_for(available(64 * 1024 * 1024)),
            backend::MAX_GRAPH_CHANGES,
            "a squeezed host floors the rebuild walk at the serving limit, never beneath it"
        );
        assert_eq!(
            backend::rebuild_change_limit_for(available(0)),
            backend::MAX_GRAPH_CHANGES,
            "a host with no measured headroom still heals at least as deep as it serves"
        );
        let one_gib = backend::rebuild_change_limit_for(available(1024 * 1024 * 1024));
        assert!(
            (100_000..150_000).contains(&one_gib),
            "on a 1 GiB host the fold budget tracks the measured per-change cost near the serving limit, was {one_gib}"
        );
        let roomy = backend::rebuild_change_limit_for(available(64 * 1024 * 1024 * 1024));
        assert_eq!(roomy, 64 * 1024 * 1024 * 1024 / 4 / 2560);
        assert!(
            roomy > backend::MAX_GRAPH_CHANGES,
            "a roomy host rebuilds far past the serving limit, was {roomy}"
        );
    }

    #[test]
    fn tag_lifecycle_roundtrips_appends_reloads_and_keeps_signatures() {
        let (_dir, repo) = fixture();
        let store = CobStore::new(&repo);
        let key = signer(1);

        let created = store
            .create(&cob_home(), &Tag::Add("nel".into()), &key, at(1))
            .unwrap();
        let fresh = store.get::<Tags>(created.object).unwrap();
        assert_eq!(fresh.id(), created.object);
        assert_eq!(fresh.state(), &BTreeSet::from(["nel".to_string()]));
        assert_eq!(fresh.history().len(), 1);
        assert_eq!(fresh.history().root(), created.tip);
        let root = backend::read_change(&repo, ChangeId::new(created.object.oid())).unwrap();
        assert!(root.verify(&cob_home(), &root.author, None));

        store
            .update(
                &cob_home(),
                created.object,
                &Tag::Add("olaren".into()),
                &key,
                at(2),
            )
            .unwrap();
        let tip = store
            .update(
                &cob_home(),
                created.object,
                &Tag::Remove("nel".into()),
                &key,
                at(3),
            )
            .unwrap();

        let object = store.get::<Tags>(created.object).unwrap();
        assert_eq!(object.state(), &BTreeSet::from(["olaren".to_string()]));
        assert_eq!(object.history().len(), 3);
        assert_eq!(store.list::<Tags>().unwrap(), vec![created.object]);
        let order = object.history().traverse(Vec::new(), |mut acc, change| {
            acc.push(change.timestamp.get());
            acc
        });
        assert_eq!(order, vec![1, 2, 3]);
        assert_eq!(object.history().tips(), vec![tip]);

        let reloaded = store.get::<Tags>(created.object).unwrap();
        assert_eq!(object.state(), reloaded.state());
        let graph = store.graph::<Tags>(created.object).unwrap();
        let again = store.graph::<Tags>(created.object).unwrap();
        assert_eq!(graph.len(), again.len());
        assert_eq!(graph.causal_order(), again.causal_order());

        let head = backend::read_change(&repo, tip).unwrap();
        assert!(head.verify(&cob_home(), &head.author, Some(created.object)));
        assert!(!head.verify(&cob_home(), &head.author, None));
        assert!(!head.verify(
            &cob_home(),
            &head.author,
            Some(CobId::new(knot_types::Oid::null()))
        ));

        let absent = CobId::new(knot_types::Oid::from_hex(&"0".repeat(40)).unwrap());
        assert!(matches!(
            store.get::<Tags>(absent),
            Err(CobError::NoSuchObject(_))
        ));
    }

    #[test]
    fn undecodable_change_poisons_the_object() {
        let (_dir, repo) = fixture();
        let key = signer(5);
        let nsid = Tag::type_name();

        let root = backend::write_change(
            &cob_home(),
            &repo,
            &nsid,
            &Tag::Add("nel".into()).encode().unwrap(),
            &[],
            None,
            &key,
            at(1),
        )
        .unwrap();
        let object = CobId::new(root.oid());
        let garbage = backend::write_change(
            &cob_home(),
            &repo,
            &nsid,
            &[0xff, 0xff, 0xff],
            &[root],
            Some(object),
            &key,
            at(2),
        )
        .unwrap();
        let name = backend::cob_ref_name(&nsid, object).unwrap();
        repo.update_ref(&RefUpdate::Create {
            name,
            new: garbage.oid(),
        })
        .unwrap();

        assert!(matches!(
            CobStore::new(&repo).get::<Tags>(object),
            Err(CobError::UndecodableChange { .. })
        ));
    }

    #[test]
    fn merge_is_last_writer_wins_with_no_tombstone() {
        let key = signer(40);
        let nsid = Tag::type_name();
        let resolve = |add_seconds: i64, remove_seconds: i64| -> bool {
            let (_dir, repo) = fixture();
            let root = backend::write_change(
                &cob_home(),
                &repo,
                &nsid,
                &Tag::Add("seed".into()).encode().unwrap(),
                &[],
                None,
                &key,
                at(1),
            )
            .unwrap();
            let object = CobId::new(root.oid());
            let add = backend::write_change(
                &cob_home(),
                &repo,
                &nsid,
                &Tag::Add("nel".into()).encode().unwrap(),
                &[root],
                Some(object),
                &key,
                at(add_seconds),
            )
            .unwrap();
            let remove = backend::write_change(
                &cob_home(),
                &repo,
                &nsid,
                &Tag::Remove("nel".into()).encode().unwrap(),
                &[root],
                Some(object),
                &key,
                at(remove_seconds),
            )
            .unwrap();
            let merge = backend::write_change(
                &cob_home(),
                &repo,
                &nsid,
                &Tag::Add("merged".into()).encode().unwrap(),
                &[add, remove],
                Some(object),
                &key,
                at(100),
            )
            .unwrap();
            let name = backend::cob_ref_name(&nsid, object).unwrap();
            repo.update_ref(&RefUpdate::Create {
                name,
                new: merge.oid(),
            })
            .unwrap();
            CobStore::new(&repo)
                .get::<Tags>(object)
                .unwrap()
                .into_state()
                .contains("nel")
        };

        assert!(resolve(3, 2), "later add resurrects removed element");
        assert!(!resolve(2, 3), "later remove wins under last-writer-wins");
    }

    #[test]
    fn tip_not_descending_from_root_is_detached() {
        let (_dir, repo) = fixture();
        let key = signer(22);
        let nsid = Tag::type_name();
        let root = backend::write_change(
            &cob_home(),
            &repo,
            &nsid,
            &Tag::Add("nel".into()).encode().unwrap(),
            &[],
            None,
            &key,
            at(1),
        )
        .unwrap();
        let object = CobId::new(root.oid());
        let unrelated = backend::write_change(
            &cob_home(),
            &repo,
            &nsid,
            &Tag::Add("olaren".into()).encode().unwrap(),
            &[],
            None,
            &key,
            at(2),
        )
        .unwrap();
        let name = backend::cob_ref_name(&nsid, object).unwrap();
        repo.update_ref(&RefUpdate::Create {
            name,
            new: unrelated.oid(),
        })
        .unwrap();

        assert!(matches!(
            CobStore::new(&repo).get::<Tags>(object),
            Err(CobError::DetachedTip(_))
        ));
    }

    #[test]
    fn deep_history_loads_and_orders_without_overflow() {
        let count: i64 = 8_000;
        let (_dir, repo) = fixture();
        let key = signer(23);
        let nsid = Tag::type_name();
        let payload = Tag::Add("nel".into()).encode().unwrap();
        let root =
            backend::write_change(&cob_home(), &repo, &nsid, &payload, &[], None, &key, at(0))
                .unwrap();
        let object = CobId::new(root.oid());
        let tip = (1..count).fold(root, |parent, i| {
            backend::write_change(
                &cob_home(),
                &repo,
                &nsid,
                &payload,
                &[parent],
                Some(object),
                &key,
                at(i),
            )
            .unwrap()
        });
        publish(&repo, object, tip);
        let graph = CobStore::new(&repo).graph::<Tags>(object).unwrap();
        assert_eq!(graph.len(), count as usize);
        assert_eq!(graph.causal_order().len(), count as usize);

        let synthetic: u64 = 100_000;
        let oid = |index: u64| knot_types::Oid::from_hex(&format!("{index:040x}")).unwrap();
        let actor = ActorId::from_secp256k1(&[0x02; 33]);
        let changes: std::collections::BTreeMap<ChangeId, Change> = (1..=synthetic)
            .map(|index| {
                let id = ChangeId::new(oid(index));
                let parents = if index == 1 {
                    Vec::new()
                } else {
                    vec![ChangeId::new(oid(index - 1))]
                };
                let change = Change {
                    id,
                    revision: oid(index),
                    parents,
                    type_name: Tag::type_name(),
                    author: actor.clone(),
                    signature: Signature::from_bytes(Vec::new()),
                    payload: Payload::new(Vec::new()),
                    timestamp: UnixSeconds::new(index as i64),
                };
                (id, change)
            })
            .collect();
        let synthetic_order = ChangeGraph::new(CobId::new(oid(1)), changes).causal_order();
        assert_eq!(synthetic_order.len(), synthetic as usize);
        assert_eq!(synthetic_order.first(), Some(&ChangeId::new(oid(1))));
        assert_eq!(synthetic_order.last(), Some(&ChangeId::new(oid(synthetic))));
    }

    #[test]
    fn grafted_foreign_genesis_is_refused() {
        let (_dir, repo) = fixture();
        let key = signer(31);
        let nsid = Tag::type_name();
        let root = backend::write_change(
            &cob_home(),
            &repo,
            &nsid,
            &Tag::Add("nel".into()).encode().unwrap(),
            &[],
            None,
            &key,
            at(1),
        )
        .unwrap();
        let object = CobId::new(root.oid());
        let child = backend::write_change(
            &cob_home(),
            &repo,
            &nsid,
            &Tag::Add("olaren".into()).encode().unwrap(),
            &[root],
            Some(object),
            &key,
            at(2),
        )
        .unwrap();
        let foreign = backend::write_change(
            &cob_home(),
            &repo,
            &nsid,
            &Tag::Add("evil".into()).encode().unwrap(),
            &[],
            None,
            &key,
            at(3),
        )
        .unwrap();
        let merge = backend::write_change(
            &cob_home(),
            &repo,
            &nsid,
            &Tag::Add("merge".into()).encode().unwrap(),
            &[child, foreign],
            Some(object),
            &key,
            at(4),
        )
        .unwrap();
        let name = backend::cob_ref_name(&nsid, object).unwrap();
        repo.update_ref(&RefUpdate::Create {
            name,
            new: merge.oid(),
        })
        .unwrap();

        assert!(matches!(
            CobStore::new(&repo).get::<Tags>(object),
            Err(CobError::MultipleRoots { .. })
        ));
    }

    #[test]
    fn cob_ref_name_and_parse_cob_ref_are_inverses() {
        let nsid = Tag::type_name();
        let object = CobId::new(knot_types::Oid::from_hex(&"a".repeat(40)).unwrap());
        let name = backend::cob_ref_name(&nsid, object).unwrap();

        assert_eq!(parse_cob_ref(name.as_str()), Some((nsid, object)));
        assert_eq!(parse_cob_ref("refs/heads/main"), None);
        assert_eq!(
            parse_cob_ref("refs/cobs/sh.tangled.test.tag/not-an-oid"),
            None
        );
    }

    fn loose_commits(repo: &Repo) -> BTreeSet<knot_types::Oid> {
        std::fs::read_dir(repo.path().join("objects"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|shard| {
                shard
                    .file_name()
                    .to_str()
                    .map(|s| s.len() == 2)
                    .unwrap_or(false)
            })
            .flat_map(|shard| {
                let prefix = shard.file_name().to_str().unwrap().to_string();
                std::fs::read_dir(shard.path())
                    .unwrap()
                    .filter_map(Result::ok)
                    .filter_map(move |entry| {
                        let rest = entry.file_name().to_str()?.to_string();
                        knot_types::Oid::from_hex(&format!("{prefix}{rest}")).ok()
                    })
                    .collect::<Vec<_>>()
            })
            .filter(|oid| {
                repo.git()
                    .find_object(oid.object_id())
                    .map(|object| object.kind == gix::object::Kind::Commit)
                    .unwrap_or(false)
            })
            .collect()
    }

    #[test]
    fn a_contended_retry_keeps_both_writes_and_leaves_no_dangling_commit() {
        let (_dir, repo) = fixture();
        let store = CobStore::new(&repo);
        let key = signer(124);
        let created = store
            .create(&cob_home(), &Tag::Add("base".into()), &key, at(1))
            .unwrap();

        let injected = std::cell::Cell::new(false);
        let mine = store.update_with::<Tags, CobError>(
            &cob_home(),
            created.object,
            &key,
            at(3),
            |_state| {
                if !injected.replace(true) {
                    store
                        .update(
                            &cob_home(),
                            created.object,
                            &Tag::Add("intruder".into()),
                            &key,
                            at(2),
                        )
                        .unwrap();
                }
                Ok(Tag::Add("mine".into()))
            },
        );
        assert!(
            mine.is_ok(),
            "handler retried instead of surfacing StaleTip"
        );

        let state = store.get::<Tags>(created.object).unwrap().into_state();
        assert!(
            state.contains("intruder"),
            "concurrent append survives retry"
        );
        assert!(
            state.contains("mine"),
            "retried append isn't lost to stale tip"
        );

        let reachable: BTreeSet<knot_types::Oid> = store
            .graph::<Tags>(created.object)
            .unwrap()
            .causal_order()
            .into_iter()
            .map(|change| change.oid())
            .collect();
        assert_eq!(
            reachable.len(),
            3,
            "live chain is base, intruder, then mine"
        );
        assert_eq!(
            loose_commits(&repo),
            reachable,
            "abandoned compare-and-swap attempt left no dangling commit behind"
        );
    }

    #[test]
    fn exhausting_the_retry_budget_is_contended_and_leaves_no_dangling_commits() {
        let (_dir, repo) = fixture();
        let store = CobStore::new(&repo);
        let key = signer(126);
        let created = store
            .create(&cob_home(), &Tag::Add("base".into()), &key, at(1))
            .unwrap();
        let calls = std::cell::Cell::new(0i64);

        let result = store.update_with::<Tags, CobError>(
            &cob_home(),
            created.object,
            &key,
            at(1000),
            |_state| {
                let i = calls.get();
                calls.set(i + 1);
                store
                    .update(
                        &cob_home(),
                        created.object,
                        &Tag::Add(format!("intruder{i}")),
                        &key,
                        at(100 + i),
                    )
                    .unwrap();
                Ok(Tag::Add("mine".into()))
            },
        );

        assert!(matches!(result, Err(CobError::Contended(_))));
        assert_eq!(
            calls.get(),
            MAX_CAS_RETRIES as i64,
            "decision ran exactly the retry budget before failing closed"
        );

        let reachable: BTreeSet<knot_types::Oid> = store
            .graph::<Tags>(created.object)
            .unwrap()
            .causal_order()
            .into_iter()
            .map(|change| change.oid())
            .collect();
        assert_eq!(
            reachable.len(),
            1 + MAX_CAS_RETRIES,
            "base plus one landed commit per injected intruder"
        );
        assert_eq!(
            loose_commits(&repo),
            reachable,
            "every failed attempt across whole budget cleaned up its own orphan"
        );
    }

    #[test]
    fn an_identical_concurrent_change_is_not_cleaned_up_as_the_live_tip() {
        let (_dir, repo) = fixture();
        let store = CobStore::new(&repo);
        let key = signer(125);
        let nsid = Tag::type_name();
        let created = store
            .create(&cob_home(), &Tag::Add("base".into()), &key, at(1))
            .unwrap();
        let dup = Tag::Add("dup".into());
        let bytes = dup.encode().unwrap();

        let first = backend::write_change(
            &cob_home(),
            &repo,
            &nsid,
            &bytes,
            &[created.tip],
            Some(created.object),
            &key,
            at(2),
        )
        .unwrap();
        let second = backend::write_change(
            &cob_home(),
            &repo,
            &nsid,
            &bytes,
            &[created.tip],
            Some(created.object),
            &key,
            at(2),
        )
        .unwrap();
        assert_eq!(
            first, second,
            "byte-identical changes are content-addressed to one commit"
        );

        let winner = store
            .append(
                &cob_home(),
                created.object,
                &nsid,
                created.tip,
                &dup,
                &key,
                at(2),
            )
            .unwrap();
        assert_eq!(winner, first);

        let loser = store.append(
            &cob_home(),
            created.object,
            &nsid,
            created.tip,
            &dup,
            &key,
            at(2),
        );
        assert!(matches!(loser, Err(CobError::StaleTip { .. })));

        assert_eq!(
            backend::resolve_tip(&repo, &nsid, created.object).unwrap(),
            Some(winner.oid()),
            "shared live tip survived identical-write loser's cleanup"
        );
        assert!(
            store.get::<Tags>(created.object).is_ok(),
            "live tip wasn't deleted out from under the object"
        );
    }

    #[test]
    fn collect_refuses_a_graph_past_its_limit() {
        let (_dir, repo) = fixture();
        let key = signer(34);
        let nsid = Tag::type_name();
        let payload = Tag::Add("nel".into()).encode().unwrap();
        let root =
            backend::write_change(&cob_home(), &repo, &nsid, &payload, &[], None, &key, at(0))
                .unwrap();
        let object = CobId::new(root.oid());
        let tip = (1..4).fold(root, |parent, i| {
            backend::write_change(
                &cob_home(),
                &repo,
                &nsid,
                &payload,
                &[parent],
                Some(object),
                &key,
                at(i),
            )
            .unwrap()
        });

        assert!(matches!(
            backend::collect(&repo, tip, object, 2, None),
            Err(CobError::HistoryTooLong(_))
        ));
        assert_eq!(
            backend::collect(&repo, tip, object, 10, None)
                .unwrap()
                .len(),
            4
        );
    }

    #[test]
    fn changes_since_returns_a_suffix_enforces_shape_and_rejects_a_non_ancestor() {
        let (_dir, repo) = fixture();
        let store = CobStore::new(&repo);
        let key = signer(60);

        let created = store
            .create(&cob_home(), &Tag::Add("nel".into()), &key, at(1))
            .unwrap();
        let full = store.changes_since::<Tags>(created.object, None).unwrap();
        assert_eq!(full.tip, created.tip);
        assert_eq!(
            full.changes.iter().map(|c| c.id).collect::<Vec<_>>(),
            vec![created.tip]
        );

        let second = store
            .update(
                &cob_home(),
                created.object,
                &Tag::Add("olaren".into()),
                &key,
                at(2),
            )
            .unwrap();
        let third = store
            .update(
                &cob_home(),
                created.object,
                &Tag::Add("teq".into()),
                &key,
                at(3),
            )
            .unwrap();

        let delta = store
            .changes_since::<Tags>(created.object, Some(created.tip))
            .unwrap();
        assert_eq!(delta.tip, third);
        assert_eq!(
            delta.changes.iter().map(|c| c.id).collect::<Vec<_>>(),
            vec![second, third],
            "delta is the appended changes in causal order instead of whole graph"
        );

        let caught_up = store
            .changes_since::<Tags>(created.object, Some(third))
            .unwrap();
        assert!(caught_up.changes.is_empty());
        assert_eq!(caught_up.tip, third);

        let (_diverged_dir, diverged_repo) = fixture();
        let diverged = linear_chain(&diverged_repo, &signer(71), &["nel", "olaren"]);
        let stranger = ChangeId::new(knot_types::Oid::from_hex(&"a".repeat(40)).unwrap());
        assert!(
            matches!(
                CobStore::new(&diverged_repo).changes_since::<Tags>(diverged, Some(stranger)),
                Err(CobError::DivergedTip { .. })
            ),
            "a since that doesn't descend from the indexed tip is refused, not re-folded onto stale state"
        );

        let (_forked_dir, forked_repo) = fixture();
        let forked = forked_tag_object(&forked_repo, &signer(70), "nel", "olaren", "teq");
        let forked_store = CobStore::new(&forked_repo);
        assert!(
            matches!(
                forked_store.get::<LinearTags>(forked),
                Err(CobError::ForkedHistory { .. })
            ),
            "materialization fails closed on forked linear history"
        );
        assert!(
            matches!(
                forked_store.changes_since::<LinearTags>(forked, None),
                Err(CobError::ForkedHistory { .. })
            ),
            "changes_since refuses the same fork instead of folding it for the index"
        );
        assert!(
            forked_store.changes_since::<Tags>(forked, None).is_ok(),
            "convergent class still folds the same graph"
        );
    }

    struct LinearTags;

    impl Evaluate for LinearTags {
        type State = BTreeSet<String>;
        type Change = Tag;

        const HISTORY: HistoryModel = HistoryModel::Linear;

        fn initial() -> Self::State {
            BTreeSet::new()
        }

        fn apply(state: Self::State, change: Self::Change, author: &ActorId) -> Self::State {
            Tags::apply(state, change, author)
        }
    }

    #[test]
    fn verify_accepts_the_owner_and_rejects_a_foreign_signer_or_mismatched_type() {
        let (_dir, repo) = fixture();
        let store = CobStore::new(&repo);
        let key = signer(51);
        let object = linear_chain(&repo, &key, &["nel", "olaren"]);
        let owner = ActorId::from_secp256k1(key.public_key().as_bytes());
        assert!(store.verify::<Tags>(&cob_home(), object, &owner).is_ok());

        let stranger = ActorId::from_secp256k1(signer(52).public_key().as_bytes());
        assert!(matches!(
            store.verify::<Tags>(&cob_home(), object, &stranger),
            Err(CobError::UnverifiedChange { .. })
        ));

        let (_other_dir, other_repo) = fixture();
        let key = signer(53);
        let foreign = TypeName::new("sh.tangled.test.other").unwrap();
        let root = tag_root(&other_repo, &key, "nel", 1);
        let object = CobId::new(root.oid());
        let child = backend::write_change(
            &cob_home(),
            &other_repo,
            &foreign,
            &Tag::Add("olaren".into()).encode().unwrap(),
            &[root],
            Some(object),
            &key,
            at(2),
        )
        .unwrap();
        publish(&other_repo, object, child);
        let owner = ActorId::from_secp256k1(key.public_key().as_bytes());
        assert!(
            matches!(
                CobStore::new(&other_repo).verify::<Tags>(&cob_home(), object, &owner),
                Err(CobError::UnexpectedChangeType { .. })
            ),
            "owner-signed change whose type doesn't match namespace is refused at import"
        );
    }

    fn ops_and_perm() -> impl Strategy<Value = (Vec<(bool, u8)>, Vec<u32>)> {
        prop::collection::vec((any::<bool>(), 0u8..4u8), 1..8usize).prop_flat_map(|ops| {
            let len = ops.len();
            (Just(ops), prop::collection::vec(any::<u32>(), len))
        })
    }

    fn permutation(keys: &[u32]) -> Vec<usize> {
        let mut order: Vec<usize> = (0..keys.len()).collect();
        order.sort_by_key(|&index| keys[index]);
        order
    }

    fn model_state(ops: &[(bool, u8)]) -> BTreeSet<String> {
        ops.iter().fold(
            BTreeSet::from(["base".to_string()]),
            |mut state, (is_add, subject)| {
                let name = format!("s{subject}");
                if *is_add {
                    state.insert(name);
                } else {
                    state.remove(&name);
                }
                state
            },
        )
    }

    fn build_repo(ops: &[(bool, u8)], creation_order: &[usize]) -> (TempDir, Repo, CobId) {
        let (dir, repo) = fixture();
        let key = signer(500);
        let nsid = Tag::type_name();
        let root = tag_root(&repo, &key, "base", 1);
        let object = CobId::new(root.oid());
        let child = |index: usize| {
            let (is_add, subject) = ops[index];
            let name = format!("s{subject}");
            let payload = if is_add {
                Tag::Add(name)
            } else {
                Tag::Remove(name)
            };
            backend::write_change(
                &cob_home(),
                &repo,
                &nsid,
                &payload.encode().unwrap(),
                &[root],
                Some(object),
                &key,
                at(index as i64 + 2),
            )
            .unwrap()
        };
        creation_order.iter().for_each(|&index| {
            child(index);
        });
        let parents: Vec<ChangeId> = (0..ops.len()).map(child).collect();
        let merge = backend::write_change(
            &cob_home(),
            &repo,
            &nsid,
            &Tag::Remove("absent".into()).encode().unwrap(),
            &parents,
            Some(object),
            &key,
            at(ops.len() as i64 + 2),
        )
        .unwrap();
        publish(&repo, object, merge);
        (dir, repo, object)
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 32, ..ProptestConfig::default() })]

        #[test]
        fn prop_concurrent_changes_converge_to_the_causal_fold((ops, keys) in ops_and_perm()) {
            let identity: Vec<usize> = (0..ops.len()).collect();
            let model = model_state(&ops);

            let (_canon_dir, canon_repo, canon_object) = build_repo(&ops, &identity);
            let canonical = CobStore::new(&canon_repo)
                .get::<Tags>(canon_object)
                .unwrap()
                .into_state();

            let (_perm_dir, perm_repo, perm_object) = build_repo(&ops, &permutation(&keys));
            let permuted = CobStore::new(&perm_repo)
                .get::<Tags>(perm_object)
                .unwrap()
                .into_state();

            prop_assert_eq!(&canonical, &model);
            prop_assert_eq!(&permuted, &model);
        }

        #[test]
        fn prop_rematerialization_is_idempotent((ops, keys) in ops_and_perm()) {
            let (_dir, repo, object) = build_repo(&ops, &permutation(&keys));
            let store = CobStore::new(&repo);
            let first = store.get::<Tags>(object).unwrap();
            let second = store.get::<Tags>(object).unwrap();
            prop_assert_eq!(first.state(), second.state());
            prop_assert_eq!(first.history().len(), second.history().len());
            prop_assert_eq!(
                store.graph::<Tags>(object).unwrap().causal_order(),
                store.graph::<Tags>(object).unwrap().causal_order()
            );
        }
    }
}
