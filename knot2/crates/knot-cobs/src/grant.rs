use std::collections::BTreeMap;

use knot_types::{AccountDid, UnixSeconds};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    pub subject: AccountDid,
    pub added_by: AccountDid,
    pub created_at: UnixSeconds,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Removal {
    pub subject: AccountDid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub added_by: AccountDid,
    pub created_at: UnixSeconds,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Roster {
    entries: BTreeMap<AccountDid, Entry>,
}

impl Roster {
    pub fn empty() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }

    pub fn admit(mut self, grant: Grant) -> Self {
        self.entries.entry(grant.subject).or_insert(Entry {
            added_by: grant.added_by,
            created_at: grant.created_at,
        });
        self
    }

    pub fn revoke(mut self, removal: Removal) -> Self {
        self.entries.remove(&removal.subject);
        self
    }

    pub fn get(&self, subject: &AccountDid) -> Option<&Entry> {
        self.entries.get(subject)
    }

    pub fn contains(&self, subject: &AccountDid) -> bool {
        self.entries.contains_key(subject)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn entries(&self) -> impl Iterator<Item = (&AccountDid, &Entry)> {
        self.entries.iter()
    }
}

pub trait GrantChange {
    fn subject(&self) -> &AccountDid;
    fn adds(&self) -> bool;
    fn as_grant(&self) -> Option<&Grant>;
}

macro_rules! grant_set_cob {
    (
        change = $change:ident,
        cob = $cob:ident,
        state = $state:ident,
        type_name = $type_name:literal,
        add = $add:ident,
        remove = $remove:ident $(,)?
    ) => {
        #[derive(Debug, Clone, PartialEq, Eq, ::serde::Serialize, ::serde::Deserialize)]
        #[serde(tag = "op", content = "data", rename_all = "snake_case")]
        pub enum $change {
            Add($crate::grant::Grant),
            Remove($crate::grant::Removal),
        }

        impl ::knot_cob::ChangePayload for $change {
            const TYPE: &'static str = $type_name;
        }

        impl $crate::grant::GrantChange for $change {
            fn subject(&self) -> &::knot_types::AccountDid {
                match self {
                    $change::Add(grant) => &grant.subject,
                    $change::Remove(removal) => &removal.subject,
                }
            }

            fn adds(&self) -> bool {
                ::core::matches!(self, $change::Add(_))
            }

            fn as_grant(&self) -> ::core::option::Option<&$crate::grant::Grant> {
                match self {
                    $change::Add(grant) => ::core::option::Option::Some(grant),
                    $change::Remove(_) => ::core::option::Option::None,
                }
            }
        }

        pub type $state = $crate::grant::Roster;

        pub struct $cob;

        impl ::knot_cob::Evaluate for $cob {
            type State = $state;
            type Change = $change;

            const HISTORY: ::knot_cob::HistoryModel = ::knot_cob::HistoryModel::Linear;

            fn initial() -> Self::State {
                $crate::grant::Roster::empty()
            }

            fn apply(
                state: Self::State,
                change: Self::Change,
                _author: &::knot_types::ActorId,
            ) -> Self::State {
                match change {
                    $change::Add(grant) => state.admit(grant),
                    $change::Remove(removal) => state.revoke(removal),
                }
            }
        }

        impl ::knot_cob::Checkpoint for $cob {
            const SNAPSHOT_STRIDE: ::knot_cob::SnapshotStride =
                ::knot_cob::SnapshotStride::new(256);
            fn checkpoint_size(state: &Self::State) -> ::knot_cob::StateSize {
                ::knot_cob::StateSize::new(state.len())
            }
        }

        pub fn $add(
            store: &::knot_cob::CobStore,
            home: &::knot_cob::CobHome,
            object: ::knot_cob::CobId,
            grant: $crate::grant::Grant,
            signer: &dyn ::knot_runtime::Signer,
            timestamp: ::knot_types::UnixSeconds,
        ) -> ::core::result::Result<::knot_cob::ChangeId, ::knot_cob::CobError> {
            store.update_with_checkpointed::<$cob, ::knot_cob::CobError>(
                home,
                object,
                signer,
                timestamp,
                |_state| ::core::result::Result::Ok($change::Add(grant.clone())),
            )
        }

        pub fn $remove(
            store: &::knot_cob::CobStore,
            home: &::knot_cob::CobHome,
            object: ::knot_cob::CobId,
            removal: $crate::grant::Removal,
            signer: &dyn ::knot_runtime::Signer,
            timestamp: ::knot_types::UnixSeconds,
        ) -> ::core::result::Result<::knot_cob::ChangeId, ::knot_cob::CobError> {
            store.update_with_checkpointed::<$cob, ::knot_cob::CobError>(
                home,
                object,
                signer,
                timestamp,
                |_state| ::core::result::Result::Ok($change::Remove(removal.clone())),
            )
        }
    };
}

pub(crate) use grant_set_cob;

#[cfg(test)]
mod tests {
    use super::*;

    fn did(suffix: &str) -> AccountDid {
        AccountDid::new(format!("did:plc:{suffix}")).unwrap()
    }

    fn grant(subject: &str, added_by: &str, at: i64) -> Grant {
        Grant {
            subject: did(subject),
            added_by: did(added_by),
            created_at: UnixSeconds::new(at),
        }
    }

    #[test]
    fn admit_keeps_the_first_provenance() {
        let roster = Roster::empty()
            .admit(grant("nel", "olaren", 1))
            .admit(grant("nel", "teq", 5));
        let entry = roster.get(&did("nel")).unwrap();
        assert_eq!(entry.added_by, did("olaren"));
        assert_eq!(entry.created_at, UnixSeconds::new(1));
        assert_eq!(roster.len(), 1);
    }
}
