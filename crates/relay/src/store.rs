//! The relay's persistent data (all in the `--data` directory):
//! - `relay-devices.json`: registered devices (public keys only)
//! - `relay-invites.json`: invite codes (hashes only)
//! - `relay-meta.json`: public address and certificate fingerprint, used by the command line to build invite links
//!
//! The running relay and commands such as `yi-relay invite / revoke` all read and write these files directly; they always reread before changing,
//! and write via temp file + rename. The relay checks for file changes every 5 seconds (revocation takes effect right away).

use anyhow::{anyhow, bail, Context, Result};
use yi_link::{auth, b64, unb64_32};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceRec {
    /// Ed25519 signing public key (base64url)
    pub pub_key: String,
    #[serde(default)]
    pub note: String,
    pub admitted_at: u64,
    /// How it was registered: invite:<invite id> / open
    #[serde(default)]
    pub via: String,
    /// Registered through a device-issued invite: id of the device that issued it
    #[serde(default)]
    pub invited_by: String,
    #[serde(default)]
    pub last_seen: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InviteRec {
    pub id: String,
    /// Hash of the invite code (base64url), see [`auth::invite_hash`]
    pub hash: String,
    pub created_at: u64,
    pub expires_at: u64,
    pub uses_left: u32,
    #[serde(default)]
    pub note: String,
    /// admin (generated on the server) or the id of the device that issued it
    pub by: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Meta {
    #[serde(default)]
    pub public_url: String,
    #[serde(default)]
    pub fp: String,
}

pub struct Store {
    dir: PathBuf,
}

fn read_json<T: for<'de> Deserialize<'de> + Default>(p: &Path) -> T {
    std::fs::read_to_string(p).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

fn write_json<T: Serialize>(p: &Path, v: &T) -> Result<()> {
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(v)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, p).with_context(|| format!("写入 {} 失败", p.display()))
}

fn mtime(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

impl Store {
    pub fn new(dir: &Path) -> Result<Store> {
        std::fs::create_dir_all(dir).with_context(|| format!("创建数据目录 {} 失败", dir.display()))?;
        Ok(Store { dir: dir.to_path_buf() })
    }

    fn devices_path(&self) -> PathBuf {
        self.dir.join("relay-devices.json")
    }

    fn invites_path(&self) -> PathBuf {
        self.dir.join("relay-invites.json")
    }

    fn meta_path(&self) -> PathBuf {
        self.dir.join("relay-meta.json")
    }

    pub fn devices(&self) -> BTreeMap<String, DeviceRec> {
        read_json(&self.devices_path())
    }

    pub fn devices_mtime(&self) -> Option<SystemTime> {
        mtime(&self.devices_path())
    }

    /// device id -> signing public key
    pub fn device_keys(&self) -> BTreeMap<String, [u8; 32]> {
        self.devices()
            .into_iter()
            .filter_map(|(id, d)| Some((id, unb64_32(&d.pub_key).ok()?)))
            .collect()
    }

    pub fn invites(&self) -> Vec<InviteRec> {
        let mut v: Vec<InviteRec> = read_json(&self.invites_path());
        let t = now();
        v.retain(|i| i.expires_at > t && i.uses_left > 0);
        v
    }

    pub fn meta(&self) -> Meta {
        read_json(&self.meta_path())
    }

    pub fn save_meta(&self, m: &Meta) -> Result<()> {
        write_json(&self.meta_path(), m)
    }

    /// Generate an invite code; returns (record, plaintext invite code)
    pub fn create_invite(&self, ttl: u64, uses: u32, note: &str, by: &str) -> Result<(InviteRec, String)> {
        if uses == 0 {
            bail!("可用次数至少为 1");
        }
        let code = yi_link::hex(&yi_link::random_bytes::<16>());
        let t = now();
        let rec = InviteRec {
            id: yi_link::hex(&yi_link::random_bytes::<3>()),
            hash: b64(&auth::invite_hash(&code)),
            created_at: t,
            expires_at: t + ttl,
            uses_left: uses,
            note: note.to_string(),
            by: by.to_string(),
        };
        let mut all = self.invites();
        all.push(rec.clone());
        write_json(&self.invites_path(), &all)?;
        Ok((rec, code))
    }

    /// Use up one invite; returns None if invalid, expired or used up
    pub fn consume_invite(&self, code: &str) -> Result<Option<InviteRec>> {
        let h = auth::invite_hash(code);
        let mut all = self.invites();
        let mut found = None;
        for (i, rec) in all.iter().enumerate() {
            if unb64_32(&rec.hash).map(|x| yi_link::ct_eq(&x, &h)).unwrap_or(false) {
                found = Some(i);
            }
        }
        let Some(i) = found else { return Ok(None) };
        let rec = all[i].clone();
        all[i].uses_left -= 1;
        all.retain(|r| r.uses_left > 0);
        write_json(&self.invites_path(), &all)?;
        Ok(Some(rec))
    }

    pub fn revoke_invite(&self, id: &str) -> Result<bool> {
        let mut all = self.invites();
        let n = all.len();
        all.retain(|r| r.id != id);
        write_json(&self.invites_path(), &all)?;
        Ok(all.len() != n)
    }

    pub fn admit(&self, device: &str, pub_key: &[u8; 32], via: &str, invited_by: &str, note: &str) -> Result<()> {
        let mut all = self.devices();
        let t = now();
        all.insert(
            device.to_string(),
            DeviceRec {
                pub_key: b64(pub_key),
                note: note.to_string(),
                admitted_at: t,
                via: via.to_string(),
                invited_by: invited_by.to_string(),
                last_seen: t,
            },
        );
        write_json(&self.devices_path(), &all)
    }

    pub fn touch(&self, device: &str) {
        let mut all = self.devices();
        if let Some(d) = all.get_mut(device) {
            d.last_seen = now();
            let _ = write_json(&self.devices_path(), &all);
        }
    }

    /// Revoke a device; with `cascade`, also devices registered through invites it issued (recursively). Returns the revoked ids
    pub fn revoke(&self, device: &str, cascade: bool) -> Result<Vec<String>> {
        let mut all = self.devices();
        if !all.contains_key(device) {
            return Err(anyhow!("没有这台设备：{device}"));
        }
        let mut gone = vec![device.to_string()];
        let mut i = 0;
        while cascade && i < gone.len() {
            let parent = gone[i].clone();
            for (id, d) in &all {
                if d.invited_by == parent && !gone.contains(id) {
                    gone.push(id.clone());
                }
            }
            i += 1;
        }
        all.retain(|id, _| !gone.contains(id));
        write_json(&self.devices_path(), &all)?;
        // Unused invites issued by revoked devices are voided too
        let mut inv = self.invites();
        inv.retain(|r| !gone.contains(&r.by));
        write_json(&self.invites_path(), &inv)?;
        Ok(gone)
    }
}

/// 24h / 10m / 7d / 3600 (seconds)
pub fn parse_ttl(s: &str) -> Result<u64> {
    let s = s.trim();
    let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: u64 = num.parse().map_err(|_| anyhow!("时长格式错误：{s}（例如 24h、10m、7d）"))?;
    let mul = match unit {
        "" | "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86400,
        _ => bail!("时长格式错误：{s}（例如 24h、10m、7d）"),
    };
    Ok(n * mul)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> Store {
        let d = std::env::temp_dir().join(format!("yi-relay-test-{}", yi_link::random_id()));
        Store::new(&d).unwrap()
    }

    #[test]
    fn invites_are_single_use() {
        let s = tmp();
        let (_, code) = s.create_invite(600, 1, "", "admin").unwrap();
        assert!(s.consume_invite("wrong").unwrap().is_none());
        assert!(s.consume_invite(&code).unwrap().is_some());
        assert!(s.consume_invite(&code).unwrap().is_none());
        let (_, code) = s.create_invite(600, 2, "", "admin").unwrap();
        assert!(s.consume_invite(&code).unwrap().is_some());
        assert!(s.consume_invite(&code).unwrap().is_some());
        assert!(s.consume_invite(&code).unwrap().is_none());
        let (_, code) = s.create_invite(0, 1, "", "admin").unwrap();
        assert!(s.consume_invite(&code).unwrap().is_none(), "过期的邀请");
        let (rec, code) = s.create_invite(600, 1, "", "admin").unwrap();
        assert!(s.revoke_invite(&rec.id).unwrap());
        assert!(s.consume_invite(&code).unwrap().is_none());
        // The file never stores plaintext
        let raw = std::fs::read_to_string(s.invites_path()).unwrap_or_default();
        assert!(!raw.contains(&code));
    }

    #[test]
    fn revoke_cascade() {
        let s = tmp();
        s.admit("a", &[1; 32], "invite:x", "", "").unwrap();
        s.admit("b", &[2; 32], "invite:y", "a", "").unwrap();
        s.admit("c", &[3; 32], "invite:z", "b", "").unwrap();
        s.admit("d", &[4; 32], "invite:w", "", "").unwrap();
        s.create_invite(600, 1, "", "b").unwrap();
        assert_eq!(s.revoke("a", false).unwrap(), vec!["a"]);
        s.admit("a", &[1; 32], "invite:x", "", "").unwrap();
        let mut gone = s.revoke("a", true).unwrap();
        gone.sort();
        assert_eq!(gone, vec!["a", "b", "c"]);
        assert_eq!(s.devices().keys().cloned().collect::<Vec<_>>(), vec!["d"]);
        assert!(s.invites().is_empty());
        assert!(s.revoke("zzz", false).is_err());
    }

    #[test]
    fn ttl() {
        assert_eq!(parse_ttl("24h").unwrap(), 86400);
        assert_eq!(parse_ttl("10m").unwrap(), 600);
        assert_eq!(parse_ttl("90").unwrap(), 90);
        assert!(parse_ttl("1y").is_err());
    }
}
