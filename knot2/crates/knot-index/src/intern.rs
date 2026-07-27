use std::sync::Arc;

use knot_types::{AccountDid, OwnerDid, RepoDid, RepoRkey};
use lasso::{Spur, ThreadedRodeo};

#[derive(Debug, Clone, Default)]
pub(crate) struct Interner(Arc<ThreadedRodeo>);

impl Interner {
    pub(crate) fn new() -> Self {
        Self(Arc::new(ThreadedRodeo::new()))
    }
}

macro_rules! interned {
    ($(
        $key:ident of $value:ty {
            $intern:ident, $get:ident, $resolve:ident, $label:literal
        }
    )+) => {$(
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub(crate) struct $key(Spur);

        impl Interner {
            pub(crate) fn $intern(&self, value: &$value) -> $key {
                $key(self.0.get_or_intern(value.as_str()))
            }

            pub(crate) fn $get(&self, value: &$value) -> Option<$key> {
                self.0.get(value.as_str()).map($key)
            }

            pub(crate) fn $resolve(&self, key: $key) -> $value {
                <$value>::new(self.0.resolve(&key.0))
                    .expect(concat!("interned ", $label, " is valid ", $label))
            }
        }
    )+};
}

interned! {
    AccountKey of AccountDid { intern_account, account, resolve_account, "account DID" }
    RepoKey of RepoDid { intern_repo, repo, resolve_repo, "repo DID" }
    OwnerKey of OwnerDid { intern_owner, owner, resolve_owner, "owner DID" }
    RkeyKey of RepoRkey { intern_rkey, rkey, resolve_rkey, "rkey" }
}
