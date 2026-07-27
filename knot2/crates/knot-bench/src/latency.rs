use std::time::Duration;

use knot_index::Index;
use knot_types::RepoDid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenLatency(Duration);

impl OpenLatency {
    pub fn micros(value: u64) -> Self {
        Self(Duration::from_micros(value))
    }
    pub fn zero() -> Self {
        Self(Duration::ZERO)
    }
    fn stall(self) {
        if !self.0.is_zero() {
            std::thread::sleep(self.0);
        }
    }
}

pub fn replay_boot(index: &Index, repos: &[RepoDid], per_open: OpenLatency) {
    index.refresh_members().expect("fold members");
    index.refresh_registry().expect("fold registry");
    repos.iter().for_each(|repo| {
        per_open.stall();
        index
            .refresh_collaborators(repo)
            .expect("fabric-boot repo must fold, otherwise bench measures nothing");
    });
}
