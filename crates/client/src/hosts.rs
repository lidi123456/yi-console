//! Host config: local / SSH / relay, stored as a JSON file (contains secrets such as passwords, mode 0600).

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HostKind {
    Local,
    Ssh,
    Relay,
    /// Forwarded through a relay host's (computer's) yi-agentd to an SSH host configured there; lets phones reach servers on an internal network
    Proxy,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SshTarget {
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub user: String,
    /// agent / key / password
    #[serde(default = "default_auth")]
    pub auth: String,
    /// Private key file path (desktop)
    #[serde(default)]
    pub key_path: String,
    /// Private key content (pasted directly on phones, where there's no file system to pick from)
    #[serde(default)]
    pub key_text: String,
    #[serde(default)]
    pub passphrase: String,
    #[serde(default)]
    pub password: String,
    /// Jump host: references another SSH host by id
    #[serde(default)]
    pub jump_host_id: String,
}

fn default_port() -> u16 {
    22
}

fn default_auth() -> String {
    "agent".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostConfig {
    #[serde(default)]
    pub id: String,
    pub name: String,
    pub kind: HostKind,
    #[serde(default)]
    pub ssh: Option<SshTarget>,
    /// Relay pairing link yiconsole://pair?...
    #[serde(default)]
    pub pairing: Option<String>,
    #[serde(default)]
    pub auto_connect: bool,
    /// Register as an autostart service after the first deployment
    #[serde(default = "yes")]
    pub install_service: bool,
    /// Detected remote platform, cached to save a round trip
    #[serde(default)]
    pub remote_os: Option<String>,
    #[serde(default)]
    pub remote_arch: Option<String>,
    /// SSH host: allow paired devices (phones) to reach it through the local yi-agentd
    #[serde(default = "yes")]
    pub relay_share: bool,
    /// Forwarded host: which relay host it goes through, and its host id on that computer
    #[serde(default)]
    pub via_host_id: Option<String>,
    #[serde(default)]
    pub target_id: Option<String>,
}

fn yes() -> bool {
    true
}

pub struct HostStore {
    path: PathBuf,
    pub hosts: Vec<HostConfig>,
}

impl HostStore {
    pub fn load(data_dir: &std::path::Path) -> HostStore {
        let path = data_dir.join("hosts.json");
        let hosts = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        HostStore { path, hosts }
    }

    pub fn save(&self) -> Result<()> {
        if let Some(d) = self.path.parent() {
            std::fs::create_dir_all(d)?;
        }
        std::fs::write(&self.path, serde_json::to_string_pretty(&self.hosts)?)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }

    pub fn get(&self, id: &str) -> Option<HostConfig> {
        self.hosts.iter().find(|h| h.id == id).cloned()
    }

    pub fn upsert(&mut self, mut h: HostConfig) -> HostConfig {
        if h.id.is_empty() {
            h.id = uuid::Uuid::new_v4().to_string();
        }
        match self.hosts.iter_mut().find(|x| x.id == h.id) {
            Some(x) => *x = h.clone(),
            None => self.hosts.push(h.clone()),
        }
        h
    }

    pub fn remove(&mut self, id: &str) {
        self.hosts.retain(|h| h.id != id);
    }
}
