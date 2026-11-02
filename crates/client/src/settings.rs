//! 客户端自己的设置（不是 yi-agentd 的），存在数据目录的 `client-settings.json`。

use anyhow::{bail, Result};
use yi_link::proxy::{self, Mode};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProxySettings {
    /// auto（默认：环境变量 → 系统代理）/ off / manual
    #[serde(default = "auto")]
    pub mode: String,
    /// manual 时的代理地址，例如 http://127.0.0.1:7897
    #[serde(default)]
    pub url: String,
}

fn auto() -> String {
    "auto".into()
}

/// 系统通知（这台设备上的偏好，和 yi-agentd 的 ntfy 推送是两回事）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct NotifyPrefs {
    /// 总开关
    #[serde(default = "yes")]
    pub enabled: bool,
    /// AI 回复完成
    #[serde(default = "yes")]
    pub on_turn_end: bool,
    /// 等待审批
    #[serde(default = "yes")]
    pub on_permission: bool,
    /// away：窗口不在前台或者看的不是这个会话时才通知；always：总是通知
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

    /// 校验并让设置在本进程里生效
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
