//! The yi-agentd side of the relay (design in `docs/relay-security-v2.md`):
//! - keep a control connection to the relay (registering with an invite code the first time), and push the allow list of paired clients and pairing tickets to it;
//! - on open, start a stream and do the Noise handshake: pairing connections check the one-time pairing code and then add the client to the paired list,
//!   normal connections only accept paired clients and, once the handshake passes, are handled like any client connection (serve_conn);
//! - when a client is revoked, push the new allow list and drop its connections on this machine.

use crate::hub::Hub;
use anyhow::{anyhow, bail, Result};
use yi_link::{auth, b64, client_id_of, AgentLink, Control, Identity, OpenReq, PairOffer};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, watch};

/// Pairing code lifetime
const PAIR_TTL: u64 = 600;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PairedClient {
    pub id: String,
    pub name: String,
    /// Ed25519 signing public key (relay-layer identity)
    pub sign_pub: String,
    /// X25519 static public key (authenticated in the Noise handshake)
    pub noise_pub: String,
    pub created_at: u64,
    #[serde(default)]
    pub last_seen: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayConfig {
    pub enabled: bool,
    pub url: String,
    /// Relay certificate fingerprint (required for self-signed certificates)
    #[serde(default)]
    pub relay_fp: String,
    #[serde(default)]
    pub name: String,
    /// This computer's identity keys, see [`Identity::encode`]
    pub identity: String,
    #[serde(default)]
    pub clients: Vec<PairedClient>,
    /// Invite code not used yet: cleared after the first successful registration
    #[serde(default)]
    pub invite: String,
}

impl RelayConfig {
    fn identity(&self) -> Result<Identity> {
        Identity::decode(&self.identity).map_err(|_| anyhow!("relay.json 里的身份密钥损坏"))
    }

    pub fn device_id(&self) -> String {
        self.identity().map(|i| yi_link::device_id_of(&i.sign_pub())).unwrap_or_default()
    }

    fn link(&self) -> Result<AgentLink> {
        Ok(AgentLink { relay: self.url.clone(), fp: self.relay_fp.clone(), me: self.identity()? })
    }
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Serializes read-modify-write of the config
static CFG_LOCK: Mutex<()> = Mutex::new(());
static KICK: tokio::sync::Notify = tokio::sync::Notify::const_new();
static ONLINE: AtomicBool = AtomicBool::new(false);
/// Reason for the last failed relay connection, shown in the UI
static LAST_ERROR: Mutex<String> = Mutex::new(String::new());
/// Pending pairing codes and their expiry (memory only, voided on restart)
static PENDING: Mutex<Vec<([u8; 32], u64)>> = Mutex::new(Vec::new());
/// Sender of the current control connection (pushing allow lists, requesting invites)
static CTRL: Mutex<Option<mpsc::UnboundedSender<Value>>> = Mutex::new(None);
static INVITE_WAIT: Mutex<Option<oneshot::Sender<std::result::Result<String, String>>>> = Mutex::new(None);
/// client id -> its connections on this machine (dropped on revocation)
static CONNS: Mutex<Option<HashMap<String, Vec<watch::Sender<bool>>>>> = Mutex::new(None);

fn path() -> std::path::PathBuf {
    crate::paths::base_dir().join("relay.json")
}

pub fn load_config() -> Option<RelayConfig> {
    let raw = std::fs::read_to_string(path()).ok()?;
    if let Ok(c) = serde_json::from_str::<RelayConfig>(&raw) {
        return Some(c);
    }
    // Old-format config: keep only address, fingerprint and name, switch to a new identity; must re-register with an invite
    let v: Value = serde_json::from_str(&raw).ok()?;
    let url = v["url"].as_str()?.to_string();
    Some(RelayConfig {
        enabled: v["enabled"].as_bool().unwrap_or(true),
        url,
        relay_fp: v["relayFp"].as_str().unwrap_or("").to_string(),
        name: v["name"].as_str().unwrap_or("").to_string(),
        identity: Identity::generate().encode(),
        clients: Vec::new(),
        invite: String::new(),
    })
}

fn save_config(c: &RelayConfig) -> Result<()> {
    crate::paths::ensure_base()?;
    std::fs::write(path(), serde_json::to_string_pretty(c)?)?;
    crate::util::restrict_file(&path());
    Ok(())
}

/// Read-modify-write; returns the updated config
fn update<F: FnOnce(&mut RelayConfig) -> Result<()>>(f: F) -> Result<RelayConfig> {
    let _g = CFG_LOCK.lock().unwrap();
    let mut c = load_config().ok_or_else(|| anyhow!("还没设置中继"))?;
    f(&mut c)?;
    save_config(&c)?;
    Ok(c)
}

fn hostname() -> String {
    gethostname::gethostname().to_string_lossy().to_string()
}

/// Set the relay address; the first time, this computer's identity keys are generated.
/// With `fp` None the old fingerprint is kept, Some("") clears it; a non-empty `invite` is used for the first registration
pub fn setup(url: &str, fp: Option<&str>, invite: &str) -> Result<RelayConfig> {
    let url = url.trim().trim_end_matches('/').to_string();
    if !(url.starts_with("ws://") || url.starts_with("wss://")) {
        bail!("中继地址要以 ws:// 或 wss:// 开头");
    }
    let c = {
        let _g = CFG_LOCK.lock().unwrap();
        let mut c = load_config().unwrap_or_else(|| RelayConfig {
            enabled: true,
            url: url.clone(),
            relay_fp: String::new(),
            name: hostname(),
            identity: Identity::generate().encode(),
            clients: Vec::new(),
            invite: String::new(),
        });
        c.url = url;
        c.enabled = true;
        if let Some(fp) = fp {
            c.relay_fp = yi_link::normalize_fp(fp)?;
        }
        if !invite.trim().is_empty() {
            c.invite = invite.trim().to_string();
        }
        save_config(&c)?;
        c
    };
    LAST_ERROR.lock().unwrap().clear();
    KICK.notify_waiters();
    Ok(c)
}

pub fn disable() -> Result<()> {
    if load_config().is_some() {
        update(|c| {
            c.enabled = false;
            Ok(())
        })?;
    }
    KICK.notify_waiters();
    Ok(())
}

/// Allow list pushed to the relay: signing public keys of paired clients + valid pairing tickets
fn acl_msg(c: &RelayConfig) -> Value {
    let t = now();
    let mut pending = PENDING.lock().unwrap();
    pending.retain(|(_, exp)| *exp > t);
    json!({
        "type": "acl",
        "clients": c.clients.iter().map(|x| x.sign_pub.clone()).collect::<Vec<_>>(),
        "tickets": pending.iter().map(|(code, exp)| json!({"hash": b64(&auth::ticket_of(code)), "exp": exp})).collect::<Vec<_>>(),
    })
}

fn push_acl() {
    let Some(c) = load_config() else { return };
    if let Some(tx) = CTRL.lock().unwrap().as_ref() {
        let _ = tx.send(acl_msg(&c));
    }
}

fn kill_client(id: &str) {
    if let Some(list) = CONNS.lock().unwrap().as_mut().and_then(|m| m.remove(id)) {
        for k in list {
            let _ = k.send(true);
        }
    }
}

fn online_ids() -> std::collections::HashSet<String> {
    let mut g = CONNS.lock().unwrap();
    let m = g.get_or_insert_with(HashMap::new);
    m.retain(|_, v| {
        v.retain(|k| !k.is_closed());
        !v.is_empty()
    });
    m.keys().cloned().collect()
}

// ---------------- RPC ----------------

/// RPC relay.pairing: relay status
pub fn pairing_info() -> Result<Value> {
    let Some(c) = load_config() else {
        return Ok(json!({ "configured": false }));
    };
    let device = c.link()?.device();
    let online = ONLINE.load(Ordering::Relaxed);
    let error = if online { String::new() } else { LAST_ERROR.lock().unwrap().clone() };
    Ok(json!({
        "configured": true,
        "enabled": c.enabled,
        "url": c.url,
        "deviceId": device,
        "name": if c.name.is_empty() { hostname() } else { c.name.clone() },
        "fp": c.relay_fp,
        "online": online,
        "error": error,
        "clients": c.clients.len(),
    }))
}

/// RPC relay.configure: set the relay address or toggle it. `link` is an access link (may include an invite code); when given it takes precedence
pub fn configure(url: &str, fp: Option<&str>, invite: Option<&str>, enabled: bool, link: Option<&str>) -> Result<Value> {
    if let Some(link) = link.filter(|l| !l.trim().is_empty()) {
        let a = yi_link::RelayAccess::parse(link)?;
        setup(&a.url, Some(&a.fp), &a.invite)?;
    } else if enabled {
        setup(url, fp, invite.unwrap_or(""))?;
    } else {
        disable()?;
    }
    pairing_info()
}

/// RPC relay.pairStart: generate a one-time pairing code; returns the pairing link (QR code content) and expiry
pub fn pair_start() -> Result<Value> {
    let c = load_config().filter(|c| c.enabled).ok_or_else(|| anyhow!("还没开启中继"))?;
    let me = c.identity()?;
    let code = yi_link::random_bytes::<32>();
    let exp = now() + PAIR_TTL;
    PENDING.lock().unwrap().push((code, exp));
    push_acl();
    let offer = PairOffer {
        relay: c.url.clone(),
        fp: c.relay_fp.clone(),
        device: yi_link::device_id_of(&me.sign_pub()),
        apk: me.noise_pub(),
        code,
        name: if c.name.is_empty() { hostname() } else { c.name.clone() },
    };
    Ok(json!({ "uri": offer.to_uri(), "expiresAt": exp, "online": ONLINE.load(Ordering::Relaxed) }))
}

/// RPC relay.clients: paired devices
pub fn clients() -> Result<Value> {
    let Some(c) = load_config() else { return Ok(json!([])) };
    let online = online_ids();
    Ok(json!(c
        .clients
        .iter()
        .map(|x| json!({"id": x.id, "name": x.name, "createdAt": x.created_at, "lastSeen": x.last_seen, "online": online.contains(&x.id)}))
        .collect::<Vec<_>>()))
}

/// RPC relay.revoke: remove a paired device; its connections drop immediately
pub fn revoke(id: &str) -> Result<Value> {
    let mut found = false;
    update(|c| {
        let n = c.clients.len();
        c.clients.retain(|x| x.id != id);
        found = c.clients.len() != n;
        Ok(())
    })?;
    if !found {
        bail!("没有这台设备：{id}");
    }
    push_acl();
    kill_client(id);
    crate::info!("已移除配对设备 {id}");
    clients()
}

pub fn rename_client(id: &str, name: &str) -> Result<Value> {
    let name = name.trim();
    if name.is_empty() {
        bail!("名称不能为空");
    }
    update(|c| {
        let x = c.clients.iter_mut().find(|x| x.id == id).ok_or_else(|| anyhow!("没有这台设备：{id}"))?;
        x.name = name.to_string();
        Ok(())
    })?;
    clients()
}

/// RPC relay.reset: clear all pairings, void unused pairing codes, rotate the end-to-end key.
/// The signing key (device id) stays the same, so there's no need to re-register with the relay
pub fn reset() -> Result<Value> {
    let mut old = Vec::new();
    update(|c| {
        let mut id = c.identity()?;
        id.noise = yi_link::random_bytes();
        c.identity = id.encode();
        old = c.clients.drain(..).map(|x| x.id).collect();
        Ok(())
    })?;
    PENDING.lock().unwrap().clear();
    push_acl();
    for id in &old {
        kill_client(id);
    }
    crate::info!("已重置中继配对");
    pairing_info()
}

/// RPC relay.invite: ask the relay for a one-time invite (the relay must have --device-invites); returns an access link
pub async fn invite() -> Result<Value> {
    let c = load_config().filter(|c| c.enabled).ok_or_else(|| anyhow!("这台电脑没有开启中继"))?;
    let (tx, rx) = oneshot::channel();
    *INVITE_WAIT.lock().unwrap() = Some(tx);
    {
        let g = CTRL.lock().unwrap();
        let ctrl = g.as_ref().ok_or_else(|| anyhow!("这台电脑还没连上中继"))?;
        ctrl.send(json!({"type": "invite"})).map_err(|_| anyhow!("这台电脑还没连上中继"))?;
    }
    let code = tokio::time::timeout(Duration::from_secs(10), rx)
        .await
        .map_err(|_| anyhow!("中继没有响应（中继版本太旧？）"))?
        .map_err(|_| anyhow!("中继连接断开"))?
        .map_err(|e| anyhow!("{e}"))?;
    let link = yi_link::RelayAccess { url: c.url, fp: c.relay_fp, invite: code }.to_uri();
    Ok(json!({ "link": link }))
}

// ---------------- Connections ----------------

/// Turn connection errors into hints users can understand; the raw error is appended
fn explain(e: &anyhow::Error) -> String {
    let raw = format!("{e:#}");
    let l = raw.to_lowercase();
    let hint = if l.contains("未登记") || l.contains("邀请码") {
        "这台电脑还没在中继上登记，需要中继的邀请链接（在中继服务器上运行 yi-relay invite 生成）"
    } else if l.contains("4410") || l.contains("移除") {
        "这台电脑已被中继运营者移除，需要新的邀请链接"
    } else if l.contains("指纹") || l.contains("certificate") || l.contains("fingerprint") {
        "中继证书和指纹对不上，检查证书指纹"
    } else if l.contains("refused") || l.contains("timed out") || l.contains("timeout") || l.contains("超时") || l.contains("dns") || l.contains("resolve") {
        "连不上中继服务器，检查地址、端口和网络（公司网络需要代理时看「网络代理」）"
    } else if l.contains("版本太旧") {
        "中继服务器版本太旧，需要升级 yi-relay"
    } else {
        "连接中继失败"
    };
    format!("{hint}（{raw}）")
}

/// Handle one open: pairing or normal connection
async fn accept(hub: Arc<Hub>, link: AgentLink, req: OpenReq) {
    let t = now();
    let code = req
        .ticket
        .and_then(|tk| PENDING.lock().unwrap().iter().find(|(c, exp)| *exp > t && auth::ticket_of(c) == tk).map(|(c, _)| *c));
    let pairing = code.is_some();
    let (stop_tx, stop_rx) = watch::channel(false);
    let mut client_id = String::new();
    let res = yi_link::agent_accept(&link, &req, code.as_ref().map(auth::psk_of), Some(stop_rx), |h| {
        if h.sign_pub != req.client {
            bail!("身份与中继报告的不一致");
        }
        let sign = b64(&h.sign_pub);
        let noise = b64(&h.noise_pub);
        client_id = client_id_of(&h.sign_pub);
        match code {
            Some(c) => {
                // A pairing code can only be used once
                {
                    let mut p = PENDING.lock().unwrap();
                    let n = p.len();
                    p.retain(|(x, _)| *x != c);
                    if p.len() == n {
                        bail!("配对二维码已用过，请重新生成");
                    }
                }
                let name = if h.name.trim().is_empty() { "未命名设备".to_string() } else { h.name.trim().to_string() };
                let id = client_id.clone();
                update(|cfg| {
                    cfg.clients.retain(|x| x.id != id);
                    cfg.clients.push(PairedClient { id, name, sign_pub: sign, noise_pub: noise, created_at: now(), last_seen: now() });
                    Ok(())
                })?;
                // Push the allow list before answering the handshake: right after the reply, the client reconnects as a paired client
                push_acl();
                Ok(())
            }
            None => {
                let c = load_config().ok_or_else(|| anyhow!("中继未配置"))?;
                if c.clients.iter().any(|x| x.sign_pub == sign && x.noise_pub == noise) {
                    Ok(())
                } else {
                    bail!("这台电脑已经移除了你的设备，请重新配对")
                }
            }
        }
    })
    .await;
    match res {
        Ok((duplex, hello)) => {
            if pairing {
                crate::info!("已配对新设备 {client_id}（{}）", hello.name);
                drop(duplex);
                return;
            }
            CONNS.lock().unwrap().get_or_insert_with(HashMap::new).entry(client_id.clone()).or_default().push(stop_tx);
            let id = client_id.clone();
            let _ = update(|c| {
                if let Some(x) = c.clients.iter_mut().find(|x| x.id == id) {
                    x.last_seen = now();
                }
                Ok(())
            });
            let (r, w) = tokio::io::split(duplex);
            crate::server::serve_conn(hub, r, w, "relay").await;
        }
        Err(e) => crate::warn!("中继数据流失败: {e:#}"),
    }
}

pub async fn run(hub: Arc<Hub>) {
    let mut backoff = 2u64;
    loop {
        let Some(cfg) = load_config().filter(|c| c.enabled) else {
            KICK.notified().await;
            continue;
        };
        let link = match cfg.link() {
            Ok(l) => l,
            Err(e) => {
                crate::warn!("中继配置无效: {e}");
                *LAST_ERROR.lock().unwrap() = e.to_string();
                KICK.notified().await;
                continue;
            }
        };
        match yi_link::agent_control(&link, &cfg.invite).await {
            Ok(mut ws) => {
                crate::info!("中继已连接 {}（设备 {}）", cfg.url, link.device());
                if !cfg.invite.is_empty() {
                    let _ = update(|c| {
                        c.invite.clear();
                        Ok(())
                    });
                }
                ONLINE.store(true, Ordering::Relaxed);
                LAST_ERROR.lock().unwrap().clear();
                backoff = 2;
                let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
                let _ = tx.send(acl_msg(&cfg));
                *CTRL.lock().unwrap() = Some(tx);
                loop {
                    tokio::select! {
                        _ = KICK.notified() => break,
                        m = rx.recv() => {
                            let Some(m) = m else { break };
                            if let Err(e) = yi_link::send_control(&mut ws, &m).await {
                                crate::warn!("中继连接中断: {e}");
                                break;
                            }
                        }
                        r = tokio::time::timeout(Duration::from_secs(90), yi_link::next_control(&mut ws)) => {
                            match r {
                                Ok(Ok(Control::Open(req))) => {
                                    tokio::spawn(accept(hub.clone(), link.clone(), req));
                                }
                                Ok(Ok(Control::Invite(res))) => {
                                    if let Some(w) = INVITE_WAIT.lock().unwrap().take() {
                                        let _ = w.send(res);
                                    }
                                }
                                Ok(Ok(Control::Other)) => {}
                                Ok(Err(e)) => {
                                    crate::warn!("中继连接中断: {e}");
                                    *LAST_ERROR.lock().unwrap() = explain(&e);
                                    break;
                                }
                                Err(_) => { crate::warn!("中继心跳超时"); break; }
                            }
                        }
                    }
                }
                *CTRL.lock().unwrap() = None;
                ONLINE.store(false, Ordering::Relaxed);
            }
            Err(e) => {
                crate::warn!("连接中继失败: {e:#}");
                *LAST_ERROR.lock().unwrap() = explain(&e);
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(backoff)) => {}
            _ = KICK.notified() => {}
        }
        backoff = (backoff * 2).min(60);
    }
}

/// Print a QR code in the terminal
pub fn print_qr(text: &str) {
    match qrcode::QrCode::new(text.as_bytes()) {
        Ok(code) => {
            let s = code.render::<qrcode::render::unicode::Dense1x2>().quiet_zone(true).build();
            println!("{s}");
        }
        Err(e) => println!("（二维码生成失败：{e}）"),
    }
}
