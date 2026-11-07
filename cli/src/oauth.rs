//! What signing in with OAuth needs on the CLI's side: PKCE's verifier and challenge, fresh
//! random values, and when an ID token expires (read, not verified: control verifies it).

use std::time::{SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use rand::RngCore;
use sha2::{Digest, Sha256};

/// 32 random bytes, base64url: a PKCE verifier, a `state`.
pub fn random() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// The verifier's S256 challenge.
pub fn challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// Whether the JWT expires within `margin` seconds (or can't be read).
pub fn expiring(jwt: &str, margin: u64) -> bool {
    let exp = jwt
        .split('.')
        .nth(1)
        .and_then(|p| URL_SAFE_NO_PAD.decode(p).ok())
        .and_then(|p| serde_json::from_slice::<serde_json::Value>(&p).ok())
        .and_then(|c| c.get("exp").and_then(|e| e.as_u64()));
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    exp.is_none_or(|exp| exp <= now + margin)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_challenge_is_rfc_7636s() {
        // RFC 7636, appendix B
        assert_eq!(
            challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        assert_ne!(random(), random());
    }

    #[test]
    fn a_token_is_expiring_near_its_exp_or_without_one() {
        let jwt = |claims: serde_json::Value| {
            format!("h.{}.s", URL_SAFE_NO_PAD.encode(claims.to_string()))
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!(!expiring(
            &jwt(serde_json::json!({ "exp": now + 3600 })),
            300
        ));
        assert!(expiring(&jwt(serde_json::json!({ "exp": now + 100 })), 300));
        assert!(expiring(&jwt(serde_json::json!({})), 300));
        assert!(expiring("garbage", 300));
    }
}
