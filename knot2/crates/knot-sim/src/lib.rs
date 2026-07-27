mod harness;
mod realdata;
pub mod trace;
mod workload;

pub use harness::OpenError;
pub use realdata::run as run_realdata;
pub use trace::{
    OperationIndex, Outcome, Projection, RepoCollaborators, RoundNumber, Snapshot, Step, Trace,
};

use std::sync::Arc;

use harness::Harness;

pub async fn run(seed: u64, rounds: u32) -> Trace {
    let stranger_pool = (rounds as usize) * 2 + 16;
    let harness = Arc::new(Harness::build(seed, stranger_pool));
    let plan = workload::plan(seed, rounds, harness.subjects.len());
    workload::execute(harness, seed, plan).await
}

pub fn predict(seed: u64, rounds: u32) -> Projection {
    workload::predict(seed, rounds)
}
