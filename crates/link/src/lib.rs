//! Relay link v2; see `docs/relay-security-v2.md` for the design.
//!
//! Topology: yi-agentd connects out to the relay's `/v1/agent` (control connection), clients connect to `/v1/client`;
//! the relay tells yi-agentd to open a `/v1/stream`, then splices the two WebSockets together as is.
//!
//! Two layers of authentication:
//! - Relay layer ([`auth`]): after the WebSocket upgrade the relay sends a nonce, and the peer proves its identity with an Ed25519 signature.
//!   yi-agentd relies on the device public key registered on the relay (registered the first time with an invite code); clients rely on the allow list yi-agentd pushes to the relay,
//!   and on a one-time pairing ticket for the first pairing. URLs never carry any keys.
//! - End to end ([`e2e`]): Noise IK (IKpsk2 when pairing), mutual authentication, forward secrecy; the relay only forwards ciphertext.
//!
//! Transport: when the relay has no domain it uses a self-signed certificate; links carry the certificate's SHA-256 fingerprint (`fp`), and only that certificate is accepted.

use anyhow::{anyhow, bail, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine;
use sha2::{Digest, Sha256};
use std::collections::HashMap;

pub mod auth;
pub mod e2e;
pub mod proxy;
mod wire;

pub use e2e::{agent_accept, client_connect, client_pair, Hello};
pub use wire::{agent_control, next_control, send_control, ws_connect, AgentLink, Control, OpenReq, Ws};

// ---------------- Helpers ----------------

pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).expect("系统随机数不可用");
    b
}

pub fn random_id() -> String {
    hex(&random_bytes::<12>())
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn b64(b: &[u8]) -> String {
    B64.encode(b)
}

/// Decode 32 bytes from base64url (unpadded)
pub fn unb64_32(s: &str) -> Result<[u8; 32]> {
    let raw = B64.decode(s.trim().as_bytes()).context("base64 格式错误")?;
    raw.try_into().map_err(|_| anyhow!("长度应为 32 字节"))
}

pub fn unb64(s: &str) -> Result<Vec<u8>> {
    B64.decode(s.trim().as_bytes()).context("base64 格式错误")
}

pub fn sha256(parts: &[&[u8]]) -> [u8; 32] {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

pub fn sha256_hex(s: &str) -> String {
    hex(&sha256(&[s.as_bytes()]))
}

/// Constant-time comparison, for tokens and tickets
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    use subtle::ConstantTimeEq;
    a.len() == b.len() && bool::from(a.ct_eq(b))
}

pub fn pct(s: &str) -> String {
    let mut o = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            o.push(b as char);
        } else {
            o.push_str(&format!("%{b:02X}"));
        }
    }
    o
}

pub fn unpct(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        // Take the two characters after % byte-wise, not by slicing &str: a following multi-byte character would be split in the middle and panic
        if b[i] == b'%' {
            if let Some(v) = b.get(i + 1..i + 3).and_then(hex_byte) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(if b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

/// Two hex characters -> one byte
pub fn hex_byte(h: &[u8]) -> Option<u8> {
    let d = |c: u8| (c as char).to_digit(16);
    match h {
        [a, b] => Some((d(*a)? * 16 + d(*b)?) as u8),
        _ => None,
    }
}

/// Normalize fingerprints to lowercase hex without colons; errors on bad format
pub fn normalize_fp(s: &str) -> Result<String> {
    let s = s.trim().to_lowercase();
    let s = s.strip_prefix("sha256:").unwrap_or(&s).replace(':', "");
    if s.is_empty() {
        return Ok(s);
    }
    if s.len() != 64 || !s.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("证书指纹格式错误（应为 64 位十六进制）");
    }
    Ok(s)
}

/// SHA-256 fingerprint of a certificate (DER)
pub fn cert_fp(der: &[u8]) -> String {
    hex(&sha256(&[der]))
}

// ---------------- Identity keys ----------------

/// A party's (yi-agentd's or a client's) long-term identity: an Ed25519 signing key (proves identity to the relay)
/// and an X25519 static key (Noise end-to-end handshake). Private keys never leave their own machine.
#[derive(Clone, PartialEq)]
pub struct Identity {
    pub sign: [u8; 32],
    pub noise: [u8; 32],
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Identity({})", client_id_of(&self.sign_pub()))
    }
}

impl Identity {
    pub fn generate() -> Identity {
        Identity { sign: random_bytes(), noise: random_bytes() }
    }

    pub fn sign_pub(&self) -> [u8; 32] {
        ed25519_dalek::SigningKey::from_bytes(&self.sign).verifying_key().to_bytes()
    }

    pub fn noise_pub(&self) -> [u8; 32] {
        curve25519_dalek::MontgomeryPoint::mul_base_clamped(self.noise).to_bytes()
    }

    pub fn sign_msg(&self, msg: &[u8]) -> [u8; 64] {
        use ed25519_dalek::Signer;
        ed25519_dalek::SigningKey::from_bytes(&self.sign).sign(msg).to_bytes()
    }

    /// Serialize as `<signing private key>.<Noise private key>` (base64url)
    pub fn encode(&self) -> String {
        format!("{}.{}", b64(&self.sign), b64(&self.noise))
    }

    pub fn decode(s: &str) -> Result<Identity> {
        let (a, b) = s.trim().split_once('.').ok_or_else(|| anyhow!("身份密钥格式错误"))?;
        Ok(Identity { sign: unb64_32(a)?, noise: unb64_32(b)? })
    }
}

/// The device id is derived from the signing public key, so the relay can recompute it from the key to confirm whose id it is
pub fn device_id_of(sign_pub: &[u8; 32]) -> String {
    hex(&sha256(&[sign_pub])[..16])
}

/// Client id: a short identifier for the UI and revocation
pub fn client_id_of(sign_pub: &[u8; 32]) -> String {
    hex(&sha256(&[sign_pub])[..8])
}

// ---------------- Links ----------------

const LINK_VERSION: &str = "2";

fn query_of(uri: &str, prefix: &str) -> Option<HashMap<String, String>> {
    let q = uri.trim().strip_prefix(prefix)?;
    Some(
        q.split('&')
            .filter(|kv| !kv.is_empty())
            .map(|kv| {
                let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
                (k.to_string(), unpct(v))
            })
            .collect(),
    )
}

fn check_version(q: &HashMap<String, String>, what: &str) -> Result<()> {
    if q.get("v").map(String::as_str) != Some(LINK_VERSION) {
        bail!("这是旧版本的{what}，请在电脑上升级 Yi Console 后重新生成");
    }
    Ok(())
}

fn get<'a>(q: &'a HashMap<String, String>, k: &str) -> &'a str {
    q.get(k).map(String::as_str).unwrap_or("")
}

fn check_relay_url(u: &str) -> Result<()> {
    if !(u.starts_with("ws://") || u.starts_with("wss://")) {
        bail!("中继地址要以 ws:// 或 wss:// 开头");
    }
    Ok(())
}

/// Pairing QR code / pairing link: device public key + one-time pairing code, no long-term secrets.
/// `yiconsole://pair?v=2&relay=&fp=&device=&apk=&code=&name=`
#[derive(Debug, Clone, PartialEq)]
pub struct PairOffer {
    pub relay: String,
    pub fp: String,
    pub device: String,
    /// yi-agentd's Noise static public key
    pub apk: [u8; 32],
    /// One-time pairing code
    pub code: [u8; 32],
    pub name: String,
}

impl PairOffer {
    pub fn to_uri(&self) -> String {
        let mut u = format!(
            "yiconsole://pair?v={LINK_VERSION}&relay={}&device={}&apk={}&code={}&name={}",
            pct(&self.relay),
            pct(&self.device),
            b64(&self.apk),
            b64(&self.code),
            pct(&self.name)
        );
        if !self.fp.is_empty() {
            u.push_str(&format!("&fp={}", self.fp));
        }
        u
    }

    pub fn parse(uri: &str) -> Result<PairOffer> {
        let q = query_of(uri, "yiconsole://pair?").ok_or_else(|| anyhow!("不是 Yi Console 配对链接"))?;
        check_version(&q, "配对链接")?;
        let p = PairOffer {
            relay: get(&q, "relay").to_string(),
            fp: normalize_fp(get(&q, "fp"))?,
            device: get(&q, "device").to_string(),
            apk: unb64_32(get(&q, "apk")).context("配对链接里的设备公钥无效")?,
            code: unb64_32(get(&q, "code")).context("配对链接里的配对码无效")?,
            name: get(&q, "name").to_string(),
        };
        check_relay_url(&p.relay)?;
        if p.device.is_empty() {
            bail!("配对链接缺少设备 id");
        }
        Ok(p)
    }
}

/// Credentials the client saves after pairing (includes the client private key; never share).
/// `yiconsole://paired?v=2&relay=&fp=&device=&apk=&name=&me=`
#[derive(Debug, Clone, PartialEq)]
pub struct Credential {
    pub relay: String,
    pub fp: String,
    pub device: String,
    pub apk: [u8; 32],
    pub name: String,
    pub me: Identity,
}

impl Credential {
    pub fn to_uri(&self) -> String {
        let mut u = format!(
            "yiconsole://paired?v={LINK_VERSION}&relay={}&device={}&apk={}&name={}&me={}",
            pct(&self.relay),
            pct(&self.device),
            b64(&self.apk),
            pct(&self.name),
            self.me.encode()
        );
        if !self.fp.is_empty() {
            u.push_str(&format!("&fp={}", self.fp));
        }
        u
    }

    pub fn parse(uri: &str) -> Result<Credential> {
        let q = query_of(uri, "yiconsole://paired?").ok_or_else(|| {
            if uri.trim().starts_with("yiconsole://pair?") {
                anyhow!("这台设备还没完成配对")
            } else {
                anyhow!("配对凭据格式错误")
            }
        })?;
        check_version(&q, "配对凭据")?;
        let c = Credential {
            relay: get(&q, "relay").to_string(),
            fp: normalize_fp(get(&q, "fp"))?,
            device: get(&q, "device").to_string(),
            apk: unb64_32(get(&q, "apk"))?,
            name: get(&q, "name").to_string(),
            me: Identity::decode(get(&q, "me"))?,
        };
        check_relay_url(&c.relay)?;
        Ok(c)
    }
}

/// Access link for enabling the relay on a computer: address, certificate fingerprint, one-time invite code (for registration, may be empty).
/// `yiconsole://relay?v=2&url=&fp=&invite=`
#[derive(Debug, Clone, PartialEq)]
pub struct RelayAccess {
    pub url: String,
    pub fp: String,
    pub invite: String,
}

impl RelayAccess {
    pub fn to_uri(&self) -> String {
        let mut u = format!("yiconsole://relay?v={LINK_VERSION}&url={}", pct(&self.url));
        if !self.fp.is_empty() {
            u.push_str(&format!("&fp={}", self.fp));
        }
        if !self.invite.is_empty() {
            u.push_str(&format!("&invite={}", pct(&self.invite)));
        }
        u
    }

    /// Accepts `yiconsole://relay?...` access links, and pairing links too (only the address and fingerprint are used; there's no invite code)
    pub fn parse(uri: &str) -> Result<RelayAccess> {
        let uri = uri.trim();
        if uri.starts_with("yiconsole://pair?") {
            let p = PairOffer::parse(uri)?;
            return Ok(RelayAccess { url: p.relay, fp: p.fp, invite: String::new() });
        }
        let q = query_of(uri, "yiconsole://relay?").ok_or_else(|| anyhow!("不是 Yi Console 中继接入链接"))?;
        check_version(&q, "中继接入链接")?;
        let a = RelayAccess {
            url: get(&q, "url").to_string(),
            fp: normalize_fp(get(&q, "fp"))?,
            invite: get(&q, "invite").trim().to_string(),
        };
        check_relay_url(&a.url)?;
        Ok(a)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unpct_handles_non_ascii_after_percent() {
        // We used to slice %xx by &str, which panicked mid-character when followed by CJK text
        assert_eq!(unpct("%a电脑"), "%a电脑");
        assert_eq!(unpct("%电"), "%电");
        assert_eq!(unpct("%E4%B8%AD+x%2"), "中 x%2");
        assert_eq!(unpct("100%"), "100%");
        assert_eq!(unpct("%zz%41"), "%zzA");
    }

    #[test]
    fn identity_roundtrip_and_ids() {
        let id = Identity::generate();
        let back = Identity::decode(&id.encode()).unwrap();
        assert_eq!(back, id);
        assert_eq!(back.sign_pub(), id.sign_pub());
        assert_eq!(device_id_of(&id.sign_pub()).len(), 32);
        assert_eq!(client_id_of(&id.sign_pub()).len(), 16);
        assert_ne!(id.sign_pub(), id.noise_pub());
    }

    #[test]
    fn noise_pub_matches_snow() {
        // The X25519 public key we compute must match the one snow uses in the handshake
        let b = snow::Builder::new("Noise_IK_25519_ChaChaPoly_SHA256".parse().unwrap());
        let kp = b.generate_keypair().unwrap();
        let id = Identity { sign: [1; 32], noise: kp.private.clone().try_into().unwrap() };
        assert_eq!(id.noise_pub().as_slice(), kp.public.as_slice());
    }

    #[test]
    fn links_roundtrip() {
        let me = Identity::generate();
        let o = PairOffer {
            relay: "wss://relay.example.com:7878/hub".into(),
            fp: cert_fp(b"cert"),
            device: device_id_of(&me.sign_pub()),
            apk: me.noise_pub(),
            code: random_bytes(),
            name: "我的电脑 & x".into(),
        };
        assert_eq!(PairOffer::parse(&o.to_uri()).unwrap(), o);
        let c = Credential {
            relay: o.relay.clone(),
            fp: String::new(),
            device: o.device.clone(),
            apk: o.apk,
            name: o.name.clone(),
            me: Identity::generate(),
        };
        assert_eq!(Credential::parse(&c.to_uri()).unwrap(), c);
        let a = RelayAccess { url: "wss://1.2.3.4:7878".into(), fp: "ab".repeat(32), invite: "abc123".into() };
        assert_eq!(RelayAccess::parse(&a.to_uri()).unwrap(), a);
        let from_pair = RelayAccess::parse(&o.to_uri()).unwrap();
        assert_eq!((from_pair.url, from_pair.invite), (o.relay.clone(), String::new()));
        assert!(RelayAccess::parse("yiconsole://relay?v=2&url=http://x").is_err());
    }

    #[test]
    fn old_links_rejected() {
        let e = PairOffer::parse("yiconsole://pair?v=1&relay=wss://x&device=d&key=AAAA&token=t").unwrap_err();
        assert!(e.to_string().contains("旧版本"));
        assert!(RelayAccess::parse("yiconsole://relay?v=1&url=wss://x&token=t").is_err());
    }

    #[test]
    fn fp_normalize() {
        let fp = cert_fp(b"x");
        let colon: Vec<String> = fp.as_bytes().chunks(2).map(|c| String::from_utf8_lossy(c).to_uppercase()).collect();
        assert_eq!(normalize_fp(&format!("SHA256:{}", colon.join(":"))).unwrap(), fp);
        assert!(normalize_fp("abc").is_err());
        assert_eq!(normalize_fp("").unwrap(), "");
    }
}
