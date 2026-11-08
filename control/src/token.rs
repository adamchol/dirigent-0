//! Control as an issuer of credentials: JWTs signed with its Ed25519 key, verified by chlive
//! (and control itself) with the key's public half, which control publishes as a JWKS. Players
//! get theirs on their lease; a dirigent also gets one to act as its owner.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ed25519_dalek::pkcs8::{DecodePrivateKey, EncodePrivateKey};
use ed25519_dalek::SigningKey;
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Who control's tokens are for: chlive, and control's own API.
pub const CHLIVE: &str = "chlive";
pub const CONTROL: &str = "control";

/// A token's claims.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claims {
    pub iss: String,
    pub sub: String,
    pub aud: Vec<String>,
    pub iat: u64,
    pub exp: u64,
    /// Who acts as `sub`: a dirigent's player, its owner's delegate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub act: Option<Actor>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Actor {
    pub sub: String,
}

pub struct Signer {
    /// The tokens' `iss`.
    pub issuer: String,
    /// How long a token is good for.
    pub ttl: Duration,
    kid: String,
    /// The public key, base64url (the JWK's `x`).
    x: String,
    encoding: EncodingKey,
    decoding: DecodingKey,
}

impl Signer {
    /// From a PKCS#8 PEM file (`control keygen`).
    pub fn load(path: &Path, issuer: &str, ttl: Duration) -> anyhow::Result<Signer> {
        let pem = std::fs::read_to_string(path)
            .with_context(|| format!("reading the signing key {}", path.display()))?;
        let key = SigningKey::from_pkcs8_pem(&pem)
            .with_context(|| format!("{}: not an Ed25519 PKCS#8 PEM key", path.display()))?;
        Signer::new(&key, issuer, ttl)
    }

    fn new(key: &SigningKey, issuer: &str, ttl: Duration) -> anyhow::Result<Signer> {
        let x = URL_SAFE_NO_PAD.encode(key.verifying_key().as_bytes());
        Ok(Signer {
            issuer: issuer.into(),
            ttl,
            kid: thumbprint(&x),
            decoding: DecodingKey::from_ed_components(&x)?,
            x,
            encoding: EncodingKey::from_ed_der(key.to_pkcs8_der()?.as_bytes()),
        })
    }

    /// The key's id: its RFC 7638 thumbprint.
    pub fn kid(&self) -> &str {
        &self.kid
    }

    /// A token saying it's `sub`, for `aud`; `act`, with it acting for `sub`.
    pub fn sign(&self, sub: &str, aud: &[&str], act: Option<&str>) -> String {
        let iat = now();
        let claims = Claims {
            iss: self.issuer.clone(),
            sub: sub.into(),
            aud: aud.iter().map(|a| a.to_string()).collect(),
            iat,
            exp: iat + self.ttl.as_secs(),
            act: act.map(|a| Actor { sub: a.into() }),
        };
        let mut header = Header::new(Algorithm::EdDSA);
        header.kid = Some(self.kid.clone());
        jsonwebtoken::encode(&header, &claims, &self.encoding).expect("an Ed25519 key signs")
    }

    /// A token with exactly these claims (an OpenID provider's, in tests).
    #[cfg(test)]
    pub fn sign_claims(&self, claims: &serde_json::Value) -> String {
        let mut header = Header::new(Algorithm::EdDSA);
        header.kid = Some(self.kid.clone());
        jsonwebtoken::encode(&header, claims, &self.encoding).unwrap()
    }

    /// One of its own tokens, if it's for `aud` and good still.
    pub fn verify(&self, token: &str, aud: &str) -> jsonwebtoken::errors::Result<Claims> {
        let mut validation = Validation::new(Algorithm::EdDSA);
        validation.set_issuer(&[&self.issuer]);
        validation.set_audience(&[aud]);
        validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
        validation.leeway = 5;
        Ok(jsonwebtoken::decode::<Claims>(token, &self.decoding, &validation)?.claims)
    }

    /// Its public key, as a JWKS.
    pub fn jwks(&self) -> serde_json::Value {
        serde_json::json!({ "keys": [{
            "kty": "OKP",
            "crv": "Ed25519",
            "x": self.x,
            "kid": self.kid,
            "alg": "EdDSA",
            "use": "sig",
        }] })
    }
}

/// An Ed25519 public key's RFC 7638 thumbprint (its key id).
fn thumbprint(x: &str) -> String {
    let canonical = format!(r#"{{"crv":"Ed25519","kty":"OKP","x":"{x}"}}"#);
    URL_SAFE_NO_PAD.encode(Sha256::digest(canonical.as_bytes()))
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("after 1970")
        .as_secs()
}

/// A new key, unless `dir` has one: `key.pem` (private) and `jwks.json` (its public half, for
/// chlive's `jwks_file`); and `link.key`, which ChatGPT links are encrypted with.
pub fn keygen(dir: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    crate::seal::keygen(&dir.join("link.key"))?;
    let pem = dir.join("key.pem");
    if !pem.exists() {
        std::fs::create_dir_all(dir)?;
        let key = SigningKey::from_bytes(&rand::random());
        let text = key.to_pkcs8_pem(Default::default())?;
        write_private(&pem, text.as_bytes())?;
    }
    let signer = Signer::load(&pem, "", Duration::ZERO)?;
    std::fs::write(
        dir.join("jwks.json"),
        serde_json::to_string_pretty(&signer.jwks())?,
    )?;
    Ok(())
}

/// A new file only its owner may read.
pub fn write_private(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(bytes)?;
    Ok(())
}

#[cfg(test)]
pub fn test_signer(issuer: &str, ttl: Duration) -> Signer {
    Signer::new(&SigningKey::from_bytes(&rand::random()), issuer, ttl).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signer() -> Signer {
        test_signer("control-test", Duration::from_secs(600))
    }

    #[test]
    fn a_token_verifies_for_its_audience_only() {
        let s = signer();
        let t = s.sign("player:p", &[CHLIVE], None);
        let claims = s.verify(&t, CHLIVE).unwrap();
        assert_eq!(claims.sub, "player:p");
        assert_eq!(claims.iss, "control-test");
        assert_eq!(claims.act, None);
        assert!(s.verify(&t, CONTROL).is_err());

        let t = s.sign("user:a", &[CHLIVE, CONTROL], Some("player:d"));
        let claims = s.verify(&t, CONTROL).unwrap();
        assert_eq!(claims.sub, "user:a");
        assert_eq!(claims.act.unwrap().sub, "player:d");
    }

    #[test]
    fn another_keys_or_issuers_or_an_expired_token_doesnt() {
        let s = signer();
        let other = signer();
        assert!(s
            .verify(&other.sign("user:a", &[CHLIVE], None), CHLIVE)
            .is_err());
        let elsewhere = Signer::new(
            &SigningKey::from_pkcs8_der(s.encoding.as_bytes()).unwrap(),
            "someone-else",
            s.ttl,
        )
        .unwrap();
        assert!(s
            .verify(&elsewhere.sign("user:a", &[CHLIVE], None), CHLIVE)
            .is_err());
        // expired past the leeway
        let claims = Claims {
            iss: s.issuer.clone(),
            sub: "user:a".into(),
            aud: vec![CHLIVE.into()],
            iat: now() - 120,
            exp: now() - 60,
            act: None,
        };
        let t = jsonwebtoken::encode(&Header::new(Algorithm::EdDSA), &claims, &s.encoding);
        assert!(s.verify(&t.unwrap(), CHLIVE).is_err());
    }

    #[test]
    fn the_jwks_names_the_key_by_its_thumbprint_and_verifies_tokens() {
        let s = signer();
        let jwks: jsonwebtoken::jwk::JwkSet = serde_json::from_value(s.jwks()).unwrap();
        let jwk = &jwks.keys[0];
        let x = match &jwk.algorithm {
            jsonwebtoken::jwk::AlgorithmParameters::OctetKeyPair(p) => p.x.clone(),
            other => panic!("{other:?}"),
        };
        assert_eq!(jwk.common.key_id.as_deref(), Some(thumbprint(&x).as_str()));
        let t = s.sign("user:a", &[CHLIVE], None);
        let mut v = Validation::new(Algorithm::EdDSA);
        v.set_audience(&[CHLIVE]);
        let key = DecodingKey::from_jwk(jwk).unwrap();
        assert!(jsonwebtoken::decode::<Claims>(&t, &key, &v).is_ok());
    }

    #[test]
    fn keygen_keeps_a_key_it_made() {
        let dir = tempfile::tempdir().unwrap();
        keygen(dir.path()).unwrap();
        let first = std::fs::read(dir.path().join("key.pem")).unwrap();
        let jwks = std::fs::read(dir.path().join("jwks.json")).unwrap();
        keygen(dir.path()).unwrap();
        assert_eq!(std::fs::read(dir.path().join("key.pem")).unwrap(), first);
        assert_eq!(std::fs::read(dir.path().join("jwks.json")).unwrap(), jwks);
    }
}
