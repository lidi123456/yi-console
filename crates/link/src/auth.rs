//! Relay-layer authentication protocol, shared by yi-agentd, the client and the relay.
//!
//! After the WebSocket upgrade:
//! ```text
//! relay → peer  {"type":"challenge","v":2,"nonce":"<32 bytes b64>"}
//! peer → relay  {"type":"auth","role":"agent|stream|client","device":"..","pub":"..","sig":"..",
//!              "stream"?: "..", "invite"?: "..", "ticket"?: ".."}
//! relay → peer  {"type":"ok"}; on failure it closes with a close code + reason
//! ```
//! See [`signed_payload`] for what is signed: it binds the role, device, stream and this nonce, so it can't be reused or replayed.

use crate::{b64, sha256, unb64, unb64_32};
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};

pub const ROLE_AGENT: &str = "agent";
pub const ROLE_STREAM: &str = "stream";
pub const ROLE_CLIENT: &str = "client";

/// Bad signature, malformed
pub const CLOSE_UNAUTHORIZED: u16 = 4401;
/// Identity is fine but not permitted (device not registered, client not on the allow list, invalid invite)
pub const CLOSE_FORBIDDEN: u16 = 4403;
pub const CLOSE_OFFLINE: u16 = 4004;
pub const CLOSE_NO_RESPONSE: u16 = 4008;
/// Too many connections
pub const CLOSE_BUSY: u16 = 4029;
/// Revoked
pub const CLOSE_REVOKED: u16 = 4410;

const CTX: &[u8] = b"yi-console relay auth v2";

pub fn challenge_msg(nonce: &[u8; 32]) -> String {
    json!({"type": "challenge", "v": 2, "nonce": b64(nonce)}).to_string()
}

/// Signed content: fields are concatenated with length prefixes so "ab"+"c" can't be confused with "a"+"bc"
pub fn signed_payload(role: &str, device: &str, stream: &str, nonce: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(160);
    for part in [CTX, role.as_bytes(), device.as_bytes(), stream.as_bytes(), nonce] {
        out.extend_from_slice(&(part.len() as u32).to_be_bytes());
        out.extend_from_slice(part);
    }
    out
}

pub fn verify(pub_key: &[u8; 32], sig: &[u8; 64], msg: &[u8]) -> bool {
    let Ok(vk) = ed25519_dalek::VerifyingKey::from_bytes(pub_key) else {
        return false;
    };
    vk.verify_strict(msg, &ed25519_dalek::Signature::from_bytes(sig)).is_ok()
}

/// Pairing ticket: the relay only knows this and can't derive the psk used by Noise
pub fn ticket_of(code: &[u8; 32]) -> [u8; 32] {
    sha256(&[b"yi-console pair ticket", code])
}

/// psk for the pairing handshake (IKpsk2)
pub fn psk_of(code: &[u8; 32]) -> [u8; 32] {
    let hk = hkdf::Hkdf::<sha2::Sha256>::new(Some(b"yi-console pair psk"), code);
    let mut out = [0u8; 32];
    hk.expand(b"psk", &mut out).expect("hkdf");
    out
}

/// The relay stores only this hash of an invite code
pub fn invite_hash(code: &str) -> [u8; 32] {
    sha256(&[b"yi-console invite", code.trim().as_bytes()])
}

/// Parsed auth message
#[derive(Debug, Clone)]
pub struct AuthRequest {
    pub role: String,
    pub device: String,
    pub pub_key: [u8; 32],
    pub sig: [u8; 64],
    pub stream: String,
    pub invite: String,
    pub ticket: Option<[u8; 32]>,
}

impl AuthRequest {
    pub fn parse(v: &Value) -> Result<AuthRequest> {
        if v["type"] != "auth" {
            bail!("第一条消息应为 auth");
        }
        let s = |k: &str| v[k].as_str().unwrap_or("").to_string();
        let sig: [u8; 64] = unb64(&s("sig"))?.try_into().map_err(|_| anyhow!("签名长度错误"))?;
        Ok(AuthRequest {
            role: s("role"),
            device: s("device"),
            pub_key: unb64_32(&s("pub"))?,
            sig,
            stream: s("stream"),
            invite: s("invite"),
            ticket: match v["ticket"].as_str() {
                Some(t) if !t.is_empty() => Some(unb64_32(t)?),
                _ => None,
            },
        })
    }

    pub fn verify(&self, nonce: &[u8; 32]) -> bool {
        verify(&self.pub_key, &self.sig, &signed_payload(&self.role, &self.device, &self.stream, nonce))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Identity;

    #[test]
    fn sign_and_verify() {
        let id = Identity::generate();
        let nonce = crate::random_bytes::<32>();
        let sig = id.sign_msg(&signed_payload(ROLE_CLIENT, "dev", "", &nonce));
        let v = json!({"type":"auth","role":ROLE_CLIENT,"device":"dev","pub":b64(&id.sign_pub()),"sig":b64(&sig)});
        let r = AuthRequest::parse(&v).unwrap();
        assert!(r.verify(&nonce));
        // A different nonce (replay) or a different role must both fail
        assert!(!r.verify(&crate::random_bytes::<32>()));
        let mut r2 = r.clone();
        r2.role = ROLE_AGENT.into();
        assert!(!r2.verify(&nonce));
    }

    #[test]
    fn payload_is_unambiguous() {
        assert_ne!(signed_payload("ab", "c", "", b"n"), signed_payload("a", "bc", "", b"n"));
    }

    #[test]
    fn ticket_and_psk_differ() {
        let code = crate::random_bytes::<32>();
        assert_ne!(ticket_of(&code), psk_of(&code));
    }
}
