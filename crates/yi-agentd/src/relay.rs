//! yi-agentd 侧的中继（设计见 `docs/relay-security-v2.md`）：
//! - 保持一条到中继的控制连接（首次凭邀请码登记），把已配对客户端的白名单和配对票据推给中继；
//! - 收到 open 就开数据流做 Noise 握手：配对连接校验一次性配对码后把客户端加进已配对列表，
//!   普通连接只接受已配对的客户端，握手通过后按普通客户端连接处理（serve_conn）；
//! - 吊销客户端时推新白名单，并断开它在本机的连接。

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

/// 配对码有效期
const PAIR_TTL: u64 = 600;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PairedClient {
    pub id: String,
    pub name: String,
    /// Ed25519 签名公钥（中继层身份）
    pub sign_pub: String,
    /// X25519 静态公钥（Noise 握手里认证）
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
    /// 中继证书指纹（自签名证书时必填）
    #[serde(default)]
    pub relay_fp: String,
    #[serde(default)]
    pub name: String,
    /// 这台电脑的身份密钥，见 [`Identity::encode`]
    pub identity: String,
    #[serde(default)]
    pub clients: Vec<PairedClient>,
    /// 还没用掉的邀请码：首次登记成功后清空
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

/// 读改写配置时串行化
static CFG_LOCK: Mutex<()> = Mutex::new(());
static KICK: tokio::sync::Notify = tokio::sync::Notify::const_new();
static ONLINE: AtomicBool = AtomicBool::new(false);
/// 最近一次连中继失败的原因，界面上提示用
static LAST_ERROR: Mutex<String> = Mutex::new(String::new());
/// 待用的配对码和过期时间（只在内存里，重启作废）
static PENDING: Mutex<Vec<([u8; 32], u64)>> = Mutex::new(Vec::new());
/// 当前控制连接的发送端（推白名单、要邀请码）
static CTRL: Mutex<Option<mpsc::UnboundedSender<Value>>> = Mutex::new(None);
static INVITE_WAIT: Mutex<Option<oneshot::Sender<std::result::Result<String, String>>>> = Mutex::new(None);
/// 客户端 id -> 它在本机的连接（吊销时断开）
static CONNS: Mutex<Option<HashMap<String, Vec<watch::Sender<bool>>>>> = Mutex::new(None);

fn path() -> std::path::PathBuf {
    crate::paths::base_dir().join("relay.json")
}

pub fn load_config() -> Option<RelayConfig> {
    let raw = std::fs::read_to_string(path()).ok()?;
    if let Ok(c) = serde_json::from_str::<RelayConfig>(&raw) {
        return Some(c);
    }
    // 旧版配置：只保留地址、指纹和名字，换成新身份；要重新用邀请登记
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

/// 读改写；返回改后的配置
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

/// 设置中继地址；首次会生成这台电脑的身份密钥。
/// `fp` 为 None 时保留原来的指纹，Some("") 表示清空；`invite` 非空时用于首次登记
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

/// 推给中继的白名单：已配对客户端的签名公钥 + 有效的配对票据
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

/// RPC relay.pairing：中继状态
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

/// RPC relay.configure：设置中继地址或开关。`link` 是接入链接（可带邀请码），给了就以它为准
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

/// RPC relay.pairStart：生成一次性配对码，返回配对链接（二维码内容）和过期时间
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

/// RPC relay.clients：已配对的设备
pub fn clients() -> Result<Value> {
    let Some(c) = load_config() else { return Ok(json!([])) };
    let online = online_ids();
    Ok(json!(c
        .clients
        .iter()
        .map(|x| json!({"id": x.id, "name": x.name, "createdAt": x.created_at, "lastSeen": x.last_seen, "online": online.contains(&x.id)}))
        .collect::<Vec<_>>()))
}

/// RPC relay.revoke：移除一台已配对的设备，它的连接立刻断开
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

/// RPC relay.reset：清空所有配对、作废未用的配对码、更换端到端密钥。
/// 签名密钥（设备 id）不变，不用重新到中继登记
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

/// RPC relay.invite：向中继要一个一次性邀请（中继要开启 --device-invites），返回接入链接
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

// ---------------- 连接 ----------------

/// 把连接错误翻成用户看得懂的提示；原始错误附在后面
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

/// 处理一次 open：配对或普通连接
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
                // 配对码只能用一次
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
                // 先推白名单再回握手：客户端收到回复后马上会以已配对身份重连
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

/// 在终端里打印二维码
pub fn print_qr(text: &str) {
    match qrcode::QrCode::new(text.as_bytes()) {
        Ok(code) => {
            let s = code.render::<qrcode::render::unicode::Dense1x2>().quiet_zone(true).build();
            println!("{s}");
        }
        Err(e) => println!("（二维码生成失败：{e}）"),
    }
}
