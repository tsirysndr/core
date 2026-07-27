use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

use gix_features::{progress::Progress, threading, zlib};

use crate::{
    cache::delta::{
        Item,
        traverse::{Context, Error},
        tree::Forest,
    },
    data,
    data::EntryRange,
};

struct Pending {
    level: u16,
    node_id: u32,
    entry: data::Entry,
    entry_end: u64,
    base_bytes: Vec<u8>,
    object_kind: gix_object::Kind,
    spill_ref: Option<super::SpillRef>,
}

fn enforce_budget(spill: &super::BaseSpill, stack: &mut [Pending]) -> Result<(), Error> {
    (0..stack.len()).try_for_each(|index| -> Result<(), Error> {
        if spill.over_budget()
            && stack[index].spill_ref.is_none()
            && !stack[index].base_bytes.is_empty()
        {
            let bytes = std::mem::take(&mut stack[index].base_bytes);
            let sref = spill
                .spill(&bytes)
                .map_err(|source| Error::BaseSpill { source })?;
            stack[index].spill_ref = Some(sref);
        }
        Ok(())
    })
}

fn restore_base_bytes(
    spill: Option<&super::BaseSpill>,
    pending: &mut Pending,
) -> Result<(), Error> {
    if let Some(spill) = spill {
        match pending.spill_ref.take() {
            Some(sref) => spill
                .reload(sref, &mut pending.base_bytes)
                .map_err(|source| Error::BaseSpill { source })?,
            None => spill.account_pop_resident(pending.base_bytes.len()),
        }
    }
    Ok(())
}

pub(super) struct State<F, MBFN, T: Send> {
    pub delta_bytes: Vec<u8>,
    pub fully_resolved_delta_bytes: Vec<u8>,
    pub progress: Box<dyn Progress>,
    pub resolve: F,
    pub modify_base: MBFN,
    pub out: Vec<Item<T>>,
}

pub(super) fn deltas<T, F, MBFN, E, R>(
    objects: gix_features::progress::StepShared,
    size: gix_features::progress::StepShared,
    root_id: u32,
    forest: &Forest<T>,
    State {
        delta_bytes,
        fully_resolved_delta_bytes,
        progress,
        resolve,
        modify_base,
        out,
    }: &mut State<F, MBFN, T>,
    resolve_data: &R,
    hash_len: usize,
    spill: Option<&super::BaseSpill>,
    collect_items: bool,
    max_object_bytes: Option<u64>,
    threads_left: &AtomicIsize,
    should_interrupt: &AtomicBool,
) -> Result<(), Error>
where
    T: Send + Sync + Clone,
    R: Send + Sync,
    F: Fn(EntryRange, &R, &mut Vec<u8>) -> bool + Send + Clone,
    MBFN: FnMut(&mut T, &dyn Progress, Context<'_>) -> Result<(), E> + Send + Clone,
    E: std::error::Error + Send + Sync + 'static,
{
    let mut inflate = zlib::Inflate::default();
    let mut decompress = decompressor(&*resolve, resolve_data, hash_len, &mut inflate);

    let mut stack: Vec<Pending> = Vec::new();

    let mut root_bytes = Vec::new();
    let (root_entry, root_end) = decompress(forest.entry_slice(root_id), &mut root_bytes)?;
    let object_kind = root_entry
        .header
        .as_kind()
        .ok_or(Error::UnexpectedRootDelta {
            pack_offset: forest.offset(root_id),
        })?;
    let mut root_data = forest.data[root_id as usize].clone();
    apply_base(
        modify_base,
        &mut root_data,
        progress,
        &root_entry,
        root_end,
        &root_bytes,
        0,
        object_kind,
    )?;
    objects.fetch_add(1, Ordering::Relaxed);
    size.fetch_add(root_bytes.len(), Ordering::Relaxed);
    if collect_items {
        out.push(forest.item(root_id, root_data));
    }
    expand(
        forest,
        forest.children(root_id),
        &root_bytes,
        object_kind,
        0,
        &mut stack,
        delta_bytes,
        fully_resolved_delta_bytes,
        modify_base,
        &**progress,
        out,
        &mut decompress,
        &objects,
        &size,
        spill,
        collect_items,
        max_object_bytes,
    )?;
    drop(root_bytes);
    if let Some(spill) = spill {
        if spill.over_budget() {
            enforce_budget(spill, &mut stack)?;
        }
    }

    loop {
        if stack.len() > 1 {
            if let Ok(initial_threads) =
                threads_left.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |threads_available| {
                    (threads_available > 0).then_some(0)
                })
            {
                *delta_bytes = Vec::new();
                *fully_resolved_delta_bytes = Vec::new();
                return deltas_mt(
                    initial_threads,
                    stack,
                    objects,
                    size,
                    &**progress,
                    resolve.clone(),
                    resolve_data,
                    modify_base.clone(),
                    hash_len,
                    spill,
                    collect_items,
                    max_object_bytes,
                    forest,
                    out,
                    threads_left,
                    should_interrupt,
                );
            }
        }

        let Some(mut pending) = stack.pop() else {
            break;
        };
        restore_base_bytes(spill, &mut pending)?;
        let Pending {
            level,
            node_id,
            entry,
            entry_end,
            base_bytes,
            object_kind,
            ..
        } = pending;
        if should_interrupt.load(Ordering::Relaxed) {
            return Err(Error::Interrupted);
        }

        let mut node_data = forest.data[node_id as usize].clone();
        apply_base(
            modify_base,
            &mut node_data,
            progress,
            &entry,
            entry_end,
            &base_bytes,
            level,
            object_kind,
        )?;
        objects.fetch_add(1, Ordering::Relaxed);
        size.fetch_add(base_bytes.len(), Ordering::Relaxed);
        if collect_items {
            out.push(forest.item(node_id, node_data));
        }
        expand(
            forest,
            forest.children(node_id),
            &base_bytes,
            object_kind,
            level,
            &mut stack,
            delta_bytes,
            fully_resolved_delta_bytes,
            modify_base,
            &**progress,
            out,
            &mut decompress,
            &objects,
            &size,
            spill,
            collect_items,
            max_object_bytes,
        )?;
        drop(base_bytes);
        if let Some(spill) = spill {
            if spill.over_budget() {
                enforce_budget(spill, &mut stack)?;
            }
        }
    }

    Ok(())
}

/// * `initial_threads` is the threads we may spawn, not accounting for our own thread which is still considered used by the parent
///   system. Since this thread will take a controlling function, we may spawn one more than that. In threaded mode, we will finish
///   all remaining work.
#[allow(clippy::too_many_arguments)]
fn deltas_mt<T, F, MBFN, E, R>(
    mut threads_to_create: isize,
    stack: Vec<Pending>,
    objects: gix_features::progress::StepShared,
    size: gix_features::progress::StepShared,
    progress: &dyn Progress,
    resolve: F,
    resolve_data: &R,
    modify_base: MBFN,
    hash_len: usize,
    spill: Option<&super::BaseSpill>,
    collect_items: bool,
    max_object_bytes: Option<u64>,
    forest: &Forest<T>,
    accumulator: &mut Vec<Item<T>>,
    threads_left: &AtomicIsize,
    should_interrupt: &AtomicBool,
) -> Result<(), Error>
where
    T: Send + Sync + Clone,
    R: Send + Sync,
    F: Fn(EntryRange, &R, &mut Vec<u8>) -> bool + Send + Clone,
    MBFN: FnMut(&mut T, &dyn Progress, Context<'_>) -> Result<(), E> + Send + Clone,
    E: std::error::Error + Send + Sync + 'static,
{
    let stack = gix_features::threading::Mutable::new(stack);
    threads_to_create += 1; // ourselves
    let mut returned_ourselves = false;

    gix_features::parallel::threads(|s| -> Result<(), Error> {
        let mut threads = Vec::new();
        let poll_interval = std::time::Duration::from_millis(100);
        loop {
            for tid in 0..threads_to_create {
                let thread = gix_features::parallel::build_thread()
                    .name(format!("gix-pack.traverse_deltas.{tid}"))
                    .spawn_scoped(s, {
                        let stack = &stack;
                        let resolve = resolve.clone();
                        let mut modify_base = modify_base.clone();
                        let objects = &objects;
                        let size = &size;

                        move || -> Result<Vec<Item<T>>, Error> {
                            let mut collected: Vec<Item<T>> = Vec::new();
                            let mut delta_bytes = Vec::new();
                            let mut fully_resolved_delta_bytes = Vec::new();
                            let mut inflate = zlib::Inflate::default();
                            let mut decompress =
                                decompressor(&resolve, resolve_data, hash_len, &mut inflate);

                            loop {
                                let Some(mut pending) = threading::lock(stack).pop() else {
                                    break;
                                };
                                restore_base_bytes(spill, &mut pending)?;
                                let Pending {
                                    level,
                                    node_id,
                                    entry,
                                    entry_end,
                                    base_bytes,
                                    object_kind,
                                    ..
                                } = pending;
                                if should_interrupt.load(Ordering::Relaxed) {
                                    return Err(Error::Interrupted);
                                }

                                let mut node_data = forest.data[node_id as usize].clone();
                                apply_base(
                                    &mut modify_base,
                                    &mut node_data,
                                    progress,
                                    &entry,
                                    entry_end,
                                    &base_bytes,
                                    level,
                                    object_kind,
                                )?;
                                objects.fetch_add(1, Ordering::Relaxed);
                                size.fetch_add(base_bytes.len(), Ordering::Relaxed);
                                if collect_items {
                                    collected.push(forest.item(node_id, node_data));
                                }
                                let mut produced: Vec<Pending> = Vec::new();
                                expand(
                                    forest,
                                    forest.children(node_id),
                                    &base_bytes,
                                    object_kind,
                                    level,
                                    &mut produced,
                                    &mut delta_bytes,
                                    &mut fully_resolved_delta_bytes,
                                    &mut modify_base,
                                    progress,
                                    &mut collected,
                                    &mut decompress,
                                    objects,
                                    size,
                                    spill,
                                    collect_items,
                                    max_object_bytes,
                                )?;
                                drop(base_bytes);
                                if !produced.is_empty() {
                                    let mut guard = threading::lock(stack);
                                    guard.append(&mut produced);
                                    if let Some(spill) = spill {
                                        if spill.over_budget() {
                                            enforce_budget(spill, &mut guard[..])?;
                                        }
                                    }
                                }
                            }
                            Ok(collected)
                        }
                    })?;
                threads.push(thread);
            }
            if threads_left
                .fetch_update(
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                    |threads_available: isize| {
                        (threads_available > 0).then(|| {
                            threads_to_create =
                                threads_available.min(threading::lock(&stack).len() as isize);
                            threads_available - threads_to_create
                        })
                    },
                )
                .is_err()
            {
                threads_to_create = 0;
            }

            // What we really want to do is either wait for one of our threads to go down
            // or for another scheduled thread to become available. Unfortunately we can't do that,
            // but may instead find a good way to set the polling interval instead of hard-coding it.
            std::thread::sleep(poll_interval);
            // Get out of threads are already starving or they would be starving soon as no work is left.
            //
            // Lint: ScopedJoinHandle is not the same depending on active features and is not exposed in some cases.
            #[allow(clippy::redundant_closure_for_method_calls)]
            if threads.iter().any(|t| t.is_finished()) {
                let mut running_threads = Vec::new();
                for thread in threads.drain(..) {
                    if thread.is_finished() {
                        match thread.join() {
                            Ok(Err(err)) => return Err(err),
                            Ok(Ok(collected)) => {
                                accumulator.extend(collected);
                                if !returned_ourselves {
                                    returned_ourselves = true;
                                } else {
                                    threads_left.fetch_add(1, Ordering::SeqCst);
                                }
                            }
                            Err(err) => {
                                std::panic::resume_unwind(err);
                            }
                        }
                    } else {
                        running_threads.push(thread);
                    }
                }
                if running_threads.is_empty() && threading::lock(&stack).is_empty() {
                    break;
                }
                threads = running_threads;
            }
        }

        Ok(())
    })
}

fn decompressor<'a, F, R>(
    resolve: &'a F,
    resolve_data: &'a R,
    hash_len: usize,
    inflate: &'a mut zlib::Inflate,
) -> impl FnMut(EntryRange, &mut Vec<u8>) -> Result<(data::Entry, u64), Error> + 'a
where
    F: Fn(EntryRange, &R, &mut Vec<u8>) -> bool,
    R: Sync,
{
    let mut raw = Vec::new();
    move |slice: EntryRange, out: &mut Vec<u8>| -> Result<(data::Entry, u64), Error> {
        if !resolve(slice.clone(), resolve_data, &mut raw) {
            return Err(Error::ResolveFailed {
                pack_offset: slice.start,
            });
        }
        let entry = data::Entry::from_bytes(&raw, slice.start, hash_len)?;
        let compressed = &raw[entry.header_size()..];
        let decompressed_len = entry.decompressed_size as usize;
        decompress_all_at_once_with(inflate, compressed, decompressed_len, out)?;
        Ok((entry, slice.end))
    }
}

fn apply_base<T, MBFN, E>(
    modify_base: &mut MBFN,
    data: &mut T,
    progress: &dyn Progress,
    entry: &data::Entry,
    entry_end: u64,
    decompressed: &[u8],
    level: u16,
    object_kind: gix_object::Kind,
) -> Result<(), Error>
where
    MBFN: FnMut(&mut T, &dyn Progress, Context<'_>) -> Result<(), E>,
    E: std::error::Error + Send + Sync + 'static,
{
    modify_base(
        data,
        progress,
        Context {
            entry,
            entry_end,
            decompressed,
            level,
            object_kind,
        },
    )
    .map_err(|err| Box::new(err) as Box<dyn std::error::Error + Send + Sync>)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn expand<T, F, MBFN, E>(
    forest: &Forest<T>,
    children: &[u32],
    base_bytes: &[u8],
    object_kind: gix_object::Kind,
    base_level: u16,
    stack: &mut Vec<Pending>,
    delta_bytes: &mut Vec<u8>,
    fully_resolved_delta_bytes: &mut Vec<u8>,
    modify_base: &mut MBFN,
    progress: &dyn Progress,
    sink: &mut Vec<Item<T>>,
    decompress: &mut F,
    objects: &gix_features::progress::StepShared,
    size: &gix_features::progress::StepShared,
    spill: Option<&super::BaseSpill>,
    collect_items: bool,
    max_object_bytes: Option<u64>,
) -> Result<(), Error>
where
    T: Clone,
    F: FnMut(EntryRange, &mut Vec<u8>) -> Result<(data::Entry, u64), Error>,
    MBFN: FnMut(&mut T, &dyn Progress, Context<'_>) -> Result<(), E>,
    E: std::error::Error + Send + Sync + 'static,
{
    for &child_id in children {
        let (child_entry, entry_end) = decompress(forest.entry_slice(child_id), delta_bytes)?;
        let (base_size, consumed) = data::delta::decode_header_size(delta_bytes)?;
        let mut header_ofs = consumed;
        if base_bytes.len() != base_size as usize {
            return Err(Error::BaseSizeMismatch {
                delta_pack_offset: forest.offset(child_id),
                declared: base_size,
                actual: base_bytes.len() as u64,
            });
        }
        let (result_size, consumed) = data::delta::decode_header_size(&delta_bytes[consumed..])?;
        header_ofs += consumed;

        if let Some(limit) = max_object_bytes {
            if result_size > limit {
                return Err(Error::DeltaResultTooLarge {
                    delta_pack_offset: forest.offset(child_id),
                    declared: result_size,
                    limit,
                });
            }
        }
        fully_resolved_delta_bytes.resize(result_size as usize, 0);
        data::delta::apply(
            base_bytes,
            fully_resolved_delta_bytes,
            &delta_bytes[header_ofs..],
        )?;

        if forest.children(child_id).is_empty() {
            let mut child_data = forest.data[child_id as usize].clone();
            apply_base(
                modify_base,
                &mut child_data,
                progress,
                &child_entry,
                entry_end,
                fully_resolved_delta_bytes,
                base_level + 1,
                object_kind,
            )?;
            objects.fetch_add(1, Ordering::Relaxed);
            size.fetch_add(base_bytes.len(), Ordering::Relaxed);
            if collect_items {
                sink.push(forest.item(child_id, child_data));
            }
        } else {
            let base_bytes = std::mem::take(fully_resolved_delta_bytes);
            if let Some(spill) = spill {
                spill.account_push(base_bytes.len());
            }
            stack.push(Pending {
                level: base_level + 1,
                node_id: child_id,
                entry: child_entry,
                entry_end,
                base_bytes,
                object_kind,
                spill_ref: None,
            });
        }
    }
    Ok(())
}

fn decompress_all_at_once_with(
    inflate: &mut zlib::Inflate,
    b: &[u8],
    decompressed_len: usize,
    out: &mut Vec<u8>,
) -> Result<(), Error> {
    out.resize(decompressed_len, 0);
    inflate.reset();
    inflate.once(b, out).map_err(|err| Error::ZlibInflate {
        source: err,
        message: "Failed to decompress entry",
    })?;
    Ok(())
}
