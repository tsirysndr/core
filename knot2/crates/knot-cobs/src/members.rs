use crate::grant::grant_set_cob;

grant_set_cob! {
    change = MembersChange,
    cob = MembersCob,
    state = Members,
    type_name = "sh.tangled.knot.member",
    add = add_member,
    remove = remove_member,
}

#[cfg(test)]
mod tests {
    use knot_cob::{ChangePayload, Evaluate};
    use knot_types::{AccountDid, ActorId, UnixSeconds};

    use super::*;
    use crate::grant::{Grant, Removal};

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

    fn fold(changes: Vec<MembersChange>) -> Members {
        let author = ActorId::from_secp256k1(&[0x02; 33]);
        changes
            .into_iter()
            .fold(MembersCob::initial(), |state, change| {
                MembersCob::apply(state, change, &author)
            })
    }

    #[test]
    fn members_fold_add_then_remove() {
        let state = fold(vec![
            MembersChange::Add(grant("nel", "nel", 1)),
            MembersChange::Add(grant("olaren", "nel", 2)),
            MembersChange::Remove(Removal {
                subject: did("nel"),
            }),
        ]);
        assert!(state.contains(&did("olaren")));
        assert!(!state.contains(&did("nel")));
        assert_eq!(state.len(), 1);
    }

    #[test]
    fn change_payload_roundtrips_through_dag_cbor() {
        assert_eq!(MembersChange::TYPE, "sh.tangled.knot.member");
        let change = MembersChange::Add(grant("nel", "olaren", 9));
        let bytes = change.encode().unwrap();
        assert_eq!(MembersChange::decode(&bytes).unwrap(), change);
    }
}
