use crate::grant::grant_set_cob;

grant_set_cob! {
    change = BlocklistChange,
    cob = BlocklistCob,
    state = Blocklist,
    type_name = "sh.tangled.knot.block",
    add = block_account,
    remove = unblock_account,
}

#[cfg(test)]
mod tests {
    use knot_cob::ChangePayload;

    #[test]
    fn type_name_is_stable() {
        assert_eq!(super::BlocklistChange::TYPE, "sh.tangled.knot.block");
    }
}
