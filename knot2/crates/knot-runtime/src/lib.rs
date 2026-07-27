mod clock;
mod dns;
mod entropy;
mod http;
mod signer;

pub use clock::{Clock, ManualClock, SystemClock, UnixMicros};
pub use dns::{DnsFuture, DnsTxtResolver, FakeDns, SystemDns};
pub use entropy::{Entropy, OsEntropy, SeededEntropy};
pub use http::{
    ByteStream, FakeHttp, HttpFuture, HttpLimits, HttpRequest, HttpResponse, HttpTransport,
    NetworkError, ReqwestHttp, StreamFuture, StreamedResponse, is_blocked_ip,
};
pub use signer::{
    K256Signer, MAX_SCALAR_ATTEMPTS, PublicKeyBytes, Signature, SignatureScheme, Signer,
    SignerError, verify,
};

#[cfg(test)]
mod contract {
    use super::*;

    fn clock_contract(clock: &dyn Clock) {
        let first = clock.now_unix_micros();
        let second = clock.now_unix_micros();
        assert!(second >= first);
    }

    #[test]
    fn every_clock_is_non_decreasing() {
        clock_contract(&SystemClock);
        clock_contract(&ManualClock::new(UnixMicros::new(1_000)));
    }

    fn entropy_contract(entropy: &dyn Entropy) {
        let mut buffer = [0u8; 16];
        entropy.fill(&mut buffer);
        assert!(buffer.iter().any(|byte| *byte != 0));
        entropy.fill(&mut []);
        let _ = entropy.next_u64();
    }

    #[test]
    fn every_entropy_produces_output() {
        entropy_contract(&OsEntropy);
        entropy_contract(&SeededEntropy::new(1));
    }

    fn signer_contract(signer: &dyn Signer) {
        let signature = signer.sign(b"the message");
        assert!(verify(&signer.public_key(), b"the message", &signature));
        assert!(!verify(
            &signer.public_key(),
            b"another message",
            &signature
        ));
    }

    #[test]
    fn the_seeded_signer_is_the_test_double() {
        signer_contract(&K256Signer::generate(&SeededEntropy::new(3)));
    }

    fn ok_response() -> HttpResponse {
        HttpResponse {
            status: ::http::StatusCode::OK,
            headers: ::http::HeaderMap::new(),
            body: bytes::Bytes::from_static(b"ok"),
        }
    }

    #[test]
    fn http_transport_surfaces_ok_and_typed_error() {
        let url = url::Url::parse("https://oyster.cafe/").unwrap();
        let okay = FakeHttp::new(|_| Ok(ok_response()));
        let response =
            futures::executor::block_on(okay.execute(HttpRequest::get(url.clone()))).unwrap();
        assert_eq!(response.body.as_ref(), b"ok");

        let failing = FakeHttp::new(|_| Err(NetworkError::Timeout("slow".to_string())));
        let error =
            futures::executor::block_on(failing.execute(HttpRequest::get(url))).unwrap_err();
        assert!(matches!(error, NetworkError::Timeout(_)));
    }
}
