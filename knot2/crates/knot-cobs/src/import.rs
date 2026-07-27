use knot_cob::{
    ActorId, ChangePayload, CobError, CobHome, CobId, CobStore, TypeName, parse_cob_ref,
};
use knot_types::RefName;

use crate::collaborators::{CollaboratorsChange, CollaboratorsCob};
use crate::members::{MembersChange, MembersCob};
use crate::registry::{RegistryChange, RepoRegistryCob};

#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error("'{0}' isn't refs/cobs/<nsid>/<oid> ref")]
    NotCobRef(String),
    #[error("no collaborative object type is registered for namespace '{0}'")]
    UnknownType(TypeName),
    #[error(transparent)]
    Cob(#[from] CobError),
}

type Verifier = fn(&CobStore<'_>, &CobHome, CobId, &ActorId) -> Result<(), CobError>;

fn verifier_for(type_name: &TypeName) -> Option<Verifier> {
    [
        (MembersChange::type_name(), {
            |store: &CobStore<'_>, home, object, owner| {
                store.verify::<MembersCob>(home, object, owner)
            }
        } as Verifier),
        (CollaboratorsChange::type_name(), {
            |store: &CobStore<'_>, home, object, owner| {
                store.verify::<CollaboratorsCob>(home, object, owner)
            }
        } as Verifier),
        (RegistryChange::type_name(), {
            |store: &CobStore<'_>, home, object, owner| {
                store.verify::<RepoRegistryCob>(home, object, owner)
            }
        } as Verifier),
    ]
    .into_iter()
    .find_map(|(name, verifier)| (name == *type_name).then_some(verifier))
}

pub fn verify_cob_ref(
    store: &CobStore,
    home: &CobHome,
    refname: &RefName,
    owner: &ActorId,
) -> Result<CobId, ImportError> {
    let (type_name, object) = parse_cob_ref(refname.as_str())
        .ok_or_else(|| ImportError::NotCobRef(refname.as_str().to_string()))?;
    let verifier = verifier_for(&type_name).ok_or(ImportError::UnknownType(type_name))?;
    verifier(store, home, object, owner)?;
    Ok(object)
}
