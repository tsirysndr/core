use std::sync::Arc;

use knot_cob::{CobHome, CobStore};
use knot_git::Repo;
use knot_messages::{Catalog, ErrorKey};
use knot_types::{ActorId, RefName};

use crate::{ReceiveCommand, ReceiveGuard, RefDecision};

pub struct PushGuard {
    pub cob_authority: ActorId,
    pub home: CobHome,
    pub messages: Arc<Catalog>,
}

impl ReceiveGuard for PushGuard {
    fn authorize(&self, staged: &Repo, commands: &[ReceiveCommand]) -> Vec<RefDecision> {
        commands
            .iter()
            .map(|command| self.decide(staged, command))
            .collect()
    }
}

impl PushGuard {
    fn decide(&self, staged: &Repo, command: &ReceiveCommand) -> RefDecision {
        match command.name() {
            None => RefDecision::Reject(
                self.messages
                    .reject
                    .cob_verification
                    .line(|ErrorKey::Error| crate::receive::invalid_refname(command.refname())),
            ),
            Some(name) if knot_git::is_public_ref(name) => RefDecision::Allow,
            Some(name) if knot_git::is_reserved(name) => {
                if command.is_delete() {
                    RefDecision::Reject(self.messages.reject.cob_delete.text())
                } else {
                    self.verify_cob(staged, name)
                }
            }
            Some(_) => RefDecision::Reject(self.messages.reject.hidden_reserved.text()),
        }
    }

    fn verify_cob(&self, staged: &Repo, name: &RefName) -> RefDecision {
        let store = CobStore::new(staged);
        match knot_cobs::verify_cob_ref(&store, &self.home, name, &self.cob_authority) {
            Ok(_) => RefDecision::Allow,
            Err(error) => RefDecision::Reject(
                self.messages
                    .reject
                    .cob_verification
                    .line(|ErrorKey::Error| error.to_string()),
            ),
        }
    }
}
