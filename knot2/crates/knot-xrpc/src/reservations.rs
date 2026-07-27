use knot_cache::{Admitted, Expiring, GroupQuota, Quotas, Rejected, TotalQuota};
use knot_runtime::UnixMicros;
use knot_types::{AccountDid, RepoDid, UnixSeconds};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReserveDecision {
    Fresh,
    Renewed,
    HeldByOther,
    PerActorFull,
    GlobalFull,
}

knot_types::scalar_newtype! {
    pub struct ReservationTtl(i64);
    pub struct PerActorQuota(usize);
    pub struct GlobalQuota(usize);
}

pub struct Reservations {
    held: Expiring<RepoDid, AccountDid, AccountDid>,
    ttl: ReservationTtl,
}

fn micros(seconds: UnixSeconds) -> UnixMicros {
    UnixMicros::new((seconds.get().max(0) as u64).saturating_mul(1_000_000))
}

impl Reservations {
    pub fn new(ttl: ReservationTtl, per_actor: PerActorQuota, global: GlobalQuota) -> Self {
        Self {
            held: Expiring::new(Quotas {
                per_group: GroupQuota::new(per_actor.get()),
                total: TotalQuota::new(global.get()),
            }),
            ttl,
        }
    }

    pub(crate) fn prune(&self, now: UnixSeconds) -> Vec<RepoDid> {
        self.held.prune(micros(now))
    }

    pub(crate) fn try_reserve(
        &self,
        repo: &RepoDid,
        actor: &AccountDid,
        now: UnixSeconds,
    ) -> ReserveDecision {
        let expires_at = micros(now.saturating_add_secs(self.ttl.get()));
        match self.held.admit_or_renew(
            repo.clone(),
            actor.clone(),
            actor.clone(),
            expires_at,
            micros(now),
        ) {
            Ok(Admitted::Inserted) => ReserveDecision::Fresh,
            Ok(Admitted::Occupied(holder)) if &holder == actor => ReserveDecision::Renewed,
            Ok(Admitted::Occupied(_)) => ReserveDecision::HeldByOther,
            Err(Rejected::Group) => ReserveDecision::PerActorFull,
            Err(Rejected::Total) => ReserveDecision::GlobalFull,
        }
    }

    pub(crate) fn holder_is(&self, repo: &RepoDid, actor: &AccountDid, now: UnixSeconds) -> bool {
        self.held
            .get(repo, micros(now))
            .is_some_and(|holder| &holder == actor)
    }

    pub(crate) fn release(&self, repo: &RepoDid) {
        self.held.remove(repo);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn actor(suffix: &str) -> AccountDid {
        AccountDid::new(format!("did:web:{suffix}")).unwrap()
    }

    fn repo(suffix: &str) -> RepoDid {
        RepoDid::new(format!("did:web:{suffix}.olaren.dev")).unwrap()
    }

    fn at(seconds: i64) -> UnixSeconds {
        UnixSeconds::new(seconds)
    }

    #[test]
    fn a_reservation_binds_to_its_actor_and_blocks_a_stranger() {
        let reservations = Reservations::new(
            ReservationTtl::new(3_600),
            PerActorQuota::new(8),
            GlobalQuota::new(64),
        );
        assert_eq!(
            reservations.try_reserve(&repo("squid"), &actor("nel.pet"), at(0)),
            ReserveDecision::Fresh
        );
        assert_eq!(
            reservations.try_reserve(&repo("squid"), &actor("olaren.dev"), at(1)),
            ReserveDecision::HeldByOther,
            "different account cannot take a live reservation"
        );
        assert!(reservations.holder_is(&repo("squid"), &actor("nel.pet"), at(1)));
        assert!(!reservations.holder_is(&repo("squid"), &actor("olaren.dev"), at(1)));
    }

    #[test]
    fn re_reserving_by_the_same_actor_renews_the_lease() {
        let reservations = Reservations::new(
            ReservationTtl::new(100),
            PerActorQuota::new(8),
            GlobalQuota::new(64),
        );
        assert_eq!(
            reservations.try_reserve(&repo("squid"), &actor("nel.pet"), at(0)),
            ReserveDecision::Fresh
        );
        assert_eq!(
            reservations.try_reserve(&repo("squid"), &actor("nel.pet"), at(50)),
            ReserveDecision::Renewed
        );
        assert!(
            reservations.holder_is(&repo("squid"), &actor("nel.pet"), at(140)),
            "renewal pushed the expiry out from the second call instead of the first"
        );
    }

    #[test]
    fn the_per_actor_limit_bounds_one_account_without_touching_another() {
        let reservations = Reservations::new(
            ReservationTtl::new(3_600),
            PerActorQuota::new(2),
            GlobalQuota::new(64),
        );
        assert_eq!(
            reservations.try_reserve(&repo("a"), &actor("nel.pet"), at(0)),
            ReserveDecision::Fresh
        );
        assert_eq!(
            reservations.try_reserve(&repo("b"), &actor("nel.pet"), at(0)),
            ReserveDecision::Fresh
        );
        assert_eq!(
            reservations.try_reserve(&repo("c"), &actor("nel.pet"), at(0)),
            ReserveDecision::PerActorFull,
            "one account is held to its per-actor budget"
        );
        assert_eq!(
            reservations.try_reserve(&repo("c"), &actor("olaren.dev"), at(0)),
            ReserveDecision::Fresh,
            "different account keeps its own budget"
        );
    }

    #[test]
    fn the_global_limit_bounds_the_total_across_accounts() {
        let reservations = Reservations::new(
            ReservationTtl::new(3_600),
            PerActorQuota::new(64),
            GlobalQuota::new(2),
        );
        assert_eq!(
            reservations.try_reserve(&repo("a"), &actor("nel.pet"), at(0)),
            ReserveDecision::Fresh
        );
        assert_eq!(
            reservations.try_reserve(&repo("b"), &actor("olaren.dev"), at(0)),
            ReserveDecision::Fresh
        );
        assert_eq!(
            reservations.try_reserve(&repo("c"), &actor("teq.dev"), at(0)),
            ReserveDecision::GlobalFull
        );
    }

    #[test]
    fn an_expired_reservation_is_pruned_and_frees_its_slot() {
        let reservations = Reservations::new(
            ReservationTtl::new(100),
            PerActorQuota::new(8),
            GlobalQuota::new(64),
        );
        reservations.try_reserve(&repo("squid"), &actor("nel.pet"), at(0));
        assert!(reservations.prune(at(50)).is_empty(), "not yet expired");
        let pruned = reservations.prune(at(150));
        assert_eq!(pruned, vec![repo("squid")], "expired lease is reaped");
        assert_eq!(
            reservations.try_reserve(&repo("squid"), &actor("olaren.dev"), at(160)),
            ReserveDecision::Fresh,
            "once the lease lapses a different account may claim DID"
        );
    }
}
