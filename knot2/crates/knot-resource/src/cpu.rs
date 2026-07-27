use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ThreadCount(usize);

impl ThreadCount {
    pub const fn new(count: usize) -> Self {
        Self(if count == 0 { 1 } else { count })
    }

    pub const fn get(self) -> usize {
        self.0
    }
}

knot_types::scalar_newtype! {
    struct WorkUnits(usize);
}

struct Budget {
    ceiling: usize,
    available: AtomicUsize,
}

static BUDGET: OnceLock<Budget> = OnceLock::new();

fn detected_threads() -> ThreadCount {
    ThreadCount::new(
        std::thread::available_parallelism()
            .map(std::num::NonZeroUsize::get)
            .unwrap_or(1),
    )
}

pub(crate) fn install(ceiling: ThreadCount) -> ThreadCount {
    let budget = BUDGET.get_or_init(|| Budget {
        ceiling: ceiling.get(),
        available: AtomicUsize::new(ceiling.get().saturating_sub(1)),
    });
    ThreadCount::new(budget.ceiling)
}

fn budget() -> &'static Budget {
    BUDGET.get_or_init(|| {
        let ceiling = detected_threads().get();
        Budget {
            ceiling,
            available: AtomicUsize::new(ceiling.saturating_sub(1)),
        }
    })
}

pub fn threads() -> ThreadCount {
    ThreadCount::new(budget().ceiling)
}

pub fn gix_thread_limit() -> Option<ThreadCount> {
    let budget = budget();
    (budget.ceiling < detected_threads().get()).then_some(ThreadCount::new(budget.ceiling))
}

pub(crate) fn ceiling() -> usize {
    budget().ceiling
}

static SATURATE: AtomicUsize = AtomicUsize::new(0);

pub struct Saturate(());

impl Drop for Saturate {
    fn drop(&mut self) {
        SATURATE.fetch_sub(1, Ordering::Relaxed);
    }
}

// the eat my machine button
pub fn saturate() -> Saturate {
    SATURATE.fetch_add(1, Ordering::Relaxed);
    Saturate(())
}

struct Lease {
    extra: usize,
    saturated: bool,
}

impl Lease {
    fn none() -> Self {
        Self {
            extra: 0,
            saturated: false,
        }
    }

    fn lanes(&self) -> usize {
        self.extra + 1
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        if !self.saturated && self.extra > 0 {
            budget().available.fetch_add(self.extra, Ordering::AcqRel);
        }
    }
}

fn lease(units: WorkUnits) -> Lease {
    let budget = budget();
    let want = units
        .get()
        .saturating_sub(1)
        .min(budget.ceiling.saturating_sub(1));
    if want == 0 {
        return Lease::none();
    }
    if SATURATE.load(Ordering::Relaxed) > 0 {
        return Lease {
            extra: want,
            saturated: true,
        };
    }
    let mut available = budget.available.load(Ordering::Relaxed);
    loop {
        let grant = want.min(available);
        if grant == 0 {
            return Lease::none();
        }
        match budget.available.compare_exchange_weak(
            available,
            available - grant,
            Ordering::AcqRel,
            Ordering::Relaxed,
        ) {
            Ok(_) => {
                return Lease {
                    extra: grant,
                    saturated: false,
                };
            }
            Err(observed) => available = observed,
        }
    }
}

pub fn map_spans<R, E, F>(len: usize, f: F) -> Result<Vec<R>, E>
where
    R: Send,
    E: Send,
    F: Fn(usize, usize) -> Result<Vec<R>, E> + Sync,
{
    if len == 0 {
        return Ok(Vec::new());
    }
    let f = &f;
    let lease = lease(WorkUnits::new(len));
    let lanes = lease.lanes().min(len);
    if lanes <= 1 {
        return f(0, len);
    }
    let chunk = len.div_ceil(lanes);
    let spans: Vec<(usize, usize)> = (0..lanes)
        .map(|lane| (lane * chunk, ((lane + 1) * chunk).min(len)))
        .filter(|(start, end)| start < end)
        .collect();
    let (head, tail) = spans
        .split_first()
        .expect("a positive lane count yields at least one span");
    let ordered: Vec<Result<Vec<R>, E>> = std::thread::scope(|scope| {
        let handles: Vec<_> = tail
            .iter()
            .map(|&(start, end)| scope.spawn(move || f(start, end)))
            .collect();
        let head = f(head.0, head.1);
        std::iter::once(head)
            .chain(
                handles
                    .into_iter()
                    .map(|handle| handle.join().expect("resource worker panicked")),
            )
            .collect()
    });
    ordered.into_iter().try_fold(Vec::new(), |mut acc, part| {
        acc.extend(part?);
        Ok(acc)
    })
}

pub fn map_chunks<T, R, E, F>(items: &[T], f: F) -> Result<Vec<R>, E>
where
    T: Sync,
    R: Send,
    E: Send,
    F: Fn(&[T]) -> Result<Vec<R>, E> + Sync,
{
    map_spans(items.len(), |start, end| f(&items[start..end]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_positive_span_count_covers_every_index_in_order() {
        let doubled = map_spans::<usize, (), _>(1000, |start, end| {
            Ok((start..end).map(|value| value * 2).collect())
        })
        .unwrap();
        assert_eq!(doubled.len(), 1000);
        assert!(
            doubled
                .iter()
                .enumerate()
                .all(|(index, value)| *value == index * 2)
        );
    }

    #[test]
    fn an_empty_span_produces_nothing() {
        assert_eq!(
            map_spans::<usize, (), _>(0, |_, _| Ok(vec![1])).unwrap(),
            Vec::<usize>::new()
        );
    }

    #[test]
    fn a_worker_error_propagates() {
        let outcome =
            map_spans::<usize, &str, _>(
                500,
                |start, _| {
                    if start == 0 { Err("boom") } else { Ok(vec![]) }
                },
            );
        assert_eq!(outcome, Err("boom"));
    }

    #[test]
    fn chunks_preserve_element_order() {
        let items: Vec<usize> = (0..777).collect();
        let echoed = map_chunks::<usize, usize, (), _>(&items, |batch| Ok(batch.to_vec())).unwrap();
        assert_eq!(echoed, items);
    }

    #[test]
    fn a_lease_never_reserves_more_than_the_ceiling() {
        let lease = lease(WorkUnits::new(usize::MAX));
        assert!(lease.lanes() <= threads().get());
    }

    #[test]
    fn a_saturated_lease_still_respects_the_ceiling() {
        let _boost = saturate();
        let lease = lease(WorkUnits::new(usize::MAX));
        assert!(lease.lanes() <= threads().get());
    }
}
