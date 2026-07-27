mod blocklist;
mod collaborators;
mod grant;
mod import;
mod members;
mod registry;

pub use blocklist::{Blocklist, BlocklistChange, BlocklistCob, block_account, unblock_account};
pub use collaborators::{
    Collaborators, CollaboratorsChange, CollaboratorsCob, add_collaborator, remove_collaborator,
};
pub use grant::{Entry, Grant, GrantChange, Removal, Roster};
pub use import::{ImportError, verify_cob_ref};
pub use members::{Members, MembersChange, MembersCob, add_member, remove_member};
pub use registry::{
    Registration, Registry, RegistryChange, RegistryError, Rename, RepoRecord, RepoRef,
    RepoRegistryCob, deregister_repo, register_repo, rename_repo,
};

#[doc(hidden)]
pub mod fuzz {
    use knot_cob::ChangePayload;

    use crate::{BlocklistChange, CollaboratorsChange, MembersChange, RegistryChange};

    pub fn change_decode(data: &[u8]) {
        let _ = RegistryChange::decode(data);
        let _ = MembersChange::decode(data);
        let _ = BlocklistChange::decode(data);
        let _ = CollaboratorsChange::decode(data);
    }

    pub fn ref_parse(data: &[u8]) {
        let _ = knot_cob::parse_cob_ref(&String::from_utf8_lossy(data));
    }
}
