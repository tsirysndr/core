use std::collections::{HashMap, HashSet};

use gix::ObjectId;
use gix::objs::tree::EntryKind as TreeEntryKind;
use gix::objs::{CommitRef, Kind, TagRefIter, TreeRef};
use gix::prelude::FindExt;
use knot_git::Repo;
use knot_types::UnixSeconds;

use crate::MaintError;
use crate::fsio;

const GRAPH_PARENT_NONE: u32 = 0x7000_0000;
const GRAPH_EXTRA_EDGES_NEEDED: u32 = 0x8000_0000;
const GRAPH_LAST_EDGE: u32 = 0x8000_0000;
const GRAPH_GENERATION_MAX: u32 = 0x3FFF_FFFF;
const MAX_PEEL_DEPTH: usize = 32;

const CORRECTED_OFFSET_OVERFLOW: u32 = 0x8000_0000;
const CORRECTED_OFFSET_MAX: u64 = (1 << 31) - 1;

const BLOOM_HASH_VERSION: u32 = 2;
const BLOOM_NUM_HASHES: u32 = 7;
const BLOOM_BITS_PER_ENTRY: u32 = 10;
const BLOOM_MAX_CHANGED_PATHS: usize = 512;
const BLOOM_SEED0: u32 = 0x293a_e76f;
const BLOOM_SEED1: u32 = 0x7e64_6e2c;
const MURMUR_C1: u32 = 0xcc9e_2d51;
const MURMUR_C2: u32 = 0x1b87_3593;
const MURMUR_N: u32 = 0xe654_6b64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct Generation(u32);

impl Generation {
    const fn new(value: u32) -> Self {
        Self(value)
    }

    const fn get(self) -> u32 {
        self.0
    }

    const fn succ(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

knot_types::scalar_newtype! {
    struct GraphPosition(u32);
}

#[derive(Debug, Clone, Copy)]
struct ParentField(u32);

impl ParentField {
    const NONE: Self = Self(GRAPH_PARENT_NONE);

    fn from_position(position: Option<GraphPosition>) -> Self {
        position.map_or(Self::NONE, |position| Self(position.get()))
    }

    fn extra_edges(index: usize) -> Self {
        Self(GRAPH_EXTRA_EDGES_NEEDED | index as u32)
    }

    fn to_be_bytes(self) -> [u8; 4] {
        self.0.to_be_bytes()
    }
}

#[derive(Debug, Clone, Copy)]
struct ParentFields {
    first: ParentField,
    second: ParentField,
}

#[derive(Debug, Clone, Copy)]
struct EdgeField(u32);

impl EdgeField {
    fn new(parent: ParentField, last: bool) -> Self {
        Self(parent.0 | if last { GRAPH_LAST_EDGE } else { 0 })
    }

    fn to_be_bytes(self) -> [u8; 4] {
        self.0.to_be_bytes()
    }
}

knot_types::scalar_newtype! {
    struct CorrectedDate(u64);
}

struct ChangedPathFilter(Vec<u8>);

impl ChangedPathFilter {
    fn bytes(&self) -> &[u8] {
        &self.0
    }

    fn len_bytes(&self) -> usize {
        self.0.len()
    }
}

struct CommitMeta {
    tree: ObjectId,
    parents: Vec<ObjectId>,
    seconds: UnixSeconds,
}

pub fn graph_path(repo: &Repo) -> std::path::PathBuf {
    repo.objects_dir().join("info").join("commit-graph")
}

pub fn exists(repo: &Repo) -> bool {
    graph_path(repo).exists()
}

pub fn write(repo: &Repo) -> Result<bool, MaintError> {
    let kind = repo.object_format().kind();
    let Some(commits) = collect(repo, kind)? else {
        return Ok(false);
    };
    if commits.is_empty() {
        return Ok(false);
    }
    let blooms = changed_path_filters(repo, &commits, kind)?;
    let bytes = serialize(&commits, &blooms, kind);
    let path = graph_path(repo);
    let info_dir = repo.objects_dir().join("info");
    std::fs::create_dir_all(&info_dir).map_err(|error| fsio::io_error(&info_dir, error))?;
    clear_chain(&info_dir)?;
    knot_resource::atomic_write_bytes(&path, &bytes, knot_resource::FileMode::Inherited)?;
    Ok(true)
}

pub fn remove(repo: &Repo) -> Result<(), MaintError> {
    let path = graph_path(repo);
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(fsio::io_error(&path, error)),
    }
    clear_chain(&repo.objects_dir().join("info"))
}

fn clear_chain(info_dir: &std::path::Path) -> Result<(), MaintError> {
    let chain_dir = info_dir.join("commit-graphs");
    match std::fs::remove_dir_all(&chain_dir) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(fsio::io_error(&chain_dir, error)),
    }
}

fn collect(
    repo: &Repo,
    kind: gix::hash::Kind,
) -> Result<Option<HashMap<ObjectId, CommitMeta>>, MaintError> {
    let odb = &repo.git().objects;
    let mut stack: Vec<ObjectId> = repo
        .references()?
        .into_iter()
        .filter_map(|record| peel_to_commit(odb, record.target.object_id(), kind, MAX_PEEL_DEPTH))
        .collect();
    let mut commits: HashMap<ObjectId, CommitMeta> = HashMap::new();
    while let Some(oid) = stack.pop() {
        if commits.contains_key(&oid) {
            continue;
        }
        let mut buf = Vec::new();
        let data = match odb.find(&oid, &mut buf) {
            Ok(data) => data,
            Err(_) => return Ok(None),
        };
        if data.kind != Kind::Commit {
            return Ok(None);
        }
        let commit = CommitRef::from_bytes(data.data, kind)
            .map_err(|error| MaintError::CommitGraph(error.to_string()))?;
        let tree = commit.tree();
        let parents: Vec<ObjectId> = commit.parents().collect();
        let seconds = UnixSeconds::new(commit.committer().map(|sig| sig.seconds()).unwrap_or(0));
        parents.iter().for_each(|parent| stack.push(*parent));
        commits.insert(
            oid,
            CommitMeta {
                tree,
                parents,
                seconds,
            },
        );
    }
    Ok(Some(commits))
}

fn peel_to_commit(
    odb: &gix::odb::Handle,
    oid: ObjectId,
    kind: gix::hash::Kind,
    depth: usize,
) -> Option<ObjectId> {
    if depth == 0 {
        return None;
    }
    let mut buf = Vec::new();
    let data = odb.find(&oid, &mut buf).ok()?;
    match data.kind {
        Kind::Commit => Some(oid),
        Kind::Tag => {
            let target = TagRefIter::from_bytes(data.data, kind).target_id().ok()?;
            peel_to_commit(odb, target, kind, depth - 1)
        }
        _ => None,
    }
}

fn serialize(
    commits: &HashMap<ObjectId, CommitMeta>,
    blooms: &HashMap<ObjectId, ChangedPathFilter>,
    kind: gix::hash::Kind,
) -> Vec<u8> {
    let hash_len = match kind {
        gix::hash::Kind::Sha256 => 32,
        _ => 20,
    };
    let mut oids: Vec<ObjectId> = commits.keys().copied().collect();
    oids.sort();
    let position: HashMap<ObjectId, GraphPosition> = oids
        .iter()
        .enumerate()
        .map(|(index, oid)| (*oid, GraphPosition::new(index as u32)))
        .collect();
    let generations = generations(commits, &oids);
    let corrected = corrected_dates(commits, &oids);

    let mut edges: Vec<EdgeField> = Vec::new();
    let mut cdat: Vec<u8> = Vec::with_capacity(oids.len() * (hash_len + 16));
    oids.iter().for_each(|oid| {
        let meta = &commits[oid];
        cdat.extend_from_slice(meta.tree.as_slice());
        let parents = parent_fields(&meta.parents, &position, &mut edges);
        cdat.extend_from_slice(&parents.first.to_be_bytes());
        cdat.extend_from_slice(&parents.second.to_be_bytes());
        let generation = generations
            .get(oid)
            .copied()
            .unwrap_or(Generation::new(0))
            .get() as u64;
        let date = (meta.seconds.get().max(0) as u64) & 0x3_FFFF_FFFF;
        let packed = (generation << 34) | date;
        cdat.extend_from_slice(&packed.to_be_bytes());
    });

    let (gda2, overflow) = oids.iter().fold(
        (Vec::with_capacity(oids.len() * 4), Vec::<u64>::new()),
        |(mut bytes, mut ovf), oid| {
            let date = commits[oid].seconds.get().max(0) as u64;
            let offset = corrected
                .get(oid)
                .copied()
                .unwrap_or(CorrectedDate::new(date))
                .get()
                .saturating_sub(date);
            let packed = if offset > CORRECTED_OFFSET_MAX {
                let index = ovf.len() as u32;
                ovf.push(offset);
                CORRECTED_OFFSET_OVERFLOW | index
            } else {
                offset as u32
            };
            bytes.extend_from_slice(&packed.to_be_bytes());
            (bytes, ovf)
        },
    );
    let gdo2: Vec<u8> = overflow
        .iter()
        .flat_map(|value| value.to_be_bytes())
        .collect();

    let bidx: Vec<u8> = oids
        .iter()
        .scan(0u32, |acc, oid| {
            *acc = acc.saturating_add(filter_for(blooms, oid).len_bytes() as u32);
            Some(acc.to_be_bytes())
        })
        .flatten()
        .collect();
    let bdat: Vec<u8> = [BLOOM_HASH_VERSION, BLOOM_NUM_HASHES, BLOOM_BITS_PER_ENTRY]
        .iter()
        .flat_map(|value| value.to_be_bytes())
        .chain(
            oids.iter()
                .flat_map(|oid| filter_for(blooms, oid).bytes().iter().copied()),
        )
        .collect();

    let oidf = fanout(&oids);
    let mut oidl: Vec<u8> = Vec::with_capacity(oids.len() * hash_len);
    oids.iter()
        .for_each(|oid| oidl.extend_from_slice(oid.as_slice()));
    let mut edge_bytes: Vec<u8> = Vec::with_capacity(edges.len() * 4);
    edges
        .iter()
        .for_each(|edge| edge_bytes.extend_from_slice(&edge.to_be_bytes()));

    let mut chunks: Vec<(&[u8; 4], Vec<u8>)> = vec![
        (b"OIDF", oidf),
        (b"OIDL", oidl),
        (b"CDAT", cdat),
        (b"GDA2", gda2),
    ];
    if !gdo2.is_empty() {
        chunks.push((b"GDO2", gdo2));
    }
    if !edge_bytes.is_empty() {
        chunks.push((b"EDGE", edge_bytes));
    }
    chunks.push((b"BIDX", bidx));
    chunks.push((b"BDAT", bdat));

    assemble(chunks, kind)
}

fn filter_for<'a>(
    blooms: &'a HashMap<ObjectId, ChangedPathFilter>,
    oid: &ObjectId,
) -> &'a ChangedPathFilter {
    static EMPTY: ChangedPathFilter = ChangedPathFilter(Vec::new());
    blooms.get(oid).unwrap_or(&EMPTY)
}

fn parent_fields(
    parents: &[ObjectId],
    position: &HashMap<ObjectId, GraphPosition>,
    edges: &mut Vec<EdgeField>,
) -> ParentFields {
    let pos = |oid: &ObjectId| ParentField::from_position(position.get(oid).copied());
    match parents {
        [] => ParentFields {
            first: ParentField::NONE,
            second: ParentField::NONE,
        },
        [first] => ParentFields {
            first: pos(first),
            second: ParentField::NONE,
        },
        [first, second] => ParentFields {
            first: pos(first),
            second: pos(second),
        },
        [first, rest @ ..] => {
            let edge_index = edges.len();
            let last = rest.len() - 1;
            rest.iter().enumerate().for_each(|(index, parent)| {
                edges.push(EdgeField::new(pos(parent), index == last));
            });
            ParentFields {
                first: pos(first),
                second: ParentField::extra_edges(edge_index),
            }
        }
    }
}

fn fanout(oids: &[ObjectId]) -> Vec<u8> {
    let mut buckets = [0u32; 256];
    oids.iter()
        .for_each(|oid| buckets[oid.as_slice()[0] as usize] += 1);
    (1..256).for_each(|index| buckets[index] += buckets[index - 1]);
    buckets
        .iter()
        .flat_map(|count| count.to_be_bytes())
        .collect()
}

fn resolve_topo<V: Copy>(
    commits: &HashMap<ObjectId, CommitMeta>,
    oids: &[ObjectId],
    transform: impl Fn(&[V], &CommitMeta) -> V,
) -> HashMap<ObjectId, V> {
    let mut value: HashMap<ObjectId, V> = HashMap::new();
    oids.iter().for_each(|root| {
        if value.contains_key(root) {
            return;
        }
        let mut stack = vec![*root];
        while let Some(top) = stack.last().copied() {
            if value.contains_key(&top) {
                stack.pop();
                continue;
            }
            let parents = &commits[&top].parents;
            let unresolved: Vec<ObjectId> = parents
                .iter()
                .filter(|parent| commits.contains_key(*parent) && !value.contains_key(*parent))
                .copied()
                .collect();
            if unresolved.is_empty() {
                let resolved: Vec<V> = parents
                    .iter()
                    .filter_map(|parent| value.get(parent))
                    .copied()
                    .collect();
                let computed = transform(&resolved, &commits[&top]);
                value.insert(top, computed);
                stack.pop();
            } else {
                unresolved.into_iter().for_each(|parent| stack.push(parent));
            }
        }
    });
    value
}

fn generations(
    commits: &HashMap<ObjectId, CommitMeta>,
    oids: &[ObjectId],
) -> HashMap<ObjectId, Generation> {
    resolve_topo(commits, oids, |parents: &[Generation], _meta| {
        parents
            .iter()
            .copied()
            .max()
            .unwrap_or(Generation::new(0))
            .succ()
            .min(Generation::new(GRAPH_GENERATION_MAX))
    })
}

fn corrected_dates(
    commits: &HashMap<ObjectId, CommitMeta>,
    oids: &[ObjectId],
) -> HashMap<ObjectId, CorrectedDate> {
    resolve_topo(commits, oids, |parents: &[CorrectedDate], meta| {
        let max_parent = parents.iter().map(|date| date.get()).max().unwrap_or(0);
        let date = meta.seconds.get().max(0) as u64;
        let base = if date > max_parent {
            date - 1
        } else {
            max_parent
        };
        CorrectedDate::new(base + 1)
    })
}

fn tuned(handle: &gix::odb::Handle) -> gix::odb::Handle {
    let mut odb = handle.clone();
    odb.refresh_never();
    odb.prevent_pack_unload();
    odb
}

fn one_filter(
    odb: &gix::odb::Handle,
    commits: &HashMap<ObjectId, CommitMeta>,
    meta: &CommitMeta,
    kind: gix::hash::Kind,
) -> Result<ChangedPathFilter, MaintError> {
    let parent_tree = meta
        .parents
        .first()
        .and_then(|parent| commits.get(parent))
        .map(|found| found.tree);
    let changed = diff_trees(odb, parent_tree, Some(meta.tree), kind)?;
    Ok(build_filter(&changed))
}

fn changed_path_filters(
    repo: &Repo,
    commits: &HashMap<ObjectId, CommitMeta>,
    kind: gix::hash::Kind,
) -> Result<HashMap<ObjectId, ChangedPathFilter>, MaintError> {
    let entries: Vec<(&ObjectId, &CommitMeta)> = commits.iter().collect();
    let path = repo.path().to_owned();
    let produced = knot_resource::map_chunks(&entries, |batch| {
        let local = Repo::open(&path)?;
        let odb = tuned(&local.git().objects);
        batch
            .iter()
            .map(|(oid, meta)| Ok((**oid, one_filter(&odb, commits, meta, kind)?)))
            .collect::<Result<Vec<_>, MaintError>>()
    })?;
    Ok(produced.into_iter().collect())
}

fn tree_entries(
    odb: &gix::odb::Handle,
    oid: Option<ObjectId>,
    kind: gix::hash::Kind,
) -> Result<HashMap<Vec<u8>, (TreeEntryKind, ObjectId)>, MaintError> {
    let Some(oid) = oid else {
        return Ok(HashMap::new());
    };
    if oid == ObjectId::empty_tree(kind) {
        return Ok(HashMap::new());
    }
    let mut buf = Vec::new();
    let data = odb
        .find(&oid, &mut buf)
        .map_err(|error| MaintError::CommitGraph(error.to_string()))?;
    if data.kind != Kind::Tree {
        return Ok(HashMap::new());
    }
    let tree = TreeRef::from_bytes(data.data, kind)
        .map_err(|error| MaintError::CommitGraph(error.to_string()))?;
    Ok(tree
        .entries
        .into_iter()
        .map(|entry| {
            (
                entry.filename.to_vec(),
                (entry.mode.kind(), entry.oid.to_owned()),
            )
        })
        .collect())
}

fn diff_trees(
    odb: &gix::odb::Handle,
    parent: Option<ObjectId>,
    commit: Option<ObjectId>,
    kind: gix::hash::Kind,
) -> Result<Vec<Vec<u8>>, MaintError> {
    let is_tree = |kind: &TreeEntryKind| matches!(kind, TreeEntryKind::Tree);
    let mut out: Vec<Vec<u8>> = Vec::new();
    let mut stack: Vec<(Option<ObjectId>, Option<ObjectId>, Vec<u8>)> =
        vec![(parent, commit, Vec::new())];
    while let Some((parent, commit, prefix)) = stack.pop() {
        let parent_entries = tree_entries(odb, parent, kind)?;
        let commit_entries = tree_entries(odb, commit, kind)?;
        let names: HashSet<&Vec<u8>> = parent_entries.keys().chain(commit_entries.keys()).collect();
        names.into_iter().for_each(|name| {
            let full: Vec<u8> = prefix.iter().copied().chain(name.iter().copied()).collect();
            let subprefix =
                || -> Vec<u8> { full.iter().copied().chain(std::iter::once(b'/')).collect() };
            match (parent_entries.get(name), commit_entries.get(name)) {
                (None, Some((ck, co))) => {
                    if is_tree(ck) {
                        stack.push((None, Some(*co), subprefix()));
                    } else {
                        out.push(full);
                    }
                }
                (Some((pk, po)), None) => {
                    if is_tree(pk) {
                        stack.push((Some(*po), None, subprefix()));
                    } else {
                        out.push(full);
                    }
                }
                (Some((pk, po)), Some((ck, co))) => match (is_tree(pk), is_tree(ck)) {
                    (true, true) => {
                        if po != co {
                            stack.push((Some(*po), Some(*co), subprefix()));
                        }
                    }
                    (false, false) => {
                        if po != co || pk != ck {
                            out.push(full);
                        }
                    }
                    (true, false) => {
                        stack.push((Some(*po), None, subprefix()));
                        out.push(full);
                    }
                    (false, true) => {
                        stack.push((None, Some(*co), subprefix()));
                        out.push(full);
                    }
                },
                (None, None) => {}
            }
        });
    }
    Ok(out)
}

fn build_filter(changed: &[Vec<u8>]) -> ChangedPathFilter {
    if changed.len() > BLOOM_MAX_CHANGED_PATHS {
        // `0xFF` = git's "too many changed paths" marker.
        return ChangedPathFilter(vec![0xFF]);
    }
    let paths: HashSet<Vec<u8>> = changed.iter().flat_map(|path| prefixes(path)).collect();
    if paths.len() > BLOOM_MAX_CHANGED_PATHS {
        return ChangedPathFilter(vec![0xFF]);
    }
    let bits = paths.len() * BLOOM_BITS_PER_ENTRY as usize;
    let len_bytes = bits.div_ceil(8).max(1);
    let modulus = (len_bytes * 8) as u64;
    let data = paths.iter().fold(vec![0u8; len_bytes], |mut data, path| {
        let hash0 = murmur3(BLOOM_SEED0, path);
        let hash1 = murmur3(BLOOM_SEED1, path);
        (0..BLOOM_NUM_HASHES).for_each(|index| {
            let combined = hash0.wrapping_add(index.wrapping_mul(hash1));
            let position = (combined as u64) % modulus;
            data[(position / 8) as usize] |= 1 << (position % 8);
        });
        data
    });
    ChangedPathFilter(data)
}

fn prefixes(path: &[u8]) -> Vec<Vec<u8>> {
    std::iter::once(path.to_vec())
        .chain(
            path.iter()
                .enumerate()
                .filter(|(_, byte)| **byte == b'/')
                .map(|(index, _)| path[..index].to_vec()),
        )
        .collect()
}

fn murmur3(seed: u32, data: &[u8]) -> u32 {
    let body = data.len() / 4;
    let mixed = (0..body).fold(seed, |seed, index| {
        let base = index * 4;
        let block =
            u32::from_le_bytes([data[base], data[base + 1], data[base + 2], data[base + 3]]);
        let block = block
            .wrapping_mul(MURMUR_C1)
            .rotate_left(15)
            .wrapping_mul(MURMUR_C2);
        (seed ^ block)
            .rotate_left(13)
            .wrapping_mul(5)
            .wrapping_add(MURMUR_N)
    });
    let tail = &data[body * 4..];
    let tail_key = tail.iter().enumerate().fold(0u32, |key, (index, byte)| {
        key | ((*byte as u32) << (8 * index))
    });
    let mixed = if tail.is_empty() {
        mixed
    } else {
        mixed
            ^ tail_key
                .wrapping_mul(MURMUR_C1)
                .rotate_left(15)
                .wrapping_mul(MURMUR_C2)
    };
    let mixed = mixed ^ (data.len() as u32);
    let mixed = (mixed ^ (mixed >> 16)).wrapping_mul(0x85eb_ca6b);
    let mixed = (mixed ^ (mixed >> 13)).wrapping_mul(0xc2b2_ae35);
    mixed ^ (mixed >> 16)
}

fn assemble(chunks: Vec<(&[u8; 4], Vec<u8>)>, kind: gix::hash::Kind) -> Vec<u8> {
    let num_chunks = chunks.len() as u8;
    let table_len = (chunks.len() + 1) * 12;
    let data_start = 8 + table_len;
    let hash_version = match kind {
        gix::hash::Kind::Sha256 => 2u8,
        _ => 1u8,
    };

    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(b"CGPH");
    out.push(1);
    out.push(hash_version);
    out.push(num_chunks);
    out.push(0);

    let mut offset = data_start as u64;
    chunks.iter().for_each(|(id, body)| {
        out.extend_from_slice(*id);
        out.extend_from_slice(&offset.to_be_bytes());
        offset += body.len() as u64;
    });
    out.extend_from_slice(&[0, 0, 0, 0]);
    out.extend_from_slice(&offset.to_be_bytes());

    chunks
        .iter()
        .for_each(|(_, body)| out.extend_from_slice(body));

    let mut hasher = gix_hash::hasher(kind);
    hasher.update(&out);
    let checksum = hasher
        .try_finalize()
        .expect("commit-graph checksum finalizes");
    out.extend_from_slice(checksum.as_slice());
    out
}

#[cfg(test)]
mod tests {
    use super::{GRAPH_GENERATION_MAX, Generation, build_filter, murmur3};

    #[test]
    fn succ_advances_by_one_and_orders_above_its_source() {
        let base = Generation::new(7);
        assert_eq!(base.succ(), Generation::new(8));
        assert!(base.succ() > base);
    }

    #[test]
    fn succ_saturates_at_the_numeric_ceiling() {
        assert_eq!(Generation::new(u32::MAX).succ(), Generation::new(u32::MAX));
    }

    #[test]
    fn a_child_sits_one_above_its_highest_parent() {
        let parents = [Generation::new(2), Generation::new(5), Generation::new(3)];
        let child = parents.into_iter().max().unwrap().succ();
        assert_eq!(child, Generation::new(6));
    }

    #[test]
    fn clamping_holds_the_value_at_the_format_maximum() {
        let value = Generation::new(GRAPH_GENERATION_MAX)
            .succ()
            .min(Generation::new(GRAPH_GENERATION_MAX));
        assert_eq!(value, Generation::new(GRAPH_GENERATION_MAX));
    }

    #[test]
    fn empty_change_set_is_a_single_zero_word() {
        let filter = build_filter(&[]);
        assert_eq!(filter.bytes(), &[0u8]);
    }

    #[test]
    fn overlarge_change_set_is_a_single_saturated_word() {
        let many: Vec<Vec<u8>> = (0..600).map(|n| format!("p{n}").into_bytes()).collect();
        let filter = build_filter(&many);
        assert_eq!(filter.bytes(), &[0xFFu8]);
    }

    #[test]
    fn murmur3_matches_known_vectors() {
        assert_eq!(murmur3(0, b""), 0);
        assert_eq!(murmur3(0, b"hello"), 0x248bfa47);
    }

    #[test]
    fn diff_trees_walks_a_deeply_nested_tree_without_overflowing_the_stack() {
        use knot_git::{EntryKind, Repo, StagedAction, StagedChange};
        use knot_types::Oid;

        let depth = 4000usize;
        let dir = tempfile::tempdir().unwrap();
        let git_dir = dir.path().join("deep.git");

        let build_dir = git_dir.clone();
        let tree = std::thread::Builder::new()
            .stack_size(64 * 1024 * 1024)
            .spawn(move || {
                let repo = Repo::create(&build_dir).unwrap();
                let kind = repo.object_format().kind();
                let base = Oid::from(gix::ObjectId::empty_tree(kind));
                let path = (0..depth)
                    .map(|_| "d")
                    .chain(std::iter::once("leaf.txt"))
                    .collect::<Vec<_>>()
                    .join("/");
                repo.write_staged_tree(
                    base,
                    &[StagedChange {
                        path: knot_types::RepoPath::new(path).unwrap(),
                        action: StagedAction::Put {
                            content: b"leaf\n".to_vec(),
                            kind: EntryKind::Blob,
                        },
                    }],
                )
                .unwrap()
                .object_id()
            })
            .unwrap()
            .join()
            .unwrap();

        let changed = std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(move || {
                let repo = Repo::open(&git_dir).unwrap();
                let kind = repo.object_format().kind();
                super::diff_trees(&repo.git().objects, None, Some(tree), kind).unwrap()
            })
            .unwrap()
            .join()
            .expect("diff_trees on a 256 KiB stack mustn't overflow on a 4000-deep tree");

        assert_eq!(changed.len(), 1, "the single leaf is the only changed path");
        assert_eq!(
            changed[0].iter().filter(|byte| **byte == b'/').count(),
            depth,
            "the changed path retains every nesting level"
        );
    }
}
