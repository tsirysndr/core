use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use gix::ObjectId;
use gix::prelude::FindExt;
use gix::progress::Discard;
use gix_pack::data::{Version, output};
use knot_types::{ObjectCount, Oid};

use crate::error::{PackError, PackLimit};
use crate::ids::{Crc32, MaxObjectBytes, PackOffset};
use crate::meter::{self, PackLimits, check_depth};
use crate::resolve;

type OidStream = Box<dyn Iterator<Item = Result<ObjectId, Box<dyn Error + Send + Sync>>> + Send>;

fn odb_at(objects_dir: &Path, kind: gix::hash::Kind) -> Result<gix::odb::Handle, PackError> {
    gix::odb::at_opts(
        objects_dir,
        std::iter::empty(),
        gix::odb::store::init::Options {
            object_hash: kind,
            ..Default::default()
        },
    )
    .map_err(|error| PackError::Pack(error.to_string()))
}

pub fn write_pack(
    objects_dir: &Path,
    oids: Vec<Oid>,
    thin_bases: Option<&HashSet<Oid>>,
    out: &mut dyn Write,
    kind: gix::hash::Kind,
) -> Result<(), PackError> {
    let mut odb = odb_at(objects_dir, kind)?;
    odb.prevent_pack_unload();
    odb.refresh_never();

    let interrupt = AtomicBool::new(false);
    let permitted_bases: Option<HashSet<ObjectId>> =
        thin_bases.map(|bases| bases.iter().map(|oid| oid.object_id()).collect());
    let oids: OidStream = Box::new(oids.into_iter().map(|oid| Ok(oid.object_id())));

    let (counts, _) = output::count::objects(
        odb.clone(),
        oids,
        &Discard,
        &interrupt,
        output::count::objects::Options {
            thread_limit: knot_resource::gix_thread_limit().map(knot_resource::ThreadCount::get),
            chunk_size: 50,
            input_object_expansion: output::count::objects::ObjectExpansion::AsIs,
        },
    )
    .map_err(|error| PackError::Pack(error.to_string()))?;

    write_counts(counts, odb, permitted_bases, out, kind)
}

pub struct ExpandedPack {
    counts: Vec<output::Count>,
    odb: gix::odb::Handle,
    kind: gix::hash::Kind,
}

impl ExpandedPack {
    pub fn len(&self) -> usize {
        self.counts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.counts.is_empty()
    }
}

pub fn count_expanded(
    objects_dir: &Path,
    roots: Vec<Oid>,
    max_objects: ObjectCount,
    stall: Duration,
    kind: gix::hash::Kind,
) -> Result<ExpandedPack, PackError> {
    let mut odb = odb_at(objects_dir, kind)?;
    odb.prevent_pack_unload();
    odb.refresh_never();

    let interrupt = Arc::new(AtomicBool::new(false));
    let counter = Arc::new(AtomicUsize::new(0));
    let progress = SharedCount(Arc::clone(&counter));
    let oids: OidStream = Box::new(roots.into_iter().map(|oid| Ok(oid.object_id())));
    let result = {
        let _watchdog = Watchdog::arm(interrupt.clone(), Arc::clone(&counter), max_objects, stall);
        output::count::objects(
            Interruptible {
                inner: odb.clone(),
                flag: Arc::clone(&interrupt),
            },
            oids,
            &progress,
            &interrupt,
            output::count::objects::Options {
                thread_limit: knot_resource::gix_thread_limit()
                    .map(knot_resource::ThreadCount::get),
                chunk_size: 50,
                input_object_expansion: output::count::objects::ObjectExpansion::TreeContents,
            },
        )
    };
    let over_limit = counter.load(Ordering::Relaxed) > max_objects.get();
    let timed_out = interrupt.load(Ordering::Relaxed);
    let counts = match result {
        Ok((counts, _)) => counts,
        Err(_) if over_limit => return Err(PackError::SelectionTooLarge),
        Err(_) if timed_out => return Err(PackError::SelectionTimeout),
        Err(error) => return Err(PackError::Pack(error.to_string())),
    };
    if counts.len() > max_objects.get() {
        return Err(PackError::SelectionTooLarge);
    }
    if timed_out {
        return Err(PackError::SelectionTimeout);
    }
    Ok(ExpandedPack { counts, odb, kind })
}

pub fn write_expanded(pack: ExpandedPack, out: &mut dyn Write) -> Result<(), PackError> {
    write_counts(pack.counts, pack.odb, None, out, pack.kind)
}

fn write_counts(
    counts: Vec<output::Count>,
    odb: gix::odb::Handle,
    permitted_bases: Option<HashSet<ObjectId>>,
    out: &mut dyn Write,
    kind: gix::hash::Kind,
) -> Result<(), PackError> {
    let num_entries = counts.len() as u32;
    let allow_thin_pack = permitted_bases.is_some();
    let counted = output::entry::iter_from_counts(
        counts,
        odb.clone(),
        Box::new(Discard),
        output::entry::iter_from_counts::Options {
            thread_limit: knot_resource::gix_thread_limit().map(knot_resource::ThreadCount::get),
            mode: output::entry::iter_from_counts::Mode::PackCopyAndBaseObjects,
            allow_thin_pack,
            chunk_size: 50,
            version: Version::V2,
        },
    );

    let entries = gix::parallel::InOrderIter::from(counted).map(
        move |chunk| -> Result<Vec<output::Entry>, PackError> {
            let entries = chunk.map_err(|error| PackError::Pack(error.to_string()))?;
            entries
                .into_iter()
                .map(|entry| restrict_thin_base(&odb, permitted_bases.as_ref(), entry))
                .collect()
        },
    );

    let mut writer =
        output::bytes::FromEntriesIter::new(entries, out, num_entries, Version::V2, kind);
    writer
        .try_fold((), |(), written| written.map(|_| ()))
        .map_err(|error| PackError::Pack(error.to_string()))?;
    Ok(())
}

struct SharedCount(Arc<AtomicUsize>);

impl gix::progress::Count for SharedCount {
    fn set(&self, step: usize) {
        self.0.store(step, Ordering::Relaxed);
    }

    fn step(&self) -> usize {
        self.0.load(Ordering::Relaxed)
    }

    fn inc_by(&self, step: usize) {
        self.0.fetch_add(step, Ordering::Relaxed);
    }

    fn counter(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.0)
    }
}

#[derive(Debug)]
struct Halted;

impl std::fmt::Display for Halted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("object enumeration halted by selection budget")
    }
}

impl Error for Halted {}

#[derive(Clone)]
struct Interruptible {
    inner: gix::odb::Handle,
    flag: Arc<AtomicBool>,
}

impl gix_pack::Find for Interruptible {
    fn contains(&self, id: &gix::hash::oid) -> bool {
        self.inner.contains(id)
    }

    fn try_find_cached<'a>(
        &self,
        id: &gix::hash::oid,
        buffer: &'a mut Vec<u8>,
        pack_cache: &mut dyn gix_pack::cache::DecodeEntry,
    ) -> Result<
        Option<(gix::objs::Data<'a>, Option<gix_pack::data::entry::Location>)>,
        gix::objs::find::Error,
    > {
        if self.flag.load(Ordering::Relaxed) {
            return Err(Box::new(Halted));
        }
        self.inner.try_find_cached(id, buffer, pack_cache)
    }

    fn location_by_oid(
        &self,
        id: &gix::hash::oid,
        buf: &mut Vec<u8>,
    ) -> Option<gix_pack::data::entry::Location> {
        self.inner.location_by_oid(id, buf)
    }

    fn pack_offsets_and_oid(
        &self,
        pack_id: u32,
    ) -> Option<Vec<(gix_pack::data::Offset, gix::hash::ObjectId)>> {
        self.inner.pack_offsets_and_oid(pack_id)
    }

    fn entry_by_location(
        &self,
        location: &gix_pack::data::entry::Location,
    ) -> Option<gix_pack::find::Entry> {
        self.inner.entry_by_location(location)
    }
}

struct Watchdog {
    idle: Option<mpsc::Sender<()>>,
    watch: Option<std::thread::JoinHandle<()>>,
}

impl Watchdog {
    fn arm(
        flag: Arc<AtomicBool>,
        counter: Arc<AtomicUsize>,
        max_objects: ObjectCount,
        stall: Duration,
    ) -> Self {
        let (idle, wake) = mpsc::channel::<()>();
        let watch = std::thread::spawn(move || {
            let poll = Duration::from_millis(25).min(stall);
            let mut last = counter.load(Ordering::Relaxed);
            let mut since = Instant::now();
            loop {
                let now = counter.load(Ordering::Relaxed);
                if now != last {
                    last = now;
                    since = Instant::now();
                }
                if since.elapsed() >= stall || now > max_objects.get() {
                    flag.store(true, Ordering::Relaxed);
                    return;
                }
                if !matches!(
                    wake.recv_timeout(poll),
                    Err(mpsc::RecvTimeoutError::Timeout)
                ) {
                    return;
                }
            }
        });
        Self {
            idle: Some(idle),
            watch: Some(watch),
        }
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        self.idle.take();
        if let Some(watch) = self.watch.take() {
            let _ = watch.join();
        }
    }
}

fn restrict_thin_base(
    odb: &gix::odb::Handle,
    permitted: Option<&HashSet<ObjectId>>,
    entry: output::Entry,
) -> Result<output::Entry, PackError> {
    let base = match &entry.kind {
        output::entry::Kind::DeltaOid { id } => *id,
        _ => return Ok(entry),
    };
    if permitted.is_some_and(|bases| bases.contains(&base)) {
        return Ok(entry);
    }
    let mut buf = Vec::new();
    let object = odb
        .find(&entry.id, &mut buf)
        .map_err(|error| PackError::Pack(error.to_string()))?;
    let count = output::Count::from_data(entry.id, None);
    output::Entry::from_data(&count, &object).map_err(|error| PackError::Pack(error.to_string()))
}

pub fn index_pack(
    objects_dir: &Path,
    pack: &[u8],
    limits: &PackLimits,
    kind: gix::hash::Kind,
) -> Result<(), PackError> {
    if pack.is_empty() {
        return Ok(());
    }
    if !pack.starts_with(b"PACK") {
        return Err(PackError::Pack(
            "packfile is missing its PACK signature".to_string(),
        ));
    }
    std::fs::create_dir_all(objects_dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(objects_dir)?;
    tmp.write_all(pack)?;
    tmp.flush()?;
    let file = gix_pack::data::File::at(tmp.path(), kind)
        .map_err(|error| PackError::Pack(error.to_string()))?;
    index_pack_bounded(objects_dir, &file, limits, kind)
}

pub(crate) fn index_pack_bounded(
    objects_dir: &Path,
    pack: &gix_pack::data::File,
    limits: &PackLimits,
    kind: gix::hash::Kind,
) -> Result<(), PackError> {
    if pack.data_len() < 12 + kind.len_in_bytes() {
        return Ok(());
    }
    let thin = meter::meter_file(pack, limits, kind)?;

    let pack_dir = objects_dir.join("pack");
    std::fs::create_dir_all(&pack_dir)?;

    inline_and_index(objects_dir, pack, &pack_dir, limits, kind, thin).or_else(|_| {
        let bytes = std::fs::read(pack.path())?;
        resolve::resolve(objects_dir, &bytes, limits, kind)
    })
}

fn inline_and_index(
    objects_dir: &Path,
    pack: &gix_pack::data::File,
    pack_dir: &Path,
    limits: &PackLimits,
    kind: gix::hash::Kind,
    thin: bool,
) -> Result<(), PackError> {
    let (pack_path, pack_hash) = if thin {
        let pack_hash = inline_thin_bases(objects_dir, pack, pack_dir, kind)?;
        (
            pack_dir.join(format!("pack-{}.pack", pack_hash.to_hex())),
            pack_hash,
        )
    } else {
        let pack_hash = pack.checksum();
        let pack_path = pack_dir.join(format!("pack-{}.pack", pack_hash.to_hex()));
        let source = pack.path().to_owned();
        persist_atomic(&pack_path, |writer| {
            std::io::copy(&mut std::fs::File::open(&source)?, writer)?;
            Ok(())
        })?;
        (pack_path, pack_hash)
    };
    let pack_cleanup = RemoveOnDrop::arm(&pack_path);

    let nodes = scan_offsets(&pack_path, kind)?;
    let spool = spool_pack(
        &pack_path,
        nodes,
        kind,
        knot_resource::ingest_base_budget(),
        limits.max_object_bytes,
        None,
    )?;
    let (_present, idx_cleanup) = persist_index(pack_dir, &pack_hash, spool, kind)?;
    pack_cleanup.disarm();
    idx_cleanup.disarm();
    Ok(())
}

fn inline_thin_bases(
    objects_dir: &Path,
    pack: &gix_pack::data::File,
    pack_dir: &Path,
    kind: gix::hash::Kind,
) -> Result<ObjectId, PackError> {
    let odb = odb_at(objects_dir, kind)?;
    let staged = tempfile::NamedTempFile::new_in(pack_dir)?;
    let writer = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(staged.path())?;
    let reader = std::io::BufReader::new(std::fs::File::open(pack.path())?);
    let entries = gix_pack::data::input::BytesToEntriesIter::new_from_header(
        reader,
        gix_pack::data::input::Mode::Verify,
        gix_pack::data::input::EntryDataMode::KeepAndCrc32,
        kind,
    )
    .map_err(|error| PackError::Pack(error.to_string()))?;
    let version = entries.version();
    let lookup = gix_pack::data::input::LookupRefDeltaObjectsIter::new(entries, odb);
    let mut sink = gix_pack::data::input::EntriesToBytesIter::new(lookup, writer, version, kind);
    sink.try_for_each(|entry| {
        entry
            .map(|_| ())
            .map_err(|error| PackError::Pack(error.to_string()))
    })?;
    let pack_hash = sink
        .digest()
        .ok_or_else(|| PackError::Pack("resolved pack has no trailer".to_string()))?;
    drop(sink);

    let pack_path = pack_dir.join(format!("pack-{}.pack", pack_hash.to_hex()));
    staged
        .persist(&pack_path)
        .map_err(|error| PackError::Pack(error.to_string()))?;
    Ok(pack_hash)
}

fn scan_offsets(pack_path: &Path, kind: gix::hash::Kind) -> Result<Vec<Node>, PackError> {
    let reader = std::io::BufReader::new(std::fs::File::open(pack_path)?);
    let mut entries = gix_pack::data::input::BytesToEntriesIter::new_from_header(
        reader,
        gix_pack::data::input::Mode::Verify,
        gix_pack::data::input::EntryDataMode::Crc32,
        kind,
    )
    .map_err(|error| PackError::Pack(error.to_string()))?;
    let mut nodes: Vec<Node> = Vec::new();
    entries.try_for_each(|entry| -> Result<(), PackError> {
        let entry = entry.map_err(|error| PackError::Pack(error.to_string()))?;
        nodes.push(Node {
            offset: PackOffset::new(entry.pack_offset),
            crc32: Crc32::new(
                entry
                    .crc32
                    .ok_or_else(|| PackError::Pack("entry crc32 not computed".to_string()))?,
            ),
        });
        Ok(())
    })?;
    Ok(nodes)
}

pub(crate) struct PresentSet(gix_pack::index::File);

impl PresentSet {
    fn open(idx_path: &Path, kind: gix::hash::Kind) -> Result<Self, PackError> {
        gix_pack::index::File::at(idx_path, kind)
            .map(Self)
            .map_err(|error| PackError::Pack(error.to_string()))
    }

    pub(crate) fn contains(&self, oid: &Oid) -> bool {
        self.0.lookup(oid.object_id()).is_some()
    }

    fn sorted_offsets(&self) -> Vec<PackOffset> {
        self.0
            .sorted_offsets()
            .into_iter()
            .map(PackOffset::new)
            .collect()
    }
}

pub(crate) struct FreshClosure {
    pub self_contained: bool,
    pub present: PresentSet,
}

#[derive(Clone)]
struct Node {
    offset: PackOffset,
    crc32: Crc32,
}

const PRESENT: u8 = 1;
const REFERENCED: u8 = 2;

struct Connectivity {
    objects: scc::HashMap<Oid, u8>,
    empty_tree: Oid,
}

impl Connectivity {
    fn mark_present(&self, oid: Oid) {
        self.objects
            .entry_sync(oid)
            .and_modify(|flags| *flags |= PRESENT)
            .or_insert(PRESENT);
    }

    fn check(&self, reference: Oid) {
        if reference != self.empty_tree {
            self.objects
                .entry_sync(reference)
                .and_modify(|flags| *flags |= REFERENCED)
                .or_insert(REFERENCED);
        }
    }

    fn self_contained(&self) -> bool {
        self.objects
            .any_sync(|_, flags| *flags & REFERENCED != 0 && *flags & PRESENT == 0)
            .is_none()
    }
}

enum Scan {
    Thin,
    Limit(PackLimit),
    Pack(String),
}

const INGEST_BYTES_PER_OBJECT: u64 = 128;

fn fits_in_memory(num_objects: ObjectCount) -> bool {
    knot_resource::ingest_admits(knot_resource::PayloadBytes::new(
        (num_objects.get() as u64).saturating_mul(INGEST_BYTES_PER_OBJECT),
    ))
}

pub(crate) fn admit_ingest(
    pack: &gix_pack::data::File,
    kind: gix::hash::Kind,
) -> Result<(), PackError> {
    if pack.data_len() < 12 + kind.len_in_bytes() {
        return Ok(());
    }
    if fits_in_memory(ObjectCount::from(pack.num_objects())) {
        Ok(())
    } else {
        Err(PackError::InsufficientMemory)
    }
}

struct RemoveOnDrop(Option<PathBuf>);

impl RemoveOnDrop {
    fn arm(path: &Path) -> Self {
        Self(Some(path.to_owned()))
    }

    fn disarm(mut self) {
        self.0 = None;
    }
}

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

pub(crate) fn ingest_and_close(
    objects_dir: &Path,
    pack: &gix_pack::data::File,
    limits: &PackLimits,
    kind: gix::hash::Kind,
    base_budget: Option<usize>,
    force_external: bool,
) -> Result<Option<FreshClosure>, PackError> {
    let hash_len = kind.len_in_bytes();
    if pack.data_len() < 12 + hash_len {
        return Ok(None);
    }
    if ObjectCount::from(pack.num_objects()) > limits.max_objects {
        return Err(PackError::LimitExceeded(PackLimit::Objects));
    }

    let reader = std::io::BufReader::new(std::fs::File::open(pack.path())?);
    let mut entries = gix_pack::data::input::BytesToEntriesIter::new_from_header(
        reader,
        gix_pack::data::input::Mode::Verify,
        gix_pack::data::input::EntryDataMode::Crc32,
        kind,
    )
    .map_err(|error| PackError::Pack(error.to_string()))?;
    let mut nodes: Vec<Node> = Vec::with_capacity(pack.num_objects() as usize);
    let mut base_of: HashMap<PackOffset, PackOffset> = HashMap::new();
    let mut total_decompressed: u64 = 0;
    let mut max_object: u64 = 0;
    let scan = entries.try_for_each(|entry| -> Result<(), Scan> {
        let entry = entry.map_err(|error| Scan::Pack(error.to_string()))?;
        match entry.header {
            gix_pack::data::entry::Header::RefDelta { .. } => return Err(Scan::Thin),
            gix_pack::data::entry::Header::OfsDelta { base_distance } => {
                let pack_offset = PackOffset::new(entry.pack_offset);
                let base = pack_offset
                    .checked_sub_distance(base_distance)
                    .ok_or_else(|| Scan::Pack("ofs-delta base out of range".to_string()))?;
                base_of.insert(pack_offset, base);
            }
            _ => {}
        }
        if limits.max_object_bytes.exceeded_by(entry.decompressed_size) {
            return Err(Scan::Limit(PackLimit::ObjectBytes));
        }
        max_object = max_object.max(entry.decompressed_size);
        total_decompressed = total_decompressed
            .checked_add(entry.decompressed_size)
            .ok_or_else(|| Scan::Pack("decompressed size overflow".to_string()))?;
        if limits.max_total_bytes.exceeded_by(total_decompressed) {
            return Err(Scan::Limit(PackLimit::TotalBytes));
        }
        nodes.push(Node {
            offset: PackOffset::new(entry.pack_offset),
            crc32: Crc32::new(
                entry
                    .crc32
                    .ok_or_else(|| Scan::Pack("entry crc32 not computed".to_string()))?,
            ),
        });
        Ok(())
    });
    match scan {
        Ok(()) => {}
        Err(Scan::Thin) => return Ok(None),
        Err(Scan::Limit(limit)) => return Err(PackError::LimitExceeded(limit)),
        Err(Scan::Pack(message)) => return Err(PackError::Pack(message)),
    }
    check_depth(&base_of, limits.max_delta_depth)?;
    drop(base_of);
    let entry_count = nodes.len();

    let base_cache = knot_resource::ingest_base_budget().unwrap_or(0) as u64;
    let working_set = (entry_count as u64)
        .saturating_mul(INGEST_BYTES_PER_OBJECT)
        .saturating_add(base_cache)
        .saturating_add(max_object);
    if !knot_resource::ingest_admits_churn(
        knot_resource::WorkingSetBytes::new(working_set),
        knot_resource::ChurnBytes::new(total_decompressed),
    ) {
        return Err(PackError::InsufficientMemory);
    }

    let pack_hash = pack.checksum();
    let pack_dir = objects_dir.join("pack");
    std::fs::create_dir_all(&pack_dir)?;
    let stem = format!("pack-{}", pack_hash.to_hex());
    let pack_path = pack_dir.join(format!("{stem}.pack"));
    let source = pack.path().to_owned();
    persist_atomic(&pack_path, |writer| {
        let mut reader = std::fs::File::open(&source)?;
        std::io::copy(&mut reader, writer)?;
        Ok(())
    })?;
    let pack_cleanup = RemoveOnDrop::arm(&pack_path);

    let externalize = force_external
        || knot_resource::externalize_connectivity(knot_resource::ConnectivityObjects::new(
            entry_count as u64,
        ));
    let connectivity = (!externalize).then(|| Connectivity {
        objects: scc::HashMap::with_capacity(entry_count),
        empty_tree: Oid::from(ObjectId::empty_tree(kind)),
    });
    let spool = spool_pack(
        &pack_path,
        nodes,
        kind,
        base_budget,
        limits.max_object_bytes,
        connectivity.as_ref(),
    )?;
    let self_contained_in_ram = connectivity.as_ref().map(Connectivity::self_contained);
    drop(connectivity);
    let (present, idx_cleanup) = persist_index(&pack_dir, &pack_hash, spool, kind)?;

    let self_contained = match self_contained_in_ram {
        Some(value) => value,
        None => verify_connectivity_retraverse(
            &pack_path,
            &present,
            kind,
            base_budget,
            limits.max_object_bytes,
        )?,
    };

    pack_cleanup.disarm();
    idx_cleanup.disarm();
    Ok(Some(FreshClosure {
        self_contained,
        present,
    }))
}

fn spool_pack(
    pack_path: &Path,
    nodes: Vec<Node>,
    kind: gix::hash::Kind,
    base_budget: Option<usize>,
    max_object_bytes: MaxObjectBytes,
    connectivity: Option<&Connectivity>,
) -> Result<crate::idxwrite::Spool, PackError> {
    let interrupt = AtomicBool::new(false);
    let tree = gix_pack::cache::delta::Tree::from_offsets_in_pack(
        pack_path,
        nodes.into_iter(),
        &|node: &Node| node.offset.get(),
        &|_id| None,
        &mut Discard,
        &interrupt,
        kind,
    )
    .map_err(|error| PackError::Pack(error.to_string()))?;
    let stored = gix_pack::data::File::at(pack_path, kind)
        .map_err(|error| PackError::Pack(error.to_string()))?;
    let spool = crate::idxwrite::Spool::new(kind)?;
    run_ingest_traverse(
        tree,
        &stored,
        &interrupt,
        base_budget,
        max_object_bytes,
        kind,
        |node: &mut Node, _progress, context| harvest(node, context, kind, connectivity, &spool),
    )?;
    Ok(spool)
}

fn persist_index(
    pack_dir: &Path,
    pack_hash: &ObjectId,
    spool: crate::idxwrite::Spool,
    kind: gix::hash::Kind,
) -> Result<(PresentSet, RemoveOnDrop), PackError> {
    let idx_path = pack_dir.join(format!("pack-{}.idx", pack_hash.to_hex()));
    persist_atomic(&idx_path, |writer| {
        crate::idxwrite::write_v2_index(writer, &spool, pack_hash, kind).map(|_| ())
    })?;
    let idx_cleanup = RemoveOnDrop::arm(&idx_path);
    drop(spool);
    let present = PresentSet::open(&idx_path, kind)?;
    Ok((present, idx_cleanup))
}

// he wishes he was on the farm already
fn harvest(
    node: &Node,
    context: gix_pack::cache::delta::traverse::Context<'_>,
    kind: gix::hash::Kind,
    connectivity: Option<&Connectivity>,
    spool: &crate::idxwrite::Spool,
) -> Result<(), PackError> {
    let id = gix::objs::compute_hash(kind, context.object_kind, context.decompressed)
        .map_err(|error| PackError::Pack(error.to_string()))?;
    spool.push(id, node.crc32, node.offset)?;
    if let Some(connectivity) = connectivity {
        connectivity.mark_present(Oid::from(id));
        parse_references(
            context.object_kind,
            context.decompressed,
            kind,
            &mut |reference| connectivity.check(reference),
        )?;
    }
    Ok(())
}

fn parse_references(
    object_kind: gix::object::Kind,
    bytes: &[u8],
    kind: gix::hash::Kind,
    check: &mut dyn FnMut(Oid),
) -> Result<(), PackError> {
    match object_kind {
        gix::object::Kind::Blob => Ok(()),
        gix::object::Kind::Tree => harvest_tree(bytes, kind, check),
        gix::object::Kind::Commit => harvest_commit(bytes, kind, check),
        gix::object::Kind::Tag => harvest_tag(bytes, kind, check),
    }
}

fn harvest_tree(
    bytes: &[u8],
    kind: gix::hash::Kind,
    check: &mut dyn FnMut(Oid),
) -> Result<(), PackError> {
    gix::objs::TreeRefIter::from_bytes(bytes, kind).try_for_each(|entry| {
        let entry = entry.map_err(|error| PackError::Pack(error.to_string()))?;
        if !entry.mode.is_commit() {
            check(Oid::from(entry.oid.to_owned()));
        }
        Ok(())
    })
}

fn harvest_commit(
    bytes: &[u8],
    kind: gix::hash::Kind,
    check: &mut dyn FnMut(Oid),
) -> Result<(), PackError> {
    let mut iter = gix::objs::CommitRefIter::from_bytes(bytes, kind);
    let tree = iter
        .tree_id()
        .map_err(|error| PackError::Pack(error.to_string()))?;
    std::iter::once(tree)
        .chain(iter.parent_ids())
        .for_each(|oid| check(Oid::from(oid)));
    Ok(())
}

fn harvest_tag(
    bytes: &[u8],
    kind: gix::hash::Kind,
    check: &mut dyn FnMut(Oid),
) -> Result<(), PackError> {
    let target = gix::objs::TagRefIter::from_bytes(bytes, kind)
        .target_id()
        .map_err(|error| PackError::Pack(error.to_string()))?;
    check(Oid::from(target));
    Ok(())
}

fn verify_connectivity_retraverse(
    pack_path: &Path,
    present: &PresentSet,
    kind: gix::hash::Kind,
    base_budget: Option<usize>,
    max_object_bytes: MaxObjectBytes,
) -> Result<bool, PackError> {
    let interrupt = AtomicBool::new(false);
    let tree = gix_pack::cache::delta::Tree::from_offsets_in_pack(
        pack_path,
        present.sorted_offsets().into_iter(),
        &|offset: &PackOffset| offset.get(),
        &|_id| None,
        &mut Discard,
        &interrupt,
        kind,
    )
    .map_err(|error| PackError::Pack(error.to_string()))?;
    let stored = gix_pack::data::File::at(pack_path, kind)
        .map_err(|error| PackError::Pack(error.to_string()))?;
    let empty_tree = Oid::from(ObjectId::empty_tree(kind));
    let missing = AtomicBool::new(false);
    run_ingest_traverse(
        tree,
        &stored,
        &interrupt,
        base_budget,
        max_object_bytes,
        kind,
        |_offset: &mut PackOffset, _progress, context| {
            parse_references(
                context.object_kind,
                context.decompressed,
                kind,
                &mut |reference| {
                    if reference != empty_tree && !present.contains(&reference) {
                        missing.store(true, Ordering::Relaxed);
                    }
                },
            )
        },
    )?;
    Ok(!missing.load(Ordering::Relaxed))
}

fn run_ingest_traverse<T, H>(
    tree: gix_pack::cache::delta::Tree<T>,
    stored: &gix_pack::data::File,
    interrupt: &AtomicBool,
    base_budget: Option<usize>,
    max_object_bytes: MaxObjectBytes,
    kind: gix::hash::Kind,
    harvest: H,
) -> Result<(), PackError>
where
    T: Send + Sync + Clone,
    H: FnMut(
            &mut T,
            &dyn gix::progress::Progress,
            gix_pack::cache::delta::traverse::Context<'_>,
        ) -> Result<(), PackError>
        + Send
        + Clone,
{
    let base_spill = match base_budget {
        Some(budget) => Some(Arc::new(gix_pack::cache::delta::traverse::BaseSpill::new(
            tempfile::tempfile()?,
            budget,
        ))),
        None => None,
    };
    tree.traverse(
        |range: gix_pack::data::EntryRange, source: &gix_pack::data::File, buf: &mut Vec<u8>| {
            source.read_into(range, buf)
        },
        stored,
        stored.pack_end() as u64,
        harvest,
        gix_pack::cache::delta::traverse::Options {
            object_progress: Box::new(Discard),
            size_progress: &mut Discard,
            thread_limit: Some(knot_resource::ingest_thread_limit()),
            should_interrupt: interrupt,
            object_hash: kind,
            base_spill,
            collect_items: false,
            max_object_bytes: Some(max_object_bytes.get()),
        },
    )
    .map(|_| ())
    .map_err(|error| PackError::Pack(error.to_string()))
}

fn persist_atomic(
    path: &Path,
    write: impl FnOnce(&mut dyn Write) -> Result<(), PackError>,
) -> Result<(), PackError> {
    let dir = path
        .parent()
        .ok_or_else(|| PackError::Pack("object path has no parent".to_string()))?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    let mut writer = std::io::BufWriter::new(tmp.as_file_mut());
    write(&mut writer)?;
    writer.flush()?;
    drop(writer);
    tmp.as_file().sync_all()?;
    tmp.persist(path)
        .map_err(|error| PackError::Pack(error.to_string()))?;
    std::fs::File::open(dir)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use gix_pack::Find;

    use super::*;

    #[test]
    fn interruptible_find_errors_once_the_flag_is_set() {
        let dir = tempfile::tempdir().unwrap();
        let flag = Arc::new(AtomicBool::new(false));
        let find = Interruptible {
            inner: gix::odb::at(dir.path()).unwrap(),
            flag: Arc::clone(&flag),
        };
        let absent = Oid::null().object_id();
        let mut buf = Vec::new();

        assert!(
            find.try_find(&absent, &mut buf).unwrap().is_none(),
            "before budget trips, lookup of an absent object is a plain miss"
        );

        flag.store(true, Ordering::Relaxed);
        assert!(
            find.try_find(&absent, &mut buf).is_err(),
            "once tripped, every decode errors, which is what aborts a breadthfirst tree walk \
             mid-closure instead of waiting for the next commit-root boundary"
        );
    }
}
