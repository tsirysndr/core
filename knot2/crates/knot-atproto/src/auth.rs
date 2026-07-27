use http::HeaderValue;
use knot_runtime::{Entropy, HttpRequest, Signer};
use knot_types::{KnotId, Nsid, ServiceDid, UnixSeconds};

use crate::AtprotoError;
use crate::jwt;
use crate::jwt::JwtNonce;

const POINTER_NONCE_BYTES: usize = 16;

pub struct PointerAuth<'a> {
    pub issuer: &'a KnotId,
    pub audience: &'a ServiceDid,
    pub lxm: &'a Nsid,
    pub now_unix: UnixSeconds,
}

pub trait PointerAuthorizer {
    fn authorize(
        &self,
        request: &mut HttpRequest,
        ctx: &PointerAuth<'_>,
    ) -> Result<(), AtprotoError>;
}

pub struct ServiceAuth<'a> {
    signer: &'a dyn Signer,
    entropy: &'a dyn Entropy,
}

impl<'a> ServiceAuth<'a> {
    pub fn new(signer: &'a dyn Signer, entropy: &'a dyn Entropy) -> Self {
        Self { signer, entropy }
    }
}

impl PointerAuthorizer for ServiceAuth<'_> {
    fn authorize(
        &self,
        request: &mut HttpRequest,
        ctx: &PointerAuth<'_>,
    ) -> Result<(), AtprotoError> {
        let mut bytes = [0u8; POINTER_NONCE_BYTES];
        self.entropy.fill(&mut bytes);
        let nonce = JwtNonce::new(knot_types::lowercase_hex(&bytes))?;
        let token = jwt::mint(
            self.signer,
            ctx.issuer,
            ctx.audience,
            ctx.lxm,
            nonce,
            ctx.now_unix,
        );
        let header = HeaderValue::from_str(&format!("Bearer {token}"))
            .expect("base64url jwt is valid header value");
        request.headers.insert(http::header::AUTHORIZATION, header);
        Ok(())
    }
}

pub struct OauthAuthorizer;

impl PointerAuthorizer for OauthAuthorizer {
    fn authorize(
        &self,
        _request: &mut HttpRequest,
        _ctx: &PointerAuth<'_>,
    ) -> Result<(), AtprotoError> {
        todo!("OAuth+DPoP authorizer is waiting on an OAuth client")
        // one day...
    }
}
