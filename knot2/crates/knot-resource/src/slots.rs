use std::sync::{Arc, OnceLock};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::cpu::threads;

const RESOLVE_SLOTS_PER_TRANSPORT: usize = 16;

const SHARING_TRANSPORTS: usize = 2;

const RESOLVE_SLOTS: usize = RESOLVE_SLOTS_PER_TRANSPORT * SHARING_TRANSPORTS;

static PROCESS: OnceLock<Slots> = OnceLock::new();

pub struct SlotPermit(#[allow(dead_code)] OwnedSemaphorePermit);

macro_rules! slot_kind {
    ($($name:ident),+ $(,)?) => {$(
        #[derive(Clone)]
        pub struct $name(Arc<Semaphore>);

        impl $name {
            pub fn new(permits: usize) -> Self {
                Self(Arc::new(Semaphore::new(permits.max(1))))
            }

            pub async fn acquire(&self) -> SlotPermit {
                SlotPermit(
                    Arc::clone(&self.0)
                        .acquire_owned()
                        .await
                        .expect("a slot budget closes only with the process that owns it"),
                )
            }

            pub fn available(&self) -> usize {
                self.0.available_permits()
            }
        }
    )+};
}

slot_kind!(ResolveSlots, ReceiveSlots, PackSlots);

impl ResolveSlots {
    pub fn try_acquire(&self) -> Option<SlotPermit> {
        Arc::clone(&self.0).try_acquire_owned().ok().map(SlotPermit)
    }
}

#[derive(Clone)]
pub struct Slots {
    pub resolve: ResolveSlots,
    pub receive: ReceiveSlots,
    pub pack: PackSlots,
}

impl Slots {
    pub fn for_machine() -> Self {
        PROCESS
            .get_or_init(|| Self {
                resolve: ResolveSlots::new(RESOLVE_SLOTS),
                receive: ReceiveSlots::new(threads().get()),
                pack: PackSlots::new(threads().get()),
            })
            .clone()
    }

    pub fn testing(permits: usize) -> Self {
        Self {
            resolve: ResolveSlots::new(permits),
            receive: ReceiveSlots::new(permits),
            pack: PackSlots::new(permits),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_clone_shares_one_pool_per_kind_of_work() {
        let slots = Slots::testing(1);
        let other = slots.clone();
        let held = slots.receive.acquire().await;
        assert_eq!(
            other.receive.available(),
            0,
            "a cloned budget mustn't grant a second permit for the one slot"
        );
        assert_eq!(
            other.pack.available(),
            1,
            "spending a receive slot mustn't spend the pack budget"
        );
        let _resolving = slots.resolve.acquire().await;
        assert!(
            other.resolve.try_acquire().is_none(),
            "a cosmetic lookup mustn't wait, \
             or a push queues on it while its receive and pack slots stay spent"
        );
        drop(held);
        assert_eq!(other.receive.available(), 1);
    }

    #[tokio::test]
    async fn every_caller_of_for_machine_shares_one_budget_that_no_test_budget_touches() {
        let ssh = Slots::for_machine();
        let http = Slots::for_machine();
        let isolated = Slots::testing(1);
        let before = http.receive.available();
        let _held = ssh.receive.acquire().await;
        assert_eq!(
            http.receive.available(),
            before - 1,
            "two transports asking the machine for a budget must get the same one, \
             or the process grants twice the concurrency it was configured for"
        );
        let resolving: Vec<SlotPermit> = (0..RESOLVE_SLOTS_PER_TRANSPORT)
            .filter_map(|_| ssh.resolve.try_acquire())
            .collect();
        assert_eq!(resolving.len(), RESOLVE_SLOTS_PER_TRANSPORT);
        assert!(
            http.resolve.try_acquire().is_some(),
            "collapsing a per-transport pool into a process-wide one mustn't give a deployment \
             running both transports less outbound resolution than either had on its own"
        );
        assert_eq!(
            isolated.receive.available(),
            1,
            "whatever the process budget is doing mustn't spend a test budget"
        );
    }
}
