//! PKCE (RFC 7636) verifier and S256 challenge. Port of `oauth/pkce.ts`.

use aws_lc_rs::digest::{SHA256, digest};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// A code verifier and its challenge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pkce {
    /// Sent with the token exchange.
    pub verifier: String,
    /// Sent with the authorization request.
    pub challenge: String,
}

/// 32 random bytes, base64url without padding.
pub fn random_base64url() -> String {
    URL_SAFE_NO_PAD.encode(yapi_types::time::random_bytes::<32>())
}

/// A fresh verifier from 32 random bytes and its SHA-256 challenge.
pub fn generate() -> Pkce {
    let verifier = random_base64url();
    let challenge = challenge(&verifier);
    Pkce {
        verifier,
        challenge,
    }
}

/// The S256 challenge of a verifier.
pub fn challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(digest(&SHA256, verifier.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_rfc_7636_example() {
        assert_eq!(
            challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        let pkce = generate();
        assert_eq!(pkce.verifier.len(), 43);
        assert_eq!(pkce.challenge, challenge(&pkce.verifier));
    }
}
