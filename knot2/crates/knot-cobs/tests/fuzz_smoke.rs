use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1024))]

    #[test]
    fn the_cob_change_decoder_never_panics(data in proptest::collection::vec(any::<u8>(), 0..4096)) {
        knot_cobs::fuzz::change_decode(&data);
    }

    #[test]
    fn the_cob_ref_parser_never_panics(data in proptest::collection::vec(any::<u8>(), 0..4096)) {
        knot_cobs::fuzz::ref_parse(&data);
    }
}
