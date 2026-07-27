use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1024))]

    #[test]
    fn the_transfer_engine_never_panics(data in proptest::collection::vec(any::<u8>(), 0..4096)) {
        knot_lfs::fuzz::transfer(&data);
    }

    #[test]
    fn the_batch_json_parser_never_panics(data in proptest::collection::vec(any::<u8>(), 0..4096)) {
        knot_lfs::fuzz::batch(&data);
    }

    #[test]
    fn the_pointer_parser_never_panics(data in proptest::collection::vec(any::<u8>(), 0..4096)) {
        knot_lfs::fuzz::pointer(&data);
    }
}
