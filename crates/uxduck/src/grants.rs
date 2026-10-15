//! The cloud's signed grants for control from away (S80, ADR-0003 C9): the
//! relay names the person behind each command with a grant the API signed,
//! and this computer checks it itself, against the API's published keys,
//! kept on disk so a cloud outage does not stop it. A relay bug cannot
//! forge a person; only the API's signing key could.
//!
//! A grant is `<payload>.<signature>`, both base64url: the payload is JSON
//! `{kid, sub, name, dev, exp}`, and the signature is Ed25519 over the
//! payload's base64url text.

use std::path::{Path, PathBuf};

use aws_lc_rs::signature::{ED25519, UnparsedPublicKey};
use serde::{Deserialize, Serialize};

/// Who a grant names, for which computer, until when.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Grant {
    pub kid: String,
    /// The person's account id.
    pub sub: String,
    pub name: String,
    /// The computer it is for: its device id.
    pub dev: String,
    /// Milliseconds since the Unix epoch.
    pub exp: u64,
}

/// One of the API's public keys, as `GET /v1/computers/keys` lists them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Key {
    pub kid: String,
    /// The Ed25519 public key, base64url.
    pub x: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Keys {
    pub keys: Vec<Key>,
}

fn path(data_dir: &Path) -> PathBuf {
    data_dir.join("relay-keys.json")
}

impl Keys {
    /// The keys kept from the last fetch.
    #[must_use]
    pub fn load(data_dir: &Path) -> Self {
        std::fs::read_to_string(path(data_dir))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, data_dir: &Path) {
        if let Ok(text) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(path(data_dir), text);
        }
    }

    #[must_use]
    pub fn has(&self, kid: &str) -> bool {
        self.keys.iter().any(|k| k.kid == kid)
    }
}

pub(crate) fn base64url(text: &str) -> Option<Vec<u8>> {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let (mut buffer, mut bits) = (0u32, 0u32);
    for byte in text.trim_end_matches('=').bytes() {
        let value = ALPHABET.iter().position(|&a| a == byte)?;
        buffer = (buffer << 6) | u32::try_from(value).ok()?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(u8::try_from((buffer >> bits) & 0xff).ok()?);
        }
    }
    Some(out)
}

/// The key id a grant was signed with, to fetch keys when it is unknown.
#[must_use]
pub fn kid_of(token: &str) -> Option<String> {
    let payload = base64url(token.split('.').next()?)?;
    serde_json::from_slice::<Grant>(&payload)
        .ok()
        .map(|g| g.kid)
}

/// Checks a grant: signed by one of `keys`, for this computer, not expired.
///
/// # Errors
/// Why it does not hold, for the relay to pass on.
pub fn verify(token: &str, keys: &Keys, device_id: &str, now_ms: u64) -> Result<Grant, String> {
    let (payload_text, signature_text) = token.split_once('.').ok_or("the grant is not signed")?;
    let payload = base64url(payload_text).ok_or("the grant is not readable")?;
    let signature = base64url(signature_text).ok_or("the grant's signature is not readable")?;
    let grant: Grant =
        serde_json::from_slice(&payload).map_err(|_| "the grant is not readable".to_owned())?;
    let key = keys
        .keys
        .iter()
        .find(|k| k.kid == grant.kid)
        .and_then(|k| base64url(&k.x))
        .ok_or_else(|| format!("no key {} to check the grant with", grant.kid))?;
    UnparsedPublicKey::new(&ED25519, key)
        .verify(payload_text.as_bytes(), &signature)
        .map_err(|_| "the grant's signature does not check out".to_owned())?;
    if grant.dev != device_id {
        return Err("the grant is for another computer".into());
    }
    if grant.exp <= now_ms {
        return Err("the grant has expired".into());
    }
    Ok(grant)
}

#[cfg(test)]
mod tests {
    use aws_lc_rs::signature::{Ed25519KeyPair, KeyPair};

    use super::*;

    fn encode(bytes: &[u8]) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let n = chunk
                .iter()
                .enumerate()
                .fold(0u32, |n, (i, b)| n | (u32::from(*b) << (16 - 8 * i)));
            for i in 0..=chunk.len() {
                out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
            }
        }
        out
    }

    fn signed(pair: &Ed25519KeyPair, payload: &serde_json::Value) -> String {
        let text = encode(payload.to_string().as_bytes());
        let signature = pair.sign(text.as_bytes());
        format!("{text}.{}", encode(signature.as_ref()))
    }

    #[test]
    fn a_grant_holds_only_signed_for_this_computer_and_in_time() {
        let rng = aws_lc_rs::rand::SystemRandom::new();
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let keys = Keys {
            keys: vec![Key {
                kid: "k1".into(),
                x: encode(pair.public_key().as_ref()),
            }],
        };
        let claims = |dev: &str, exp: u64| serde_json::json!({ "kid": "k1", "sub": "u-ana", "name": "ana@example.com", "dev": dev, "exp": exp });
        let good = signed(&pair, &claims("dv0000000000000001", 2_000));
        let grant = verify(&good, &keys, "dv0000000000000001", 1_000).unwrap();
        assert_eq!(
            (grant.sub.as_str(), grant.name.as_str()),
            ("u-ana", "ana@example.com")
        );
        assert_eq!(kid_of(&good).as_deref(), Some("k1"));

        assert!(
            verify(&good, &keys, "dv0000000000000002", 1_000)
                .unwrap_err()
                .contains("another computer")
        );
        assert!(
            verify(&good, &keys, "dv0000000000000001", 2_000)
                .unwrap_err()
                .contains("expired")
        );
        // Changed after signing: refused.
        let (_, signature) = good.split_once('.').unwrap();
        let forged = format!(
            "{}.{signature}",
            encode(claims("dv0000000000000001", 9_999).to_string().as_bytes())
        );
        assert!(
            verify(&forged, &keys, "dv0000000000000001", 1_000)
                .unwrap_err()
                .contains("signature")
        );
        // Signed by another key.
        let other =
            Ed25519KeyPair::from_pkcs8(Ed25519KeyPair::generate_pkcs8(&rng).unwrap().as_ref())
                .unwrap();
        let theirs = signed(&other, &claims("dv0000000000000001", 2_000));
        assert!(verify(&theirs, &keys, "dv0000000000000001", 1_000).is_err());
        assert!(verify("nonsense", &keys, "dv0000000000000001", 1_000).is_err());
    }
}
