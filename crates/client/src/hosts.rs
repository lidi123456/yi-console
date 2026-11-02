//! 主机配置：本机 / SSH / 中继，存成 JSON 文件（含密码等敏感信息，权限 0600）。

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HostKind {
    Local,
    Ssh,
    Relay,
    /// 经某台中继主机（电脑）的 yi-agentd 转发到它配的 SSH 主机，手机连内网服务器用
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
    /// 私钥文件路径（桌面端）
    #[serde(default)]
    pub key_path: String,
    /// 私钥内容（手机上没有文件系统可选时直接粘贴）
    #[serde(default)]
    pub key_text: String,
    #[serde(default)]
    pub passphrase: String,
    #[serde(default)]
    pub password: String,
    /// 跳板机：引用另一台 SSH 主机的 id
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
    /// 中继配对链接 yiconsole://pair?...
    #[serde(default)]
    pub pairing: Option<String>,
    #[serde(default)]
    pub auto_connect: bool,
    /// 首次部署后注册成自启服务
    #[serde(default = "yes")]
    pub install_service: bool,
    /// 探测到的远程平台，缓存下来省一次往返
    #[serde(default)]
    pub remote_os: Option<String>,
    #[serde(default)]
    pub remote_arch: Option<String>,
    /// SSH 主机：允许已配对的设备（手机）经本机 yi-agentd 访问
    #[serde(default = "yes")]
    pub relay_share: bool,
    /// 转发主机：经哪台中继主机，以及它在那台电脑上的主机 id
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
