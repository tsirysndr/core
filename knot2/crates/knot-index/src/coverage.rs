use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Coverage {
    Warming,
    Ready,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved<T> {
    Warming,
    Ready(T),
}

impl<T> Resolved<T> {
    pub fn is_warming(&self) -> bool {
        matches!(self, Resolved::Warming)
    }

    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Resolved<U> {
        match self {
            Resolved::Ready(value) => Resolved::Ready(f(value)),
            Resolved::Warming => Resolved::Warming,
        }
    }
}

const WARMING: u8 = 0;
const READY: u8 = 1;

#[derive(Debug)]
pub(crate) struct CoverageCell(AtomicU8);

impl CoverageCell {
    pub(crate) fn new(initial: Coverage) -> Self {
        Self(AtomicU8::new(encode(initial)))
    }

    pub(crate) fn get(&self) -> Coverage {
        decode(self.0.load(Ordering::Acquire))
    }

    pub(crate) fn set(&self, coverage: Coverage) {
        self.0.store(encode(coverage), Ordering::Release);
    }
}

fn encode(coverage: Coverage) -> u8 {
    match coverage {
        Coverage::Warming => WARMING,
        Coverage::Ready => READY,
    }
}

fn decode(raw: u8) -> Coverage {
    match raw {
        READY => Coverage::Ready,
        _ => Coverage::Warming,
    }
}
