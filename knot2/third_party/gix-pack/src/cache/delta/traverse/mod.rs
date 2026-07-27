use std::os::unix::fs::FileExt;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use gix_features::{
    parallel::in_parallel_with_slice,
    progress::{self, DynNestedProgress, Progress},
    threading,
    threading::{Mutable, OwnShared},
};

use crate::{
    cache::delta::{Item, Tree},
    data::EntryRange,
};

mod resolve;

#[derive(Clone, Copy)]
pub(crate) struct SpillRef {
    offset: u64,
    len: usize,
}

pub struct BaseSpill {
    budget: usize,
    resident: AtomicUsize,
    peak_resident: AtomicUsize,
    spilled_bytes: AtomicU64,
    file: std::fs::File,
    write_cursor: Mutex<u64>,
}

impl BaseSpill {
    pub fn new(file: std::fs::File, budget: usize) -> Self {
        Self {
            budget,
            resident: AtomicUsize::new(0),
            peak_resident: AtomicUsize::new(0),
            spilled_bytes: AtomicU64::new(0),
            file,
            write_cursor: Mutex::new(0),
        }
    }

    pub fn peak_resident(&self) -> usize {
        self.peak_resident.load(Ordering::Relaxed)
    }

    pub fn spilled_bytes(&self) -> u64 {
        self.spilled_bytes.load(Ordering::Relaxed)
    }

    fn account_push(&self, len: usize) {
        let now = self.resident.fetch_add(len, Ordering::Relaxed) + len;
        self.peak_resident.fetch_max(now, Ordering::Relaxed);
    }

    fn account_pop_resident(&self, len: usize) {
        self.resident.fetch_sub(len, Ordering::Relaxed);
    }

    fn over_budget(&self) -> bool {
        self.resident.load(Ordering::Relaxed) > self.budget
    }

    fn spill(&self, bytes: &[u8]) -> std::io::Result<SpillRef> {
        let len = bytes.len();
        let offset = {
            let mut cursor = self.write_cursor.lock().expect("base spill cursor poisoned");
            let offset = *cursor;
            *cursor += len as u64;
            offset
        };
        self.file.write_all_at(bytes, offset)?;
        self.resident.fetch_sub(len, Ordering::Relaxed);
        self.spilled_bytes.fetch_add(len as u64, Ordering::Relaxed);
        Ok(SpillRef { offset, len })
    }

    fn reload(&self, sref: SpillRef, out: &mut Vec<u8>) -> std::io::Result<()> {
        out.resize(sref.len, 0);
        self.file.read_exact_at(out, sref.offset)
    }
}

/// Returned by [`Tree::traverse()`]
#[derive(thiserror::Error, Debug)]
#[allow(missing_docs)]
pub enum Error {
    #[error("{message}")]
    ZlibInflate {
        source: gix_features::zlib::inflate::Error,
        message: &'static str,
    },
    #[error("The resolver failed to obtain the pack entry bytes for the entry at {pack_offset}")]
    ResolveFailed { pack_offset: u64 },
    #[error(transparent)]
    EntryType(#[from] crate::data::entry::decode::Error),
    #[error("One of the object inspectors failed")]
    Inspect(#[from] Box<dyn std::error::Error + Send + Sync>),
    #[error("Interrupted")]
    Interrupted,
    #[error(
        "The base at {base_pack_offset} was referred to by a ref-delta, but it was never added to the tree as if the pack was still thin."
    )]
    OutOfPackRefDelta {
        /// The base's offset which was from a resolved ref-delta that didn't actually get added to the tree
        base_pack_offset: crate::data::Offset,
    },
    #[error("Failed to spawn thread when switching to work-stealing mode")]
    SpawnThread(#[from] std::io::Error),
    #[error("Failed to page a delta base to the memory-budget overflow file")]
    BaseSpill { source: std::io::Error },
    #[error(
        "The base object at {pack_offset} decoded as a delta, but base objects cannot be deltas"
    )]
    UnexpectedRootDelta { pack_offset: crate::data::Offset },
    #[error(transparent)]
    Delta(#[from] crate::data::delta::apply::Error),
    #[error(
        "The delta at {delta_pack_offset} declared a base of {declared} bytes but the resolved base is {actual} bytes"
    )]
    BaseSizeMismatch {
        delta_pack_offset: crate::data::Offset,
        declared: u64,
        actual: u64,
    },
    #[error(
        "The delta at {delta_pack_offset} declared a reconstructed size of {declared} bytes, over the {limit}-byte object limit"
    )]
    DeltaResultTooLarge {
        delta_pack_offset: crate::data::Offset,
        declared: u64,
        limit: u64,
    },
}

/// Additional context passed to the `inspect_object(…)` function of the [`Tree::traverse()`] method.
#[allow(missing_docs)]
pub struct Context<'a> {
    /// The pack entry describing the object
    pub entry: &'a crate::data::Entry,
    /// The offset at which `entry` ends in the pack, useful to learn about the exact range of `entry` within the pack.
    pub entry_end: u64,
    /// The decompressed object itself, ready to be decoded.
    pub decompressed: &'a [u8],
    /// The depth at which this object resides in the delta-tree. It represents the number of base objects, with 0 indicating
    /// an 'undeltified' object, and higher values indicating delta objects with the given number of bases.
    pub level: u16,
    pub object_kind: gix_object::Kind,
}

/// Options for [`Tree::traverse()`].
pub struct Options<'a, 's> {
    /// is a progress instance to track progress for each object in the traversal.
    pub object_progress: Box<dyn DynNestedProgress>,
    /// is a progress instance to track the overall progress.
    pub size_progress: &'s mut dyn Progress,
    /// If `Some`, only use the given number of threads. Otherwise, the number of threads to use will be selected based on
    /// the number of available logical cores.
    pub thread_limit: Option<usize>,
    /// Abort the operation if the value is `true`.
    pub should_interrupt: &'a AtomicBool,
    /// specifies what kind of hashes we expect to be stored in oid-delta entries, which is viable to decoding them
    /// with the correct size.
    pub object_hash: gix_hash::Kind,
    pub base_spill: Option<Arc<BaseSpill>>,
    pub collect_items: bool,
    pub max_object_bytes: Option<u64>,
}

/// The outcome of [`Tree::traverse()`]
#[allow(missing_docs)]
pub struct Outcome<T> {
    pub items: Vec<Item<T>>,
}

impl<T> Tree<T>
where
    T: Send + Sync + Clone,
{
    /// Traverse this tree of delta objects with a function `inspect_object` to process each object at will.
    ///
    /// * `should_run_in_parallel() -> bool` returns true if the underlying pack is big enough to warrant parallel traversal at all.
    /// * `resolve(EntrySlice, &R, &mut Vec<u8>) -> bool` reads the raw pack bytes for the given `EntrySlice` into the
    ///   output vector, reusing its allocation. It returns `true` if the object existed in the pack, or `false` to indicate a
    ///   resolution error, which aborts the operation.
    /// * `pack_entries_end` marks one-past-the-last byte of the last entry in the pack, as the last entries size would otherwise
    ///   be unknown as it's not part of the index file.
    /// * `inspect_object(node_data: &mut T, progress: Progress, context: Context<ThreadLocal State>) -> Result<(), CustomError>` is a function
    ///   running for each thread receiving fully decoded objects along with contextual information, which either succeeds with `Ok(())`
    ///   or returns a `CustomError`.
    ///   Note that `node_data` can be modified to allow storing maintaining computation results on a per-object basis. It should contain
    ///   its own mutable per-thread data as required.
    ///
    /// This method returns a vector of all tree items, along with their potentially modified custom node data.
    ///
    /// _Note_ that this method consumed the Tree to assure safe parallel traversal with mutation support.
    pub fn traverse<F, MBFN, E, R>(
        self,
        resolve: F,
        resolve_data: &R,
        pack_entries_end: u64,
        inspect_object: MBFN,
        Options {
            thread_limit,
            mut object_progress,
            size_progress,
            should_interrupt,
            object_hash,
            base_spill,
            collect_items,
            max_object_bytes,
        }: Options<'_, '_>,
    ) -> Result<Outcome<T>, Error>
    where
        F: Fn(EntryRange, &R, &mut Vec<u8>) -> bool + Send + Clone,
        R: Send + Sync,
        MBFN: FnMut(&mut T, &dyn Progress, Context<'_>) -> Result<(), E> + Send + Clone,
        E: std::error::Error + Send + Sync + 'static,
    {
        let num_objects = self.num_items();
        let object_counter = {
            let progress = &mut object_progress;
            progress.init(Some(num_objects), progress::count("objects"));
            progress.counter()
        };
        size_progress.init(None, progress::bytes());
        let size_counter = size_progress.counter();
        let object_progress = OwnShared::new(Mutable::new(object_progress));

        let start = std::time::Instant::now();
        let (forest, mut roots) = self.into_forest(pack_entries_end)?;
        let forest = &forest;
        let base_spill = base_spill.as_deref();
        let harvested = in_parallel_with_slice(
            &mut roots,
            thread_limit,
            {
                let object_progress = object_progress.clone();
                move |thread_index| resolve::State {
                    delta_bytes: Vec::<u8>::with_capacity(4096),
                    fully_resolved_delta_bytes: Vec::<u8>::with_capacity(4096),
                    progress: Box::new(
                        threading::lock(&object_progress)
                            .add_child(format!("thread {thread_index}")),
                    ),
                    resolve: resolve.clone(),
                    modify_base: inspect_object.clone(),
                    out: Vec::new(),
                }
            },
            {
                move |root_id: &mut u32, state, threads_left, should_interrupt| {
                    resolve::deltas(
                        object_counter.clone(),
                        size_counter.clone(),
                        *root_id,
                        forest,
                        state,
                        resolve_data,
                        object_hash.len_in_bytes(),
                        base_spill,
                        collect_items,
                        max_object_bytes,
                        threads_left,
                        should_interrupt,
                    )
                }
            },
            || {
                (!should_interrupt.load(Ordering::Relaxed))
                    .then(|| std::time::Duration::from_millis(50))
            },
            |state| state.out,
        )?;

        threading::lock(&object_progress).show_throughput(start);
        size_progress.show_throughput(start);

        Ok(Outcome {
            items: harvested.into_iter().flatten().collect(),
        })
    }
}
