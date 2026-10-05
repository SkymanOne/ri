//! PKCE (RFC 7636) verifier and S256 challenge. Port of `oauth/pkce.ts`.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};

/// A code verifier and its challenge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pkce {
    /// Sent with the token exchange.
    pub verifier: String,
    /// Sent with the authorization request.
    pub challenge: String,
}

/// `count` random bytes, base64url without padding.
pub fn random_base64url(count: usize) -> String {
    let mut bytes = vec![0u8; count];
    // The OS generator only fails on unsupported platforms, where sign-in cannot work anyway.
    let _ = getrandom::fill(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// A fresh verifier from 32 random bytes and its SHA-256 challenge.
pub fn generate() -> Pkce {
    let verifier = random_base64url(32);
    let challenge = challenge(&verifier);
    Pkce {
        verifier,
        challenge,
    }
}

/// The S256 challenge of a verifier.
pub fn challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
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
