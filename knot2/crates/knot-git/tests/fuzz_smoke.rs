use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1024))]

    #[test]
    fn the_patch_parsers_never_panic(data in proptest::collection::vec(any::<u8>(), 0..4096)) {
        knot_git::fuzz::patch(&data);
    }
}
