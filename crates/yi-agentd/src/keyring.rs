//! Cross-platform OS keyring snapshot / restore helpers for tool OAuth credentials.
//!
//! Used primarily for HostActive tools (such as Antigravity / Cursor / Kiro / Qoder / CodeBuddy)
//! whose native CLI stores credentials in the platform keyring in addition to config files.
//! On headless Linux without a running D-Bus Secret Service session, operations degrade safely
//! to empty snapshots and no-op restores so CLI file-based credentials remain authoritative.

use anyhow::{anyhow, Result};
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use yi_proto::ToolKind;

/// Serializable snapshot of a single OS keyring entry belonging to a tool.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct KeyringEntry {
    /// Service name (macOS `svce`, Windows `TargetName`, Linux `attribute.service`).
    pub service: String,
    /// Account / username (`acct`, Windows `UserName`, Linux `attribute.username`).
    #[serde(default)]
    pub account: String,
    /// Optional human-readable label (e.g. `0x00000007` or `label`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Secret UTF-8 string when valid UTF-8; empty when `secret_b64` holds binary/non-UTF-8 bytes.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub secret: String,
    /// Base64-encoded raw bytes for non-UTF-8 secrets (e.g. Windows UTF-16LE blobs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_b64: Option<String>,
    /// Platform metadata such as `schema`, `persist`, or original Windows target name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub attrs: BTreeMap<String, String>,
}

impl KeyringEntry {
    pub fn from_bytes(service: String, account: String, label: Option<String>, bytes: &[u8]) -> Self {
        match std::str::from_utf8(bytes) {
            Ok(s) => Self {
                service,
                account,
                label,
                secret: s.to_string(),
                secret_b64: None,
                attrs: BTreeMap::new(),
            },
            Err(_) => Self {
                service,
                account,
                label,
                secret: String::new(),
                secret_b64: Some(base64::engine::general_purpose::STANDARD.encode(bytes)),
                attrs: BTreeMap::new(),
            },
        }
    }

    pub fn secret_bytes(&self) -> Vec<u8> {
        if let Some(b64) = &self.secret_b64 {
            base64::engine::general_purpose::STANDARD
                .decode(b64)
                .unwrap_or_else(|_| self.secret.as_bytes().to_vec())
        } else {
            self.secret.as_bytes().to_vec()
        }
    }

    #[allow(dead_code)]
    fn key(&self) -> (String, String) {
        (self.service.clone(), self.account.clone())
    }
}

struct KeyringSpec {
    prefixes: &'static [&'static str],
    known_pairs: &'static [(&'static str, &'static str)],
}

fn spec_for(tool: ToolKind) -> Option<KeyringSpec> {
    match tool {
        ToolKind::Antigravity => Some(KeyringSpec {
            prefixes: &["antigravity", "gemini-cli", "google.antigravity", "jetski"],
            known_pairs: &[
                ("antigravity-oauth-token", "oauth-token"),
                ("antigravity-oauth-token", "default"),
                ("antigravity-cli", "oauth-token"),
                ("antigravity-cli", "default"),
                ("antigravity", "oauth-token"),
                ("antigravity", "default"),
                ("gemini-cli-oauth", "main-account"),
            ],
        }),
        ToolKind::Cursor => Some(KeyringSpec {
            prefixes: &["cursor"],
            known_pairs: &[
                ("cursor-access-token", "cursor-user"),
                ("cursor-refresh-token", "cursor-user"),
                ("Cursor", "cursor-access-token"),
                ("cursor", "default"),
            ],
        }),
        ToolKind::Kiro => Some(KeyringSpec {
            prefixes: &["kiro", "q-cli", "amazon-q"],
            known_pairs: &[("kiro-cli", "default"), ("kiro", "default")],
        }),
        ToolKind::Qoder => Some(KeyringSpec {
            prefixes: &["qoder"],
            known_pairs: &[("qoder", "default"), ("qodercli", "default")],
        }),
        ToolKind::Codebuddy => Some(KeyringSpec {
            prefixes: &["codebuddy", "cbc"],
            known_pairs: &[("codebuddy", "default"), ("codebuddy-code", "default")],
        }),
        ToolKind::Claude => Some(KeyringSpec {
            prefixes: &["claude code", "claude-code", "claude"],
            known_pairs: &[("Claude Code-credentials", "")],
        }),
        ToolKind::Codex => Some(KeyringSpec {
            prefixes: &["codex auth", "codex", "openai-codex"],
            known_pairs: &[("Codex Auth", "default"), ("Codex Auth", "")],
        }),
        ToolKind::Shell | ToolKind::Files => None,
    }
}

fn matches_spec(spec: &KeyringSpec, service: &str, account: &str, label: Option<&str>) -> bool {
    let svc_low = service.to_ascii_lowercase();
    let acct_low = account.to_ascii_lowercase();
    let lbl_low = label.unwrap_or("").to_ascii_lowercase();
    // Avoid matching our own app or unrelated system services if generic prefix is used.
    if svc_low.contains("yi-console") || svc_low.contains("yi_console") {
        return false;
    }
    for (ks, ka) in spec.known_pairs {
        if service == *ks && (ka.is_empty() || account == *ka) {
            return true;
        }
    }
    spec.prefixes.iter().any(|p| {
        let p = p.to_ascii_lowercase();
        svc_low == p
            || svc_low.starts_with(&format!("{p}-"))
            || svc_low.starts_with(&format!("{p}:"))
            || svc_low.starts_with(&format!("{p}_"))
            || svc_low.starts_with(&format!("{p} "))
            || svc_low.contains(&format!(".{p}"))
            || lbl_low == p
            || acct_low.starts_with(&format!("{p}-"))
    })
}

/// Snapshot all OS keyring entries belonging to `tool`.
pub fn snapshot_tool(tool: ToolKind) -> Result<Vec<KeyringEntry>> {
    let Some(spec) = spec_for(tool) else {
        return Ok(vec![]);
    };
    platform::snapshot(&spec)
}

/// Replace OS keyring entries belonging to `tool` with `entries`.
pub fn restore_tool(tool: ToolKind, entries: &[KeyringEntry]) -> Result<()> {
    let Some(spec) = spec_for(tool) else {
        return Ok(());
    };
    platform::restore(&spec, entries)
}

/// Delete all OS keyring entries belonging to `tool`.
#[allow(dead_code)]
pub fn clear_tool(tool: ToolKind) -> Result<()> {
    let Some(spec) = spec_for(tool) else {
        return Ok(());
    };
    platform::restore(&spec, &[])
}

/// Parse `security dump-keychain` text into `(service, account, label)` tuples for `genp` items.
#[cfg(any(target_os = "macos", test))]
fn parse_macos_keychain_dump(dump: &str) -> Vec<(String, String, Option<String>)> {
    let mut out = Vec::new();
    let mut is_genp = false;
    let mut service: Option<String> = None;
    let mut account: Option<String> = None;
    let mut label: Option<String> = None;

    let flush = |out: &mut Vec<(String, String, Option<String>)>,
                 is_genp: &mut bool,
                 service: &mut Option<String>,
                 account: &mut Option<String>,
                 label: &mut Option<String>| {
        if *is_genp {
            if let Some(svc) = service.take() {
                let acct = account.take().unwrap_or_default();
                out.push((svc, acct, label.take()));
            }
        }
        *is_genp = false;
        *service = None;
        *account = None;
        *label = None;
    };

    for raw in dump.lines() {
        let line = raw.trim();
        if line.starts_with("keychain:") {
            flush(&mut out, &mut is_genp, &mut service, &mut account, &mut label);
            continue;
        }
        if line.starts_with("class:") && line.contains("\"genp\"") {
            is_genp = true;
            continue;
        }
        if !is_genp {
            continue;
        }
        if let Some(val) = extract_quoted_blob(line, "\"svce\"<blob>=") {
            service = Some(val);
        } else if let Some(val) = extract_quoted_blob(line, "\"acct\"<blob>=") {
            account = Some(val);
        } else if let Some(val) = extract_quoted_blob(line, "0x00000007 <blob>=") {
            label = Some(val);
        }
    }
    flush(&mut out, &mut is_genp, &mut service, &mut account, &mut label);
    out
}

#[cfg(any(target_os = "macos", test))]
fn extract_quoted_blob(line: &str, prefix: &str) -> Option<String> {
    let rest = line.strip_prefix(prefix)?.trim();
    if rest == "<NULL>" {
        return None;
    }
    if let Some(inner) = rest.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        return Some(inner.replace("\\\"", "\"").replace("\\\\", "\\"));
    }
    None
}

/// Parse `secret-tool search --all ...` stdout blocks into `KeyringEntry`.
#[cfg(any(target_os = "linux", test))]
fn parse_secret_tool_search(out: &str) -> Vec<KeyringEntry> {
    let mut entries = Vec::new();
    let mut label: Option<String> = None;
    let mut secret: Option<String> = None;
    let mut attrs = BTreeMap::new();

    let flush = |entries: &mut Vec<KeyringEntry>,
                 label: &mut Option<String>,
                 secret: &mut Option<String>,
                 attrs: &mut BTreeMap<String, String>| {
        if let Some(sec) = secret.take() {
            let service = attrs
                .get("service")
                .cloned()
                .or_else(|| label.clone())
                .unwrap_or_default();
            let account = attrs
                .get("username")
                .or_else(|| attrs.get("account"))
                .cloned()
                .unwrap_or_default();
            if !service.is_empty() {
                let mut e = KeyringEntry::from_bytes(service, account, label.take(), sec.as_bytes());
                e.attrs = std::mem::take(attrs);
                entries.push(e);
            }
        }
        *label = None;
        *secret = None;
        attrs.clear();
    };

    for raw in out.lines() {
        let line = raw.trim();
        if line.starts_with("[/") && line.ends_with(']') {
            flush(&mut entries, &mut label, &mut secret, &mut attrs);
            continue;
        }
        if let Some(v) = line.strip_prefix("label = ") {
            label = Some(v.to_string());
        } else if let Some(v) = line.strip_prefix("secret = ") {
            secret = Some(v.to_string());
        } else if let Some(rest) = line.strip_prefix("attribute.") {
            if let Some((k, v)) = rest.split_once(" = ") {
                attrs.insert(k.to_string(), v.to_string());
            }
        }
    }
    flush(&mut entries, &mut label, &mut secret, &mut attrs);
    entries
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;
    use std::process::Command;

    fn security_bin() -> &'static str {
        "/usr/bin/security"
    }

    pub fn snapshot(spec: &KeyringSpec) -> Result<Vec<KeyringEntry>> {
        let user = std::env::var("USER").unwrap_or_default();
        let mut candidates: Vec<(String, String, Option<String>)> = Vec::new();
        let mut seen = HashSet::new();

        if let Ok(out) = Command::new(security_bin()).arg("dump-keychain").output() {
            let text = String::from_utf8_lossy(&out.stdout);
            for (svc, acct, lbl) in parse_macos_keychain_dump(&text) {
                if matches_spec(spec, &svc, &acct, lbl.as_deref()) && seen.insert((svc.clone(), acct.clone())) {
                    candidates.push((svc, acct, lbl));
                }
            }
        }

        for (ks, ka) in spec.known_pairs {
            let acct = if ka.is_empty() { user.clone() } else { (*ka).to_string() };
            let svc = (*ks).to_string();
            if seen.insert((svc.clone(), acct.clone())) {
                candidates.push((svc, acct, None));
            }
        }

        let mut entries = Vec::new();
        for (svc, acct, lbl) in candidates {
            let mut cmd = Command::new(security_bin());
            cmd.arg("find-generic-password").arg("-s").arg(&svc);
            if !acct.is_empty() {
                cmd.arg("-a").arg(&acct);
            }
            cmd.arg("-w");
            let Ok(out) = cmd.output() else { continue };
            if !out.status.success() {
                continue;
            }
            let mut raw = out.stdout;
            if raw.ends_with(b"\n") {
                raw.pop();
            }
            if raw.ends_with(b"\r") {
                raw.pop();
            }
            entries.push(KeyringEntry::from_bytes(svc, acct, lbl, &raw));
        }
        Ok(entries)
    }

    pub fn restore(spec: &KeyringSpec, entries: &[KeyringEntry]) -> Result<()> {
        let keep: HashSet<(String, String)> = entries.iter().map(KeyringEntry::key).collect();
        if let Ok(existing) = snapshot(spec) {
            for e in existing {
                if !keep.contains(&e.key()) {
                    let mut cmd = Command::new(security_bin());
                    cmd.arg("delete-generic-password").arg("-s").arg(&e.service);
                    if !e.account.is_empty() {
                        cmd.arg("-a").arg(&e.account);
                    }
                    let _ = cmd.output();
                }
            }
        }
        for e in entries {
            let bytes = e.secret_bytes();
            let mut cmd = Command::new(security_bin());
            cmd.arg("add-generic-password").arg("-U").arg("-s").arg(&e.service);
            if !e.account.is_empty() {
                cmd.arg("-a").arg(&e.account);
            }
            if e.secret_b64.is_some() {
                let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
                cmd.arg("-X").arg(hex);
            } else {
                cmd.arg("-w").arg(&e.secret);
            }
            let out = cmd.output()?;
            if !out.status.success() {
                return Err(anyhow!(
                    "macOS security add-generic-password failed for {}: {}",
                    e.service,
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
            }
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use super::*;
    use std::io::Write;
    use std::process::{Command, Stdio};

    fn has_secret_tool() -> bool {
        if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none() {
            return false;
        }
        which_secret_tool().is_some()
    }

    fn which_secret_tool() -> Option<std::path::PathBuf> {
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path)
            .map(|d| d.join("secret-tool"))
            .find(|p| p.is_file())
    }

    pub fn snapshot(spec: &KeyringSpec) -> Result<Vec<KeyringEntry>> {
        if !has_secret_tool() {
            return Ok(vec![]);
        }
        let mut out_entries = Vec::new();
        let mut seen = HashSet::new();

        if let Ok(out) = Command::new("secret-tool")
            .args(["search", "--all", "xdg:schema", "org.freedesktop.Secret.Generic"])
            .output()
        {
            if out.status.success() {
                let text = String::from_utf8_lossy(&out.stdout);
                for entry in parse_secret_tool_search(&text) {
                    if matches_spec(spec, &entry.service, &entry.account, entry.label.as_deref())
                        && seen.insert(entry.key())
                    {
                        out_entries.push(entry);
                    }
                }
            }
        }

        // Also query exact known pairs in case the item uses a different schema.
        let user = std::env::var("USER").unwrap_or_default();
        for (ks, ka) in spec.known_pairs {
            let acct = if ka.is_empty() { user.as_str() } else { *ka };
            if acct.is_empty() || seen.contains(&((*ks).to_string(), acct.to_string())) {
                continue;
            }
            if let Ok(out) = Command::new("secret-tool")
                .args(["lookup", "service", ks, "username", acct])
                .output()
            {
                if out.status.success() && !out.stdout.is_empty() {
                    let mut raw = out.stdout;
                    if raw.ends_with(b"\n") {
                        raw.pop();
                    }
                    let e = KeyringEntry::from_bytes((*ks).to_string(), acct.to_string(), None, &raw);
                    if seen.insert(e.key()) {
                        out_entries.push(e);
                    }
                }
            }
        }

        Ok(out_entries)
    }

    pub fn restore(spec: &KeyringSpec, entries: &[KeyringEntry]) -> Result<()> {
        if !has_secret_tool() {
            return Ok(());
        }
        let keep: HashSet<(String, String)> = entries.iter().map(KeyringEntry::key).collect();
        if let Ok(existing) = snapshot(spec) {
            for e in existing {
                if !keep.contains(&e.key()) {
                    let _ = Command::new("secret-tool")
                        .args(["clear", "service", &e.service, "username", &e.account])
                        .output();
                }
            }
        }
        for e in entries {
            let label = e
                .label
                .clone()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| e.service.clone());
            let mut cmd = Command::new("secret-tool");
            cmd.arg("store").arg(format!("--label={label}"));
            if e.attrs.is_empty() {
                cmd.args(["service", &e.service, "username", &e.account]);
            } else {
                for (k, v) in &e.attrs {
                    if k != "xdg:schema" {
                        cmd.arg(k).arg(v);
                    }
                }
                if !e.attrs.contains_key("service") {
                    cmd.args(["service", &e.service]);
                }
                if !e.attrs.contains_key("username") && !e.account.is_empty() {
                    cmd.args(["username", &e.account]);
                }
            }
            cmd.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped());
            let mut child = cmd.spawn()?;
            if let Some(mut stdin) = child.stdin.take() {
                stdin.write_all(&e.secret_bytes())?;
            }
            let out = child.wait_with_output()?;
            if !out.status.success() {
                return Err(anyhow!(
                    "secret-tool store failed for {}: {}",
                    e.service,
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
            }
        }
        Ok(())
    }
}

#[cfg(target_os = "windows")]
mod platform {
    use super::*;
    use std::ffi::OsString;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use windows_sys::Win32::Security::Credentials::{
        CredDeleteW, CredEnumerateW, CredFree, CredWriteW, CREDENTIALW, CRED_PERSIST_LOCAL_MACHINE,
        CRED_TYPE_GENERIC,
    };

    unsafe fn pwstr_to_string(p: *mut u16) -> String {
        if p.is_null() {
            return String::new();
        }
        let mut len = 0usize;
        while *p.add(len) != 0 {
            len += 1;
        }
        OsString::from_wide(std::slice::from_raw_parts(p, len))
            .to_string_lossy()
            .into_owned()
    }

    fn to_wide(s: &str) -> Vec<u16> {
        OsString::from(s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    fn entry_target_matches(spec: &KeyringSpec, target: &str, user: &str) -> bool {
        // go-keyring formats Windows TargetName as `service:user` or `service`
        if let Some((svc, _)) = target.split_once(':') {
            if matches_spec(spec, svc, user, None) {
                return true;
            }
        }
        matches_spec(spec, target, user, None)
    }

    pub fn snapshot(spec: &KeyringSpec) -> Result<Vec<KeyringEntry>> {
        let mut count: u32 = 0;
        let mut creds: *mut *mut CREDENTIALW = std::ptr::null_mut();
        let ok = unsafe { CredEnumerateW(std::ptr::null(), 0, &mut count, &mut creds) };
        if ok == 0 || creds.is_null() {
            return Ok(vec![]);
        }
        let mut out = Vec::new();
        unsafe {
            let slice = std::slice::from_raw_parts(creds, count as usize);
            for &ptr in slice {
                if ptr.is_null() {
                    continue;
                }
                let c = &*ptr;
                if c.Type != CRED_TYPE_GENERIC {
                    continue;
                }
                let target = pwstr_to_string(c.TargetName);
                let user = pwstr_to_string(c.UserName);
                if !entry_target_matches(spec, &target, &user) {
                    continue;
                }
                let bytes = if c.CredentialBlob.is_null() || c.CredentialBlobSize == 0 {
                    Vec::new()
                } else {
                    std::slice::from_raw_parts(c.CredentialBlob, c.CredentialBlobSize as usize).to_vec()
                };
                let mut e = KeyringEntry::from_bytes(target.clone(), user, None, &bytes);
                e.attrs.insert("persist".into(), c.Persist.to_string());
                out.push(e);
            }
            CredFree(creds as *const core::ffi::c_void);
        }
        Ok(out)
    }

    pub fn restore(spec: &KeyringSpec, entries: &[KeyringEntry]) -> Result<()> {
        let keep: HashSet<String> = entries.iter().map(|e| e.service.clone()).collect();
        if let Ok(existing) = snapshot(spec) {
            for e in existing {
                if !keep.contains(&e.service) {
                    let w = to_wide(&e.service);
                    unsafe {
                        CredDeleteW(w.as_ptr(), CRED_TYPE_GENERIC, 0);
                    }
                }
            }
        }
        for e in entries {
            let mut target_w = to_wide(&e.service);
            let mut user_w = to_wide(&e.account);
            let mut blob = e.secret_bytes();
            let persist = e
                .attrs
                .get("persist")
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(CRED_PERSIST_LOCAL_MACHINE);
            let cred = CREDENTIALW {
                Type: CRED_TYPE_GENERIC,
                TargetName: target_w.as_mut_ptr(),
                CredentialBlobSize: blob.len() as u32,
                CredentialBlob: if blob.is_empty() {
                    std::ptr::null_mut()
                } else {
                    blob.as_mut_ptr()
                },
                Persist: persist,
                UserName: if e.account.is_empty() {
                    std::ptr::null_mut()
                } else {
                    user_w.as_mut_ptr()
                },
                ..Default::default()
            };
            let ok = unsafe { CredWriteW(&cred, 0) };
            if ok == 0 {
                return Err(anyhow!(
                    "Windows CredWriteW failed for {}: {}",
                    e.service,
                    std::io::Error::last_os_error()
                ));
            }
        }
        Ok(())
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
mod platform {
    use super::*;
    pub fn snapshot(_spec: &KeyringSpec) -> Result<Vec<KeyringEntry>> {
        Ok(vec![])
    }
    pub fn restore(_spec: &KeyringSpec, _entries: &[KeyringEntry]) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_matching_excludes_self_and_matches_tool_prefixes() {
        let agy = spec_for(ToolKind::Antigravity).unwrap();
        assert!(matches_spec(&agy, "antigravity-oauth-token", "oauth-token", None));
        assert!(matches_spec(&agy, "gemini-cli-oauth", "main-account", None));
        assert!(matches_spec(&agy, "antigravity:user@example.com", "user", None));
        assert!(!matches_spec(&agy, "yi-console-antigravity", "user", None));

        let claude = spec_for(ToolKind::Claude).unwrap();
        assert!(matches_spec(&claude, "Claude Code-credentials", "alice", None));
    }

    #[test]
    fn parse_macos_dump_extracts_genp_items() {
        let dump = r#"
keychain: "/Users/a/Library/Keychains/login.keychain-db"
version: 512
class: "genp"
attributes:
    0x00000007 <blob>="antigravity-oauth-token"
    "acct"<blob>="oauth-token"
    "svce"<blob>="antigravity-oauth-token"
keychain: "/Users/a/Library/Keychains/login.keychain-db"
class: "inet"
attributes:
    "svce"<blob>="ignore-me"
keychain: "/Users/a/Library/Keychains/login.keychain-db"
class: "genp"
attributes:
    0x00000007 <blob>="Claude Code-credentials"
    "acct"<blob>="alice"
    "svce"<blob>="Claude Code-credentials"
"#;
        let items = parse_macos_keychain_dump(dump);
        assert_eq!(
            items,
            vec![
                (
                    "antigravity-oauth-token".to_string(),
                    "oauth-token".to_string(),
                    Some("antigravity-oauth-token".to_string())
                ),
                (
                    "Claude Code-credentials".to_string(),
                    "alice".to_string(),
                    Some("Claude Code-credentials".to_string())
                ),
            ]
        );
    }

    #[test]
    fn parse_secret_tool_output_extracts_attrs_and_secret() {
        let text = "\
[/org/freedesktop/secrets/collection/login/1]
label = antigravity-oauth-token
secret = {\"access_token\":\"abc\"}
created = 2026-11-15 10:00:00
attribute.service = antigravity-oauth-token
attribute.username = oauth-token
attribute.xdg:schema = org.freedesktop.Secret.Generic
";
        let entries = parse_secret_tool_search(text);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].service, "antigravity-oauth-token");
        assert_eq!(entries[0].account, "oauth-token");
        assert_eq!(entries[0].secret, "{\"access_token\":\"abc\"}");
    }

    #[test]
    fn binary_secret_roundtrip() {
        let raw = vec![0xffu8, 0x00, 0x41, 0xfe];
        let e = KeyringEntry::from_bytes("svc".into(), "acct".into(), None, &raw);
        assert!(e.secret.is_empty());
        assert!(e.secret_b64.is_some());
        assert_eq!(e.secret_bytes(), raw);
    }
}
