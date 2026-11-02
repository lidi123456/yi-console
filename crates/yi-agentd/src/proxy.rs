//! 经本机转发到 SSH 主机。
//!
//! 手机经中继连到电脑后，看不到电脑上配的其他主机（配置在桌面客户端里，内网服务器手机也直连不到）。
//! 桌面客户端把可共享的主机同步过来（`proxy.setHosts`，只接受本机连接）；设备新开一条中继连接，
//! 第一条消息发 `proxy.connect`，这里连到目标（SSH 主机起 `yi-agentd bridge`，中继主机直接连它的 yi-agentd），
//! 之后原样转发字节。
//! 设备和目标 yi-agentd 之间仍是普通的 JSON-RPC，订阅、重连、补拉都不用改。

use anyhow::{anyhow, bail, Result};
use yi_client::hosts::{HostConfig, HostKind};
use yi_client::{deploy, ssh};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufRead, AsyncRead, AsyncWrite, AsyncWriteExt};

fn hosts_path() -> PathBuf {
    crate::paths::base_dir().join("proxy-hosts.json")
}

fn known_path() -> PathBuf {
    crate::paths::base_dir().join("proxy_known_hosts")
}

fn write_private(p: &PathBuf, data: &str) -> Result<()> {
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(p, data)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// 桌面客户端同步过来的主机（含跳板机）和它信任过的主机指纹（OpenSSH known_hosts 格式）
pub fn set_hosts(hosts: Vec<HostConfig>, known_hosts: &str) -> Result<()> {
    write_private(&hosts_path(), &serde_json::to_string(&hosts)?)?;
    write_private(&known_path(), known_hosts)
}

fn load() -> Vec<HostConfig> {
    std::fs::read_to_string(hosts_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn shared(h: &HostConfig) -> bool {
    h.relay_share
        && match h.kind {
            HostKind::Ssh => h.ssh.is_some(),
            HostKind::Relay => h.pairing.is_some(),
            _ => false,
        }
}

trait Duplex: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Duplex for T {}

/// 给设备看的列表：不带任何凭据
pub fn list() -> Value {
    let list: Vec<Value> = load()
        .iter()
        .filter(|h| shared(h))
        .map(|h| {
            let target = match &h.ssh {
                Some(t) if h.kind == HostKind::Ssh => format!("{}@{}", t.user, t.host),
                _ => "中继".into(),
            };
            json!({"id": h.id, "name": h.name, "kind": h.kind, "target": target})
        })
        .collect();
    json!(list)
}

/// 连上目标主机的 yi-agentd；SSH 时返回的会话要一直持有，断了通道也就断了
async fn open(host_id: &str) -> Result<(Option<ssh::SshSession>, Box<dyn Duplex>)> {
    let hosts = load();
    let h = hosts
        .iter()
        .find(|h| h.id == host_id && shared(h))
        .ok_or_else(|| anyhow!("电脑上没有共享这台主机"))?;
    if h.kind == HostKind::Relay {
        let c = yi_link::Credential::parse(h.pairing.as_deref().unwrap_or(""))?;
        let d = yi_link::client_connect(&c).await?;
        return Ok((None, Box::new(d)));
    }
    let t = h.ssh.clone().unwrap();
    let jump = (!t.jump_host_id.is_empty())
        .then(|| hosts.iter().find(|j| j.id == t.jump_host_id).and_then(|j| j.ssh.clone()))
        .flatten();
    let known = Arc::new(ssh::KnownHosts {
        app_file: known_path(),
        approved: Default::default(),
    });
    let s = ssh::connect(&t, jump.as_ref(), known).await?;
    let p = deploy::detect(&s).await?;
    if !p.yi_agentd_present {
        bail!("{} 上还没有部署 yi-agentd，请先在电脑上连接一次这台主机", h.name);
    }
    let stream = s.exec_stream(&p.command("bridge")).await?;
    Ok((Some(s), Box::new(stream)))
}

/// 处理 proxy.connect：先回一行结果，成功后双向转发直到任一方断开
pub async fn serve<R, W>(mut r: R, mut w: W, id: u64, host_id: &str)
where
    R: AsyncBufRead + AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    crate::info!("proxy -> {host_id}");
    let (sess, stream) = match open(host_id).await {
        Ok(x) => x,
        Err(e) => {
            crate::warn!("proxy {host_id} failed: {e:#}");
            let line = json!({"id": id, "error": {"code": -32000, "message": format!("{e:#}")}});
            let _ = w.write_all(format!("{line}\n").as_bytes()).await;
            let _ = w.flush().await;
            return;
        }
    };
    let ok = json!({"id": id, "result": {"ok": true}});
    if w.write_all(format!("{ok}\n").as_bytes()).await.is_err() || w.flush().await.is_err() {
        return;
    }
    let (mut sr, mut sw) = tokio::io::split(stream);
    tokio::select! {
        _ = tokio::io::copy(&mut r, &mut sw) => {}
        _ = tokio::io::copy(&mut sr, &mut w) => {}
    }
    drop(sess);
    crate::info!("proxy {host_id} closed");
}
