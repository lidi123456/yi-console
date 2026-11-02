//! The client's own settings (not yi-agentd's), stored in `client-settings.json` in the data directory.

use anyhow::{bail, Result};
use yi_link::proxy::{self, Mode};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProxySettings {
    /// auto (default: environment variables → system proxy) / off / manual
    #[serde(default = "auto")]
    pub mode: String,
    /// Proxy address for manual, e.g. http://127.0.0.1:7897
    #[serde(default)]
    pub url: String,
}

fn auto() -> String {
    "auto".into()
}

/// System notifications (this device's preference; unrelated to yi-agentd's ntfy push)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct NotifyPrefs {
    /// Master switch
    #[serde(default = "yes")]
    pub enabled: bool,
    /// AI reply finished
    #[serde(default = "yes")]
    pub on_turn_end: bool,
    /// Awaiting approval
    #[serde(default = "yes")]
    pub on_permission: bool,
    /// away: notify only when the window isn't in front or another session is being viewed; always: always notify
    #[serde(default = "away")]
    pub when: String,
}

fn yes() -> bool {
    true
}

fn away() -> String {
    "away".into()
}

impl Default for NotifyPrefs {
    fn default() -> Self {
        NotifyPrefs { enabled: true, on_turn_end: true, on_permission: true, when: away() }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientSettings {
    #[serde(default)]
    pub proxy: ProxySettings,
    #[serde(default)]
    pub notify: NotifyPrefs,
}

fn file(dir: &Path) -> PathBuf {
    dir.join("client-settings.json")
}

impl ClientSettings {
    pub fn load(dir: &Path) -> Self {
        std::fs::read(file(dir))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, dir: &Path) -> Result<()> {
        std::fs::write(file(dir), serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }

    /// Validate and apply the settings in this process
    pub fn apply(&self) -> Result<()> {
        let m = match self.proxy.mode.as_str() {
            "" | "auto" => Mode::Auto,
            "off" => Mode::Off,
            "manual" => {
                if proxy::parse_proxy(&self.proxy.url).is_none() {
                    bail!("代理地址格式不对，应为 http://主机:端口（不支持 socks）");
                }
                Mode::Manual(self.proxy.url.trim().to_string())
            }
            m => bail!("未知的代理模式: {m}"),
        };
        proxy::set_mode(m);
        Ok(())
    }
}
