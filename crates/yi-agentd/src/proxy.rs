//! Forwarding to SSH hosts through this machine.
//!
//! A phone connected to a computer through the relay can't see other hosts configured on that computer (they're configured in the desktop client, and phones can't reach internal servers directly).
//! The desktop client syncs shareable hosts here (`proxy.setHosts`, local connections only); the device opens a new relay connection
//! whose first message is `proxy.connect`, and we connect to the target here (SSH hosts start `yi-agentd bridge`, relay hosts connect to their yi-agentd directly),
//! then relay bytes as is.
//! Between the device and the target yi-agentd it's still plain JSON-RPC, so subscriptions, reconnects and catch-ups need no changes.

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

/// Hosts synced from the desktop client (including jump hosts) and the host fingerprints it trusts (OpenSSH known_hosts format)
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

/// List for devices: carries no credentials
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

/// Connect to the target host's yi-agentd; for SSH, the returned session must be held, since the channel dies with it
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

/// Handle proxy.connect: reply with one result line first, then relay both ways until either side disconnects
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
