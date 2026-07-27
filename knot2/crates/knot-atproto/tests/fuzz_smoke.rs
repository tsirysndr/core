use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1024))]

    #[test]
    fn the_pubkey_parsers_never_panic(data in proptest::collection::vec(any::<u8>(), 0..4096)) {
        knot_atproto::fuzz::pubkey(&data);
    }

    #[test]
    fn the_did_document_decoder_never_panics(data in proptest::collection::vec(any::<u8>(), 0..4096)) {
        knot_atproto::fuzz::did_document(&data);
    }
}
