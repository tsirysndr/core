use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1024))]

    #[test]
    fn tls_parsers_never_panic(data in proptest::collection::vec(any::<u8>(), 0..4096)) {
        knot_edge::fuzz::spki_of_certificate(&data);
        knot_edge::fuzz::spki_pin(&data);
    }
}
