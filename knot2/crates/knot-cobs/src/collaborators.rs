use crate::grant::grant_set_cob;

grant_set_cob! {
    change = CollaboratorsChange,
    cob = CollaboratorsCob,
    state = Collaborators,
    type_name = "sh.tangled.repo.collaborator",
    add = add_collaborator,
    remove = remove_collaborator,
}

#[cfg(test)]
mod tests {
    use knot_cob::ChangePayload;

    #[test]
    fn type_name_is_stable() {
        assert_eq!(
            super::CollaboratorsChange::TYPE,
            "sh.tangled.repo.collaborator"
        );
    }
}
