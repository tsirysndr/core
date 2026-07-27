use knot_cob::{ChangePayload, Checkpoint, CobError, CobHome, CobStore, Evaluate};
use knot_cobs::{GrantChange, Roster};
use knot_runtime::Signer;
use knot_types::UnixSeconds;

use crate::error::XrpcError;

pub(crate) fn grant_set_apply<E>(
    store: &CobStore,
    home: &CobHome,
    change: E::Change,
    signer: &dyn Signer,
    now: UnixSeconds,
    create_if_absent: bool,
) -> Result<bool, XrpcError>
where
    E: Checkpoint + Evaluate<State = Roster>,
    E::Change: ChangePayload + Clone + GrantChange,
{
    match store.list::<E>().map_err(XrpcError::from)?.as_slice() {
        [] if create_if_absent => {
            store
                .create(home, &change, signer, now)
                .map_err(XrpcError::from)?;
            Ok(true)
        }
        [] => Ok(false),
        [object] => store
            .update_maybe_checkpointed::<E, CobError>(home, *object, signer, now, |roster| {
                let redundant = change.adds() == roster.contains(change.subject());
                Ok(if redundant {
                    None
                } else {
                    Some(change.clone())
                })
            })
            .map(|change_id| change_id.is_some())
            .map_err(XrpcError::from),
        many => Err(XrpcError::internal(format!(
            "{} collaborative objects of one type share namespace",
            many.len()
        ))),
    }
}
