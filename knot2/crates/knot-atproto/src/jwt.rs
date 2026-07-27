use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use knot_runtime::{SignatureScheme, Signer};
use knot_types::crypto::{KeyCodec, PublicKey as CryptoKey};
use knot_types::service_auth::{
    JwtHeader, ParsedJwt, PublicKey as VerifyKey, ServiceAuthClaims, ServiceAuthError, parse_jwt,
};
use knot_types::{AccountDid, CowStr, Did, DidService, KnotId, Nsid, ServiceDid, UnixSeconds};

pub(crate) const CLOCK_SKEW_SECS: i64 = 60;
pub(crate) const SERVICE_TOKEN_LIFETIME_SECS: i64 = 60;
const MAX_TOKEN_LIFETIME_SECS: i64 = 300;
const MAX_NONCE_BYTES: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct JwtNonce(String);

#[derive(Clone, PartialEq, Eq)]
pub struct ServiceJwt(String);

impl std::fmt::Debug for ServiceJwt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("ServiceJwt").finish_non_exhaustive()
    }
}

impl ServiceJwt {
    pub fn new(value: impl Into<String>) -> Result<Self, JwtError> {
        let value = value.into();
        let three_segments =
            value.split('.').count() == 3 && value.split('.').all(|segment| !segment.is_empty());
        match three_segments {
            true => Ok(Self(value)),
            false => Err(JwtError::NotAJwt),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ServiceJwt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl JwtNonce {
    pub fn new(value: impl Into<String>) -> Result<Self, JwtError> {
        let value = value.into();
        let len = value.len();
        (len <= MAX_NONCE_BYTES)
            .then_some(Self(value))
            .ok_or(JwtError::OversizedNonce { len })
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, thiserror::Error)]
pub enum JwtError {
    #[error("malformed token: {0}")]
    Parse(#[from] ServiceAuthError),
    #[error("token type {typ:?} isn't JWT")]
    UnexpectedType { typ: String },
    #[error("token isn't three dot-separated JWT segments")]
    NotAJwt,
    #[error("issuer {value:?} isn't valid account DID")]
    MalformedIssuer { value: String },
    #[error("token has no jti, refusing write without replay protection")]
    MissingNonce,
    #[error("{len}-byte jti exceeds {MAX_NONCE_BYTES}-byte nonce limit")]
    OversizedNonce { len: usize },
    #[error("issuer key codec isn't a signing algorithm")]
    UnsupportedKeyCodec,
    #[error("issuer key isn't valid verifying key: {0}")]
    MalformedKey(String),
    #[error("signature doesn't verify against issuer key")]
    InvalidSignature,
    #[error("audience mismatch: token addressed {actual}, expected {expected}")]
    AudienceMismatch { expected: String, actual: String },
    #[error("token expired at {exp}, now {now}")]
    Expired { exp: UnixSeconds, now: UnixSeconds },
    #[error("token issued in future: iat {iat}, now {now}")]
    IssuedInFuture { iat: UnixSeconds, now: UnixSeconds },
    #[error("token lifetime is too long: iat {iat}, exp {exp}, limit {max}s")]
    LifetimeTooLong {
        exp: UnixSeconds,
        iat: UnixSeconds,
        max: i64,
    },
    #[error("token expires at {exp}, before its issue at {iat}")]
    ExpiresBeforeIssued { exp: UnixSeconds, iat: UnixSeconds },
    #[error("method binding mismatch: token bound to {actual:?}, expected {expected}")]
    MethodMismatch {
        expected: String,
        actual: Option<String>,
    },
}

pub(crate) fn mint(
    signer: &dyn Signer,
    issuer: &KnotId,
    audience: &ServiceDid,
    method: &Nsid,
    nonce: JwtNonce,
    now_unix: UnixSeconds,
) -> ServiceJwt {
    let alg = match signer.scheme() {
        SignatureScheme::Secp256k1 => "ES256K",
        SignatureScheme::P256 => "ES256",
    };
    let header = JwtHeader {
        alg: CowStr::new_static(alg),
        typ: CowStr::new_static("JWT"),
    };
    let claims = ServiceAuthClaims {
        iss: Did::new_owned(issuer.as_str()).expect("knot DID parses as a DID"),
        aud: DidService::new_owned(audience.as_str()).expect("service DID parses as a DID"),
        exp: now_unix
            .saturating_add_secs(SERVICE_TOKEN_LIFETIME_SECS)
            .get(),
        iat: now_unix.get(),
        jti: Some(nonce.as_str().into()),
        lxm: Some(method.clone()),
    };
    let header_b64 =
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header).expect("jwt header serializes"));
    let payload_b64 =
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).expect("service-auth claims serialize"));
    let signing_input = format!("{header_b64}.{payload_b64}");
    let signature = signer.sign(signing_input.as_bytes());
    ServiceJwt(format!(
        "{signing_input}.{}",
        URL_SAFE_NO_PAD.encode(signature.as_bytes())
    ))
}

pub fn parse(token: &ServiceJwt) -> Result<ParsedJwt, JwtError> {
    let parsed = parse_jwt(token.as_str())?;
    let typ = parsed.header().typ.as_str();
    if !typ.eq_ignore_ascii_case("JWT") {
        return Err(JwtError::UnexpectedType {
            typ: typ.to_string(),
        });
    }
    Ok(parsed)
}

pub fn issuer(parsed: &ParsedJwt) -> Result<AccountDid, JwtError> {
    let iss = parsed.claims().iss.as_str();
    AccountDid::new(iss).map_err(|_| JwtError::MalformedIssuer {
        value: iss.to_string(),
    })
}

fn verifying_key(key: &CryptoKey<'_>) -> Result<VerifyKey, JwtError> {
    match key.codec {
        KeyCodec::Secp256k1 => VerifyKey::from_k256_bytes(&key.bytes)
            .map_err(|e| JwtError::MalformedKey(e.to_string())),
        KeyCodec::P256 => VerifyKey::from_p256_bytes(&key.bytes)
            .map_err(|e| JwtError::MalformedKey(e.to_string())),
        KeyCodec::Ed25519 | KeyCodec::Unknown(_) => Err(JwtError::UnsupportedKeyCodec),
    }
}

pub trait TokenAudience {
    fn as_str(&self) -> &str;
    fn canonicalizes(&self, claimed: &str) -> bool;
}

impl TokenAudience for KnotId {
    fn as_str(&self) -> &str {
        KnotId::as_str(self)
    }

    fn canonicalizes(&self, claimed: &str) -> bool {
        KnotId::new(claimed).is_ok_and(|aud| aud.as_str() == KnotId::as_str(self))
    }
}

impl TokenAudience for ServiceDid {
    fn as_str(&self) -> &str {
        ServiceDid::as_str(self)
    }

    fn canonicalizes(&self, claimed: &str) -> bool {
        ServiceDid::new(claimed).is_ok_and(|aud| aud.as_str() == ServiceDid::as_str(self))
    }
}

pub fn check_claims(
    parsed: &ParsedJwt,
    audience: &impl TokenAudience,
    method: &Nsid,
    now_unix: UnixSeconds,
) -> Result<(), JwtError> {
    let claims = parsed.claims();
    let exp = UnixSeconds::new(claims.exp);
    let iat = UnixSeconds::new(claims.iat);

    if !audience.canonicalizes(claims.aud.as_str()) {
        return Err(JwtError::AudienceMismatch {
            expected: audience.as_str().to_string(),
            actual: claims.aud.as_str().to_string(),
        });
    }

    if exp.saturating_add_secs(CLOCK_SKEW_SECS) < now_unix {
        return Err(JwtError::Expired { exp, now: now_unix });
    }

    if iat.saturating_sub_secs(CLOCK_SKEW_SECS) > now_unix {
        return Err(JwtError::IssuedInFuture { iat, now: now_unix });
    }

    if exp < iat {
        return Err(JwtError::ExpiresBeforeIssued { exp, iat });
    }

    if exp.get().saturating_sub(iat.get()) > MAX_TOKEN_LIFETIME_SECS {
        return Err(JwtError::LifetimeTooLong {
            exp,
            iat,
            max: MAX_TOKEN_LIFETIME_SECS,
        });
    }

    let bound = claims.lxm.as_ref().map(|lxm| lxm.as_str());
    if bound != Some(method.as_str()) {
        return Err(JwtError::MethodMismatch {
            expected: method.as_str().to_string(),
            actual: bound.map(str::to_string),
        });
    }

    Ok(())
}

pub(crate) fn nonce(parsed: &ParsedJwt) -> Result<JwtNonce, JwtError> {
    let jti: &str = parsed
        .claims()
        .jti
        .as_ref()
        .map(|jti| jti.as_ref())
        .ok_or(JwtError::MissingNonce)?;
    JwtNonce::new(jti)
}

pub fn verify_signature(parsed: &ParsedJwt, issuer_key: &CryptoKey<'_>) -> Result<(), JwtError> {
    let key = verifying_key(issuer_key)?;
    knot_types::service_auth::verify_signature(parsed, &key).map_err(|error| match error {
        ServiceAuthError::InvalidSignature => JwtError::InvalidSignature,
        other => JwtError::Parse(other),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::mint as mint_claims;
    use crate::test_support::*;
    use std::borrow::Cow;

    fn claims(iss: &str, aud: &str, exp: UnixSeconds, lxm: &str) -> serde_json::Value {
        serde_json::json!({
            "iss": iss,
            "aud": aud,
            "exp": exp.get(),
            "iat": exp.saturating_sub_secs(60).get(),
            "lxm": lxm,
        })
    }

    #[test]
    fn a_well_formed_token_authenticates_its_issuer() {
        let signing = signer(1);
        let public = k256_public(&signing);
        let token = mint_claims(
            &signing,
            &claims(SQUID, KNOT, UnixSeconds::new(1_000), METHOD),
        );
        let parsed = parse(&token).unwrap();
        check_claims(
            &parsed,
            &knot_did(KNOT),
            &member_method(),
            UnixSeconds::new(900),
        )
        .unwrap();
        verify_signature(&parsed, &public).unwrap();
        assert_eq!(issuer(&parsed).unwrap(), AccountDid::new(SQUID).unwrap());
    }

    struct ClaimsCase {
        name: &'static str,
        claims: fn() -> serde_json::Value,
        now: i64,
        expect: fn(&Result<(), JwtError>) -> bool,
    }

    const CLAIMS_CASES: &[ClaimsCase] = &[
        ClaimsCase {
            name: "expired past the skew window",
            claims: || serde_json::json!({ "iss": SQUID, "aud": KNOT, "exp": 1_000, "iat": 940, "lxm": METHOD }),
            now: 1_100,
            expect: |r| {
                matches!(r, Err(JwtError::Expired { exp, now })
                if *exp == UnixSeconds::new(1_000) && *now == UnixSeconds::new(1_100))
            },
        },
        ClaimsCase {
            name: "within the skew window past exp",
            claims: || serde_json::json!({ "iss": SQUID, "aud": KNOT, "exp": 1_000, "iat": 940, "lxm": METHOD }),
            now: 1_030,
            expect: |r| r.is_ok(),
        },
        ClaimsCase {
            name: "addressed to another knot",
            claims: || serde_json::json!({ "iss": SQUID, "aud": "did:web:oyster.cafe", "exp": 1_000, "iat": 940, "lxm": METHOD }),
            now: 900,
            expect: |r| matches!(r, Err(JwtError::AudienceMismatch { .. })),
        },
        ClaimsCase {
            name: "bound to another method",
            claims: || serde_json::json!({ "iss": SQUID, "aud": KNOT, "exp": 1_000, "iat": 940, "lxm": "sh.tangled.repo.delete" }),
            now: 900,
            expect: |r| matches!(r, Err(JwtError::MethodMismatch { .. })),
        },
        ClaimsCase {
            name: "has no method binding",
            claims: || serde_json::json!({ "iss": SQUID, "aud": KNOT, "exp": 1_000, "iat": 940 }),
            now: 900,
            expect: |r| matches!(r, Err(JwtError::MethodMismatch { actual: None, .. })),
        },
        ClaimsCase {
            name: "issued in the future",
            claims: || serde_json::json!({ "iss": SQUID, "aud": KNOT, "exp": 2_001, "iat": 2_000, "lxm": METHOD }),
            now: 900,
            expect: |r| {
                matches!(r, Err(JwtError::IssuedInFuture { iat, now })
                if *iat == UnixSeconds::new(2_000) && *now == UnixSeconds::new(900))
            },
        },
        ClaimsCase {
            name: "lifetime exceeds the limit",
            claims: || serde_json::json!({ "iss": SQUID, "aud": KNOT, "exp": 1_400, "iat": 1_000, "lxm": METHOD }),
            now: 1_000,
            expect: |r| {
                matches!(r, Err(JwtError::LifetimeTooLong { exp, iat, max })
                if *exp == UnixSeconds::new(1_400) && *iat == UnixSeconds::new(1_000) && *max == 300)
            },
        },
        ClaimsCase {
            name: "expires before it was issued",
            claims: || serde_json::json!({ "iss": SQUID, "aud": KNOT, "exp": 1_040, "iat": 1_050, "lxm": METHOD }),
            now: 1_000,
            expect: |r| {
                matches!(r, Err(JwtError::ExpiresBeforeIssued { exp, iat })
                if *exp == UnixSeconds::new(1_040) && *iat == UnixSeconds::new(1_050))
            },
        },
        ClaimsCase {
            name: "audience in a different case still matches",
            claims: || serde_json::json!({ "iss": SQUID, "aud": "did:web:NEL.PET", "exp": 1_000, "iat": 940, "lxm": METHOD }),
            now: 900,
            expect: |r| r.is_ok(),
        },
    ];

    #[test]
    fn check_claims_enforces_the_audience_window_and_method_binding() {
        let signing = signer(1);
        CLAIMS_CASES.iter().for_each(|case| {
            let token = mint_claims(&signing, &(case.claims)());
            let parsed = parse(&token).unwrap();
            let result = check_claims(
                &parsed,
                &knot_did(KNOT),
                &member_method(),
                UnixSeconds::new(case.now),
            );
            assert!(
                (case.expect)(&result),
                "case {:?} got {result:?}",
                case.name
            );
        });
    }

    #[test]
    fn nonce_extraction_enforces_presence_and_limit() {
        let signing = signer(1);
        let with = |jti: Option<String>| {
            let mut body = claims(SQUID, KNOT, UnixSeconds::new(1_000), METHOD);
            if let Some(jti) = jti {
                body["jti"] = serde_json::json!(jti);
            }
            parse(&mint_claims(&signing, &body)).unwrap()
        };

        assert!(matches!(nonce(&with(None)), Err(JwtError::MissingNonce)));
        assert_eq!(
            nonce(&with(Some("nonce-1".to_string()))).unwrap().as_str(),
            "nonce-1"
        );
        assert!(matches!(
            nonce(&with(Some("n".repeat(MAX_NONCE_BYTES + 1)))),
            Err(JwtError::OversizedNonce { len }) if len == MAX_NONCE_BYTES + 1
        ));
        assert!(nonce(&with(Some("n".repeat(MAX_NONCE_BYTES)))).is_ok());
    }

    #[test]
    fn a_minted_token_round_trips_through_the_verify_half_and_honors_its_lifetime() {
        let key = runtime_signer(8);
        let knot_issuer = knot_did(KNOT);
        let audience = ServiceDid::new("did:web:pds.oyster.cafe").unwrap();
        let bound = Nsid::new_owned("com.atproto.repo.putRecord").unwrap();
        let token = super::mint(
            &key,
            &knot_issuer,
            &audience,
            &bound,
            JwtNonce::new("nonce-minted").unwrap(),
            UnixSeconds::new(1_000),
        );
        let parsed = parse(&token).unwrap();

        assert_eq!(parsed.claims().iat, 1_000);
        assert_eq!(parsed.claims().exp, 1_000 + SERVICE_TOKEN_LIFETIME_SECS);
        check_claims(&parsed, &audience, &bound, UnixSeconds::new(1_005)).unwrap();
        assert_eq!(nonce(&parsed).unwrap().as_str(), "nonce-minted");
        assert_eq!(issuer(&parsed).unwrap(), AccountDid::new(KNOT).unwrap());

        let public = CryptoKey {
            codec: KeyCodec::Secp256k1,
            bytes: Cow::Owned(knot_runtime::Signer::public_key(&key).as_bytes().to_vec()),
        };
        verify_signature(&parsed, &public).unwrap();

        let stranger = k256_public(&signer(4));
        assert!(matches!(
            verify_signature(&parsed, &stranger).unwrap_err(),
            JwtError::InvalidSignature
        ));
    }

    #[test]
    fn a_jwt_nonce_preserves_its_string_and_compares_by_value() {
        let nonce = JwtNonce::new("nonce-value").unwrap();
        assert_eq!(nonce.as_str(), "nonce-value");
        assert_eq!(nonce, JwtNonce::new("nonce-value".to_string()).unwrap());
        assert_ne!(nonce, JwtNonce::new("other").unwrap());
    }

    #[test]
    fn an_ed25519_issuer_key_is_unsupported() {
        let signing = signer(1);
        let token = mint_claims(
            &signing,
            &claims(SQUID, KNOT, UnixSeconds::new(1_000), METHOD),
        );
        let parsed = parse(&token).unwrap();
        let ed = CryptoKey {
            codec: KeyCodec::Ed25519,
            bytes: Cow::Owned(vec![0u8; 32]),
        };
        let error = verify_signature(&parsed, &ed).unwrap_err();
        assert!(matches!(error, JwtError::UnsupportedKeyCodec));
    }
}
