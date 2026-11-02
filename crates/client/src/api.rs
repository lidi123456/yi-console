//! ClientHub：前端唯一的入口。Tauri 命令和 Web 模式的 WebSocket 都转到 `call`，
//! 事件经 `Sink` 推给前端。
//!
//! 推给前端的事件：
//! - `{"type":"host.status","hostId","status":"connecting|connected|disconnected|error","error"?,"code"?,"hello"?,"versionMismatch"?}`
//! - `{"type":"deploy","hostId","message"}` 部署进度
//! - `{"type":"notification","hostId","method","params"}` yi-agentd 的通知原样转发；
//!   重连后补发的积压以 method = "session.backlog" 推送（params 同 session.subscribe 的返回值 + sessionId）

use crate::deploy;
use crate::forward::{self, Forward};
use crate::hosts::{HostConfig, HostKind, HostStore};
use crate::rpc::RpcConn;
use crate::settings::ClientSettings;
use crate::ssh::{self, KnownHosts, SshSession};
use anyhow::{anyhow, bail, Result};
use yi_proto::{Notification, DAEMON_VERSION};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::{oneshot, Notify};

pub type Sink = Arc<dyn Fn(Value) + Send + Sync>;

type BoxR = Box<dyn AsyncRead + Unpin + Send>;
type BoxW = Box<dyn AsyncWrite + Unpin + Send>;

#[derive(Default)]
struct HostConn {
    rpc: Mutex<Option<Arc<RpcConn>>>,
    status: Mutex<Value>,
    subs: Mutex<HashMap<String, i64>>,
    stop: AtomicBool,
    running: AtomicBool,
    kick: Notify,
    ssh: Mutex<Option<Arc<SshSession>>>,
    child: Mutex<Option<tokio::process::Child>>,
}

pub struct ClientHub {
    yi_agentd_dir: Option<PathBuf>,
    data_dir: PathBuf,
    settings: Mutex<ClientSettings>,
    sink: Sink,
    store: Mutex<HostStore>,
    conns: Mutex<HashMap<String, Arc<HostConn>>>,
    known: Arc<KnownHosts>,
    /// 手机等没有本机 yi-agentd 的平台
    local_supported: bool,
    /// 正在升级的主机：重连后版本对上了才算完成
    upgrading: Mutex<HashSet<String>>,
    /// 这次启动里已经自动升级过的主机，升级后仍不一致就不再重试，免得来回重启
    auto_tried: Mutex<HashSet<String>>,
    /// 端口转发，键是 `<主机 id>/<主机上的端口>`
    forwards: Mutex<HashMap<String, Arc<Forward>>>,
    /// 浏览器标签预览 HTML 用的文件服务（见 preview.rs）
    previews: crate::preview::Previews,
}

/// 不重试的错误（需要用户处理）
fn fatal_code(e: &str) -> Option<&'static str> {
    // 中继或电脑明确拒绝了这台设备（被移除、重置过配对、身份不符）：重连也没用，不要一直重试
    if ["（4403）", "（4410）", "请重新配对", "设备身份校验失败", "还没完成配对", "旧版本"].iter().any(|k| e.contains(k)) {
        return Some("PAIRING_INVALID");
    }
    [ssh::ERR_HOSTKEY_UNKNOWN, ssh::ERR_HOSTKEY_CHANGED, ssh::ERR_AUTH, ssh::ERR_KEY, "NO_YI_AGENTD_BINARY"]
        .into_iter()
        .find(|c| e.starts_with(c))
}

impl ClientHub {
    pub fn new(
        data_dir: PathBuf,
        yi_agentd_dir: Option<PathBuf>,
        local_supported: bool,
        sink: Sink,
    ) -> Arc<ClientHub> {
        let _ = std::fs::create_dir_all(&data_dir);
        let settings = ClientSettings::load(&data_dir);
        if let Err(e) = settings.apply() {
            eprintln!("客户端设置无效，已忽略: {e:#}");
        }
        let mut store = HostStore::load(&data_dir);
        if local_supported && !store.hosts.iter().any(|h| h.kind == HostKind::Local) {
            store.upsert(HostConfig {
                id: "local".into(),
                name: "本机".into(),
                kind: HostKind::Local,
                ssh: None,
                pairing: None,
                auto_connect: true,
                install_service: false,
                remote_os: None,
                remote_arch: None,
                relay_share: false,
                via_host_id: None,
                target_id: None,
            });
            let _ = store.save();
        }
        let hub = Arc::new(ClientHub {
            yi_agentd_dir,
            data_dir: data_dir.clone(),
            settings: Mutex::new(settings),
            sink,
            store: Mutex::new(store),
            conns: Mutex::new(HashMap::new()),
            known: Arc::new(KnownHosts {
                app_file: data_dir.join("known_hosts"),
                approved: Mutex::new(HashMap::new()),
            }),
            local_supported,
            upgrading: Mutex::new(HashSet::new()),
            auto_tried: Mutex::new(HashSet::new()),
            forwards: Mutex::new(HashMap::new()),
            previews: Default::default(),
        });
        let auto: Vec<String> = hub
            .store
            .lock()
            .unwrap()
            .hosts
            .iter()
            .filter(|h| h.auto_connect)
            .map(|h| h.id.clone())
            .collect();
        for id in auto {
            hub.clone().ensure_loop(&id, None);
        }
        hub
    }

    fn emit(&self, v: Value) {
        (self.sink)(v);
    }

    fn conn(&self, id: &str) -> Arc<HostConn> {
        self.conns
            .lock()
            .unwrap()
            .entry(id.to_string())
            .or_default()
            .clone()
    }

    fn set_status(&self, id: &str, v: Value) {
        let mut v = v;
        v["type"] = json!("host.status");
        v["hostId"] = json!(id);
        *self.conn(id).status.lock().unwrap() = v.clone();
        self.emit(v);
    }

    fn host(&self, id: &str) -> Result<HostConfig> {
        self.store
            .lock()
            .unwrap()
            .get(id)
            .ok_or_else(|| anyhow!("主机不存在: {id}"))
    }

    /// 已经和设备 `device` 配对过的中继主机，连同它的配对凭据；有几条时优先正在编辑的 `prefer`
    fn paired_with(&self, device: &str, prefer: &str) -> Option<(HostConfig, yi_link::Credential)> {
        let st = self.store.lock().unwrap();
        let mut found: Vec<(HostConfig, yi_link::Credential)> = st
            .hosts
            .iter()
            .filter(|h| h.kind == HostKind::Relay)
            .filter_map(|h| {
                let c = yi_link::Credential::parse(h.pairing.as_deref()?).ok()?;
                (c.device == device).then(|| (h.clone(), c))
            })
            .collect();
        found.sort_by_key(|(h, _)| h.id != prefer);
        found.into_iter().next()
    }

    pub async fn call(self: &Arc<Self>, cmd: &str, args: Value) -> Result<Value> {
        let id = || -> Result<String> {
            args["id"]
                .as_str()
                .or(args["hostId"].as_str())
                .map(str::to_string)
                .ok_or_else(|| anyhow!("缺少主机 id"))
        };
        Ok(match cmd {
            "hosts.list" => {
                let hosts = self.store.lock().unwrap().hosts.clone();
                let list: Vec<Value> = hosts
                    .into_iter()
                    .map(|h| {
                        let mut status = self.conn(&h.id).status.lock().unwrap().clone();
                        if status.is_null() {
                            status = json!({ "status": "disconnected" });
                        }
                        json!({ "config": redact(&h), "status": status })
                    })
                    .collect();
                json!({ "hosts": list, "localSupported": self.local_supported })
            }
            "hosts.save" => {
                let mut h: HostConfig = serde_json::from_value(args["host"].clone())?;
                // 前端拿到的是脱敏后的配置，没改密码就沿用旧值
                if let Ok(old) = self.host(&h.id) {
                    if let (Some(new), Some(old)) = (h.ssh.as_mut(), old.ssh.as_ref()) {
                        for (n, o) in [
                            (&mut new.password, &old.password),
                            (&mut new.passphrase, &old.passphrase),
                            (&mut new.key_text, &old.key_text),
                        ] {
                            if n == REDACTED {
                                *n = o.clone();
                            }
                        }
                    }
                    if h.pairing.as_deref() == Some(REDACTED) {
                        h.pairing = old.pairing.clone();
                    }
                }
                if h.kind == HostKind::Relay {
                    let uri = h.pairing.clone().unwrap_or_default();
                    // 新粘贴 / 扫到的配对链接：先完成配对，保存的是配对凭据（含这台设备自己的私钥）
                    let cred = if uri.trim().starts_with("yiconsole://pair?") {
                        let offer = yi_link::PairOffer::parse(&uri)?;
                        let me = args["clientName"].as_str().map(str::to_string).filter(|s| !s.trim().is_empty()).unwrap_or_else(device_name);
                        // 和这台电脑配对过：沿用原来的身份和主机条目，重新配对等于更新，两边都不会多出一条
                        let prev = self.paired_with(&offer.device, &h.id);
                        let cred = yi_link::client_pair(&offer, &me, prev.as_ref().map(|(_, c)| c.me.clone())).await?;
                        if let Some((old, _)) = prev {
                            h.id = old.id;
                            if h.name.trim().is_empty() {
                                h.name = old.name;
                            }
                            h.remote_os = h.remote_os.or(old.remote_os);
                            h.remote_arch = h.remote_arch.or(old.remote_arch);
                        }
                        cred
                    } else {
                        yi_link::Credential::parse(&uri)?
                    };
                    if h.name.trim().is_empty() {
                        h.name = cred.name.clone();
                    }
                    h.pairing = Some(cred.to_uri());
                }
                let h = {
                    let mut st = self.store.lock().unwrap();
                    let h = st.upsert(h);
                    st.save()?;
                    h
                };
                tokio::spawn(self.clone().sync_proxy_hosts());
                json!(redact(&h))
            }
            "hosts.delete" => {
                let id = id()?;
                let via: Vec<String> = self
                    .store
                    .lock()
                    .unwrap()
                    .hosts
                    .iter()
                    .filter(|h| h.via_host_id.as_deref() == Some(id.as_str()))
                    .map(|h| h.id.clone())
                    .collect();
                for x in via.iter().chain(std::iter::once(&id)) {
                    self.disconnect(x).await;
                    self.conns.lock().unwrap().remove(x);
                }
                {
                    let mut st = self.store.lock().unwrap();
                    for x in via.iter().chain(std::iter::once(&id)) {
                        st.remove(x);
                    }
                    st.save()?;
                }
                self.forwards
                    .lock()
                    .unwrap()
                    .retain(|_, f| !via.iter().chain(std::iter::once(&id)).any(|x| *x == f.info.lock().unwrap().host_id));
                tokio::spawn(self.clone().sync_proxy_hosts());
                Value::Null
            }
            // 识别用户粘贴的链接，给界面做预览；不返回密钥和令牌
            "relay.parseLink" => {
                let uri = args["uri"].as_str().unwrap_or("").trim();
                if uri.starts_with("yiconsole://pair?") {
                    let p = yi_link::PairOffer::parse(uri)?;
                    json!({ "kind": "pair", "name": p.name, "device": p.device, "url": p.relay, "pinned": !p.fp.is_empty() })
                } else {
                    let a = yi_link::RelayAccess::parse(uri)?;
                    json!({ "kind": "relay", "url": a.url, "pinned": !a.fp.is_empty(), "hasInvite": !a.invite.is_empty() })
                }
            }
            // 已保存的中继设备用到的中继服务器（去重），给"本机开启中继"一键复用
            "relay.known" => {
                let hosts = self.store.lock().unwrap().hosts.clone();
                let mut seen = std::collections::HashSet::new();
                let list: Vec<Value> = hosts
                    .iter()
                    .filter(|h| h.kind == HostKind::Relay)
                    .filter_map(|h| Some((h, yi_link::Credential::parse(h.pairing.as_deref()?).ok()?)))
                    .filter(|(_, p)| seen.insert((p.relay.clone(), p.fp.clone())))
                    .map(|(h, p)| json!({ "hostId": h.id, "name": h.name, "url": p.relay }))
                    .collect();
                json!(list)
            }
            // 给目标主机开启和某台已保存的中继设备同一个中继：请那台电脑向中继要一个一次性邀请
            // （中继要开启 --device-invites），邀请链接只在这里转一手，不经过界面
            "relay.reuse" => {
                let target = id()?;
                let from = args["fromHostId"].as_str().ok_or_else(|| anyhow!("缺少 fromHostId"))?;
                let src = self.host(from)?;
                let src_rpc = self.conn(from).rpc.lock().unwrap().clone();
                let src_rpc = src_rpc.ok_or_else(|| anyhow!("{} 没有连接，先连上它再试", src.name))?;
                let inv = src_rpc.call("relay.invite", json!({})).await?;
                let link = inv["link"].as_str().ok_or_else(|| anyhow!("{} 的 yi-agentd 版本太旧", src.name))?.to_string();
                let rpc = self.conn(&target).rpc.lock().unwrap().clone();
                let rpc = rpc.ok_or_else(|| anyhow!("主机未连接"))?;
                rpc.call("relay.configure", json!({ "link": link, "enabled": true })).await?
            }
            // ---------- 端口转发 ----------
            // {id, port, localPort?, label?} -> ForwardInfo；已经在转发就直接返回
            "ports.forward" => {
                let id = id()?;
                let port = args["port"].as_u64().filter(|p| (1..=65535).contains(p)).ok_or_else(|| anyhow!("端口无效"))? as u16;
                let key = format!("{id}/{port}");
                if let Some(f) = self.forwards.lock().unwrap().get(&key) {
                    return Ok(json!(f.snapshot()));
                }
                let h = self.host(&id)?;
                let label = args["label"].as_str().filter(|s| !s.is_empty()).map(str::to_string);
                let f = if h.kind == HostKind::Local {
                    Arc::new(forward::direct(&id, port, label))
                } else {
                    let me = Arc::downgrade(self);
                    let hid = id.clone();
                    let open: forward::Opener = Arc::new(move || {
                        let me = me.clone();
                        let hid = hid.clone();
                        Box::pin(async move {
                            let me = me.upgrade().ok_or_else(|| anyhow!("客户端已关闭"))?;
                            me.open_port(&hid, port).await
                        })
                    });
                    let local = args["localPort"].as_u64().map(|p| p as u16);
                    forward::start(&id, port, local, label, open).await?
                };
                let snap = f.snapshot();
                self.forwards.lock().unwrap().insert(key, f);
                self.emit(json!({"type": "ports.changed"}));
                json!(snap)
            }
            "ports.unforward" => {
                let id = id()?;
                let port = args["port"].as_u64().unwrap_or(0);
                self.forwards.lock().unwrap().remove(&format!("{id}/{port}"));
                self.emit(json!({"type": "ports.changed"}));
                Value::Null
            }
            // {id?} -> [ForwardInfo]：不带 id 列出全部
            "ports.forwards" => {
                let only = args["id"].as_str().or(args["hostId"].as_str()).map(str::to_string);
                let mut v: Vec<_> = self
                    .forwards
                    .lock()
                    .unwrap()
                    .values()
                    .map(|f| f.snapshot())
                    .filter(|f| only.as_deref().is_none_or(|h| h == f.host_id))
                    .collect();
                v.sort_by(|a, b| (&a.host_id, a.port).cmp(&(&b.host_id, b.port)));
                json!(v)
            }
            // {id, root} -> {token}：给浏览器标签预览这个目录里的网页
            "preview.root" => {
                let id = id()?;
                let root = args["root"].as_str().filter(|s| !s.is_empty()).ok_or_else(|| anyhow!("缺少 root"))?;
                json!({ "token": self.previews.token(&id, root) })
            }
            // 用系统默认浏览器打开网址（只接受 http / https）
            "url.open" => {
                let url = args["url"].as_str().unwrap_or("");
                if !(url.starts_with("http://") || url.starts_with("https://")) {
                    bail!("只能打开 http / https 地址");
                }
                open_url(url)?;
                Value::Null
            }
            "hosts.importSshConfig" => json!(crate::sshconfig::load_default()),
            "hosts.connect" => {
                let id = id()?;
                let (tx, rx) = oneshot::channel();
                let c = self.conn(&id);
                c.stop.store(false, Ordering::SeqCst);
                if c.running.load(Ordering::SeqCst) {
                    c.kick.notify_waiters();
                    return Ok(c.status.lock().unwrap().clone());
                }
                self.clone().ensure_loop(&id, Some(tx));
                match rx.await {
                    Ok(Ok(v)) => v,
                    Ok(Err(e)) => return Err(anyhow!(e)),
                    Err(_) => return Err(anyhow!("连接已取消")),
                }
            }
            "hosts.disconnect" => {
                self.disconnect(&id()?).await;
                Value::Null
            }
            "hosts.trustKey" => {
                let id = id()?;
                let h = self.host(&id)?;
                let fp = args["fingerprint"].as_str().unwrap_or("").to_string();
                let t = match (h.ssh.as_ref(), args["jump"].as_bool().unwrap_or(false)) {
                    (Some(s), true) => self
                        .host(&s.jump_host_id)?
                        .ssh
                        .ok_or_else(|| anyhow!("跳板机配置无效"))?,
                    (Some(s), false) => s.clone(),
                    _ => bail!("不是 SSH 主机"),
                };
                self.known
                    .approved
                    .lock()
                    .unwrap()
                    .insert(format!("{}:{}", t.host, t.port), fp);
                Value::Null
            }
            "hosts.installService" => {
                let id = id()?;
                let h = self.host(&id)?;
                match h.kind {
                    HostKind::Ssh => {
                        let s = self.ssh_session(&h).await?;
                        let p = deploy::detect(&s).await?;
                        json!(deploy::install_service(&s, &p).await?)
                    }
                    HostKind::Local => {
                        let exe = self.local_yi_agentd()?;
                        let mut cmd = tokio::process::Command::new(exe);
                        cmd.arg("install");
                        #[cfg(windows)]
                        cmd.creation_flags(0x0800_0000);
                        let out = cmd.output().await?;
                        json!(format!(
                            "{}{}",
                            String::from_utf8_lossy(&out.stdout),
                            String::from_utf8_lossy(&out.stderr)
                        ))
                    }
                    HostKind::Relay | HostKind::Proxy => bail!("这台主机请在它所在的电脑上执行 yi-agentd install"),
                }
            }
            "hosts.upgrade" => {
                self.upgrade(&id()?).await?;
                Value::Null
            }
            "client.getSettings" => {
                let s = self.settings.lock().unwrap().clone();
                json!({ "settings": s, "detectedProxy": yi_link::proxy::describe() })
            }
            "client.setSettings" => {
                // 只改传进来的部分（比如只传 proxy），其余沿用现有设置
                let mut cur = serde_json::to_value(self.settings.lock().unwrap().clone())?;
                if let (Some(cur), Some(new)) = (cur.as_object_mut(), args["settings"].as_object()) {
                    for (k, v) in new {
                        cur.insert(k.clone(), v.clone());
                    }
                }
                let s: ClientSettings = serde_json::from_value(cur)?;
                s.apply()?;
                s.save(&self.data_dir)?;
                *self.settings.lock().unwrap() = s.clone();
                // 只影响之后新建的连接；已连上的不打断
                json!({ "settings": s, "detectedProxy": yi_link::proxy::describe() })
            }
            "pairing.parse" => {
                let p = yi_link::PairOffer::parse(args["uri"].as_str().unwrap_or(""))?;
                json!({ "relay": p.relay, "device": p.device, "name": p.name, "pinned": !p.fp.is_empty() })
            }
            "rpc" => {
                let id = id()?;
                let method = args["method"].as_str().ok_or_else(|| anyhow!("缺少 method"))?;
                let params = args["params"].clone();
                self.rpc(&id, method, params).await?
            }
            _ => bail!("未知命令: {cmd}"),
        })
    }

    async fn rpc(&self, id: &str, method: &str, params: Value) -> Result<Value> {
        let c = self.conn(id);
        let rpc = c
            .rpc
            .lock()
            .unwrap()
            .clone()
            .filter(|r| !r.is_closed())
            .ok_or_else(|| anyhow!("主机未连接"))?;
        let sid = params["sessionId"].as_str().map(str::to_string);
        let r = rpc.call(method, params).await?;
        match (method, sid) {
            ("session.subscribe", Some(sid)) => {
                let max = r["events"]
                    .as_array()
                    .and_then(|a| a.iter().filter_map(|e| e["seq"].as_i64()).max())
                    .unwrap_or(0);
                let mut subs = c.subs.lock().unwrap();
                let e = subs.entry(sid).or_insert(0);
                *e = (*e).max(max);
            }
            ("session.unsubscribe", Some(sid)) => {
                c.subs.lock().unwrap().remove(&sid);
            }
            _ => {}
        }
        Ok(r)
    }

    async fn disconnect(&self, id: &str) {
        let c = self.conn(id);
        c.stop.store(true, Ordering::SeqCst);
        c.kick.notify_waiters();
        c.rpc.lock().unwrap().take();
        c.ssh.lock().unwrap().take();
        let child = c.child.lock().unwrap().take();
        if let Some(mut ch) = child {
            let _ = ch.start_kill();
        }
    }

    fn ensure_loop(self: Arc<Self>, id: &str, first: Option<oneshot::Sender<Result<Value, String>>>) {
        let c = self.conn(id);
        if c.running.swap(true, Ordering::SeqCst) {
            if let Some(tx) = first {
                let _ = tx.send(Ok(c.status.lock().unwrap().clone()));
            }
            return;
        }
        let id = id.to_string();
        tokio::spawn(async move {
            let mut first = first;
            let mut backoff = 1u64;
            loop {
                if c.stop.load(Ordering::SeqCst) {
                    break;
                }
                self.set_status(&id, json!({"status":"connecting"}));
                match self.attempt(&id, &c).await {
                    Ok((rpc, status)) => {
                        backoff = 1;
                        if let Some(tx) = first.take() {
                            let _ = tx.send(Ok(status));
                        }
                        tokio::select! {
                            _ = rpc.wait_closed() => {}
                            _ = c.kick.notified() => {}
                        }
                        c.rpc.lock().unwrap().take();
                        c.ssh.lock().unwrap().take();
                        if c.stop.load(Ordering::SeqCst) {
                            break;
                        }
                        self.set_status(&id, json!({"status":"disconnected","error":"连接中断，正在重连"}));
                    }
                    Err(e) => {
                        let msg = format!("{e:#}");
                        let code = fatal_code(&msg);
                        self.set_status(&id, json!({"status":"error","error":msg,"code":code}));
                        if let Some(tx) = first.take() {
                            let _ = tx.send(Err(msg.clone()));
                        }
                        if code.is_some() {
                            break;
                        }
                    }
                }
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(backoff)) => {}
                    _ = c.kick.notified() => {}
                }
                backoff = (backoff * 2).min(30);
            }
            c.running.store(false, Ordering::SeqCst);
            if c.stop.load(Ordering::SeqCst) {
                self.set_status(&id, json!({"status":"disconnected"}));
            }
        });
    }

    async fn attempt(self: &Arc<Self>, id: &str, c: &Arc<HostConn>) -> Result<(Arc<RpcConn>, Value)> {
        let h = self.host(id)?;
        let (r, w) = self.open_transport(&h, c).await?;
        let me = self.clone();
        let hid = id.to_string();
        let conn = c.clone();
        let rpc = RpcConn::start(
            r,
            w,
            Arc::new(move |n: Notification| me.on_notify(&hid, &conn, n)),
        );
        let hello = rpc.call("hello", json!({"client": "yi-console"})).await?;
        *c.rpc.lock().unwrap() = Some(rpc.clone());
        // 版本号相同也可能是两次不同的构建（拷源码构建都是 +dev，未提交的改动提交号也不变），所以连构建时间一起比
        let mismatch = hello["daemonVersion"].as_str() != Some(DAEMON_VERSION)
            || hello["buildTime"].as_u64().unwrap_or(0) != yi_proto::build_time();
        match h.kind {
            HostKind::Local => {
                tokio::spawn(self.clone().sync_proxy_hosts());
            }
            HostKind::Relay => {
                tokio::spawn(self.clone().discover_proxies(id.to_string()));
            }
            _ => {}
        }
        if self.upgrading.lock().unwrap().remove(id) {
            if mismatch {
                self.upgrade_event(id, "error", "升级后版本仍不一致，可以在主机页手动重试", None);
            } else {
                self.upgrade_event(id, "done", "升级完成", None);
            }
        } else if mismatch && self.should_auto_upgrade(&h, &hello) && self.auto_tried.lock().unwrap().insert(id.to_string()) {
            tokio::spawn(self.clone().auto_upgrade(id.to_string()));
        }
        let status = json!({
            "status": "connected",
            "hello": hello,
            "versionMismatch": mismatch,
            "clientVersion": DAEMON_VERSION,
        });
        self.set_status(id, status.clone());
        self.resubscribe(id, c, &rpc).await;
        Ok((rpc, status))
    }

    /// 把可共享的主机（及其跳板机）和信任过的主机指纹同步给本机 yi-agentd，供已配对的设备经本机转发
    async fn sync_proxy_hosts(self: Arc<Self>) {
        let Some(local) = self.store.lock().unwrap().hosts.iter().find(|h| h.kind == HostKind::Local).map(|h| h.id.clone()) else {
            return;
        };
        if self.conn(&local).rpc.lock().unwrap().is_none() {
            return;
        }
        let hosts = self.store.lock().unwrap().hosts.clone();
        let shared: Vec<&HostConfig> = hosts
            .iter()
            .filter(|h| h.relay_share && matches!(h.kind, HostKind::Ssh | HostKind::Relay))
            .collect();
        let jumps: HashSet<&str> = shared
            .iter()
            .filter_map(|h| h.ssh.as_ref().map(|t| t.jump_host_id.as_str()))
            .filter(|j| !j.is_empty())
            .collect();
        let send: Vec<HostConfig> = hosts
            .iter()
            .filter(|h| shared.iter().any(|s| s.id == h.id) || jumps.contains(h.id.as_str()))
            .cloned()
            .collect();
        let known = std::fs::read_to_string(&self.known.app_file).unwrap_or_default();
        let _ = self
            .rpc(&local, yi_proto::methods::PROXY_SET_HOSTS, json!({"hosts": send, "knownHosts": known}))
            .await;
    }

    /// 中继主机（电脑）共享了哪些主机：同步成本机的转发主机，id 为 `<中继主机 id>~<那边的主机 id>`
    async fn discover_proxies(self: Arc<Self>, via: String) {
        // 旧版 yi-agentd 没有 proxy.list，不支持转发
        let Ok(list) = self.rpc(&via, yi_proto::methods::PROXY_LIST, json!({})).await else {
            return;
        };
        let via_name = self.host(&via).map(|h| h.name).unwrap_or_default();
        let items: Vec<(String, String)> = list
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|x| Some((x["id"].as_str()?.to_string(), x["name"].as_str().unwrap_or("").to_string())))
            .collect();
        let (added, stale, changed) = {
            let mut st = self.store.lock().unwrap();
            let wanted: HashSet<String> = items.iter().map(|(t, _)| format!("{via}~{t}")).collect();
            let stale: Vec<String> = st
                .hosts
                .iter()
                .filter(|h| h.kind == HostKind::Proxy && h.via_host_id.as_deref() == Some(&via) && !wanted.contains(&h.id))
                .map(|h| h.id.clone())
                .collect();
            let mut added = vec![];
            let mut changed = !stale.is_empty();
            for id in &stale {
                st.remove(id);
            }
            for (tid, name) in &items {
                let id = format!("{via}~{tid}");
                let name = format!("{name}（经 {via_name}）");
                match st.get(&id) {
                    Some(old) if old.name == name => {}
                    Some(mut old) => {
                        old.name = name;
                        st.upsert(old);
                        changed = true;
                    }
                    None => {
                        st.upsert(HostConfig {
                            id: id.clone(),
                            name,
                            kind: HostKind::Proxy,
                            ssh: None,
                            pairing: None,
                            auto_connect: true,
                            install_service: false,
                            remote_os: None,
                            remote_arch: None,
                            relay_share: false,
                            via_host_id: Some(via.clone()),
                            target_id: Some(tid.clone()),
                        });
                        added.push(id);
                        changed = true;
                    }
                }
            }
            if changed {
                let _ = st.save();
            }
            (added, stale, changed)
        };
        for id in &stale {
            self.disconnect(id).await;
            self.conns.lock().unwrap().remove(id);
        }
        for id in &added {
            self.clone().ensure_loop(id, None);
        }
        if changed {
            self.emit(json!({"type": "hosts.changed"}));
        }
    }

    /// 升级进度：phase 为 waiting（等回合结束）/ uploading / restarting / done / error
    fn upgrade_event(&self, id: &str, phase: &str, message: &str, progress: Option<f64>) {
        self.emit(json!({"type":"upgrade","hostId":id,"phase":phase,"message":message,"progress":progress}));
    }

    /// 本机：二进制就是安装包自带的，版本不一致就该重启。
    /// SSH：只在客户端比守护进程新时升级，避免两台不同版本的电脑来回覆盖。中继传不了文件，只提示
    fn should_auto_upgrade(&self, h: &HostConfig, hello: &Value) -> bool {
        match h.kind {
            HostKind::Local => true,
            HostKind::Ssh => hello["buildTime"].as_u64().unwrap_or(0) < yi_proto::build_time(),
            HostKind::Relay | HostKind::Proxy => false,
        }
    }

    /// 自动升级：有回合在跑就等它们结束（期间照常使用），再升级
    async fn auto_upgrade(self: Arc<Self>, id: String) {
        loop {
            // 期间用户点了"立即升级"、或者别的客户端已经换好了：版本对上就不用再动
            let st = self.conn(&id).status.lock().unwrap().clone();
            if st["status"] != "connected" || st["versionMismatch"] != json!(true) {
                return;
            }
            let busy = match self.rpc(&id, "session.list", json!({})).await {
                Ok(v) => v
                    .as_array()
                    .map(|a| a.iter().filter(|s| matches!(s["status"].as_str(), Some("running" | "waiting-permission"))).count())
                    .unwrap_or(0),
                // 连接断了：重连后会重新判断
                Err(_) => return,
            };
            if self.upgrading.lock().unwrap().contains(&id) {
                return;
            }
            if busy == 0 {
                break;
            }
            self.upgrade_event(&id, "waiting", &format!("有 {busy} 个会话正在进行，结束后自动升级 yi-agentd"), None);
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
        if let Err(e) = self.upgrade(&id).await {
            self.upgrade_event(&id, "error", &format!("升级失败：{e:#}"), None);
        }
    }

    /// 换上新的 yi-agentd 并让旧进程退出；bridge / 系统服务会用新二进制重新拉起，重连后在 attempt 里确认版本
    async fn upgrade(self: &Arc<Self>, id: &str) -> Result<()> {
        let h = self.host(id)?;
        if matches!(h.kind, HostKind::Relay | HostKind::Proxy) {
            bail!("中继连接的主机没法远程更新程序，请在那台机器上更新 Yi Console");
        }
        if !self.upgrading.lock().unwrap().insert(id.to_string()) {
            return Ok(());
        }
        let r = async {
            if h.kind == HostKind::Local {
                // 连接时已同步过，这里再确认一次
                self.upgrade_event(id, "uploading", "正在准备新版 yi-agentd…", Some(1.0));
                self.local_yi_agentd()?;
            }
            if h.kind == HostKind::Ssh {
                self.upgrade_event(id, "uploading", "正在检测远程平台…", Some(0.0));
                let s = self.ssh_session(&h).await?;
                let p = deploy::detect(&s).await?;
                let bin = deploy::local_binary(self.yi_agentd_dir.as_deref(), &p.os, &p.arch)
                    .ok_or_else(|| anyhow!("客户端没有附带 yi-agentd-{}-{}", p.os, p.arch))?;
                let me = self.clone();
                let hid = id.to_string();
                let last = std::sync::atomic::AtomicU64::new(0);
                deploy::upload(&s, &p, &bin, move |sent, total| {
                    // 每 5% 报一次，免得刷屏
                    let pct = sent * 100 / total.max(1);
                    if pct >= last.load(Ordering::Relaxed) + 5 || sent == total {
                        last.store(pct, Ordering::Relaxed);
                        me.upgrade_event(&hid, "uploading",
                            &format!("正在上传新版 yi-agentd（{:.1} / {:.1} MB）", sent as f64 / 1048576.0, total as f64 / 1048576.0),
                            Some(sent as f64 / total.max(1) as f64));
                    }
                })
                .await?;
            }
            self.upgrade_event(id, "restarting", "正在重启守护进程…", None);
            let rpc = self.conn(id).rpc.lock().unwrap().clone();
            if let Some(rpc) = rpc {
                let _ = tokio::time::timeout(Duration::from_secs(3), rpc.call("daemon.shutdown", json!({}))).await;
            }
            anyhow::Ok(())
        }
        .await;
        if r.is_err() {
            self.upgrading.lock().unwrap().remove(id);
        } else {
            // 重连超时兜底：1 分钟还没接上就报错，别让界面一直挡着
            let me = self.clone();
            let hid = id.to_string();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(60)).await;
                if me.upgrading.lock().unwrap().remove(&hid) {
                    me.upgrade_event(&hid, "error", "守护进程 1 分钟内没有重新连上，请检查主机状态", None);
                }
            });
        }
        r
    }

    /// 重连后按各会话已收到的最大 seq 补拉事件
    async fn resubscribe(&self, id: &str, c: &HostConn, rpc: &RpcConn) {
        let subs: Vec<(String, i64)> = c
            .subs
            .lock()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect();
        for (sid, since) in subs {
            match rpc
                .call("session.subscribe", json!({"sessionId": sid, "sinceSeq": since}))
                .await
            {
                Ok(mut r) => {
                    if let Some(max) = r["events"]
                        .as_array()
                        .and_then(|a| a.iter().filter_map(|e| e["seq"].as_i64()).max())
                    {
                        c.subs.lock().unwrap().insert(sid.clone(), max.max(since));
                    }
                    r["sessionId"] = json!(sid);
                    self.emit(json!({"type":"notification","hostId":id,
                        "method":"session.backlog","params":r}));
                }
                Err(_) => {
                    c.subs.lock().unwrap().remove(&sid);
                }
            }
        }
    }

    fn on_notify(self: &Arc<Self>, id: &str, c: &Arc<HostConn>, n: Notification) {
        if n.method == "session.event" {
            if let (Some(sid), Some(seq)) = (
                n.params["sessionId"].as_str(),
                n.params["event"]["seq"].as_i64(),
            ) {
                if let Some(v) = c.subs.lock().unwrap().get_mut(sid) {
                    *v = (*v).max(seq);
                }
            }
        }
        if n.method == "resync" {
            let me = self.clone();
            let c = c.clone();
            let id = id.to_string();
            tokio::spawn(async move {
                let rpc = c.rpc.lock().unwrap().clone();
                if let Some(rpc) = rpc {
                    me.resubscribe(&id, &c, &rpc).await;
                }
            });
            return;
        }
        self.emit(json!({"type":"notification","hostId":id,"method":n.method,"params":n.params}));
    }

    /// 本机守护进程用的 yi-agentd：先把安装包自带的同步到 `~/.yi-console/bin`，再用那份
    fn local_yi_agentd(&self) -> Result<PathBuf> {
        let bundled = self.bundled_yi_agentd()?;
        crate::localbin::sync(&bundled).or_else(|e| {
            // 主目录不可写之类的情况，退回直接用自带的
            eprintln!("同步本机 yi-agentd 失败，直接使用安装包里的: {e:#}");
            Ok(bundled)
        })
    }

    fn bundled_yi_agentd(&self) -> Result<PathBuf> {
        let exe = if cfg!(windows) { "yi-agentd.exe" } else { "yi-agentd" };
        let os = match std::env::consts::OS {
            "macos" => "macos",
            o => o,
        };
        let mut cands = vec![];
        if let Ok(p) = std::env::var("YI_CONSOLE_AGENTD") {
            cands.push(PathBuf::from(p));
        }
        if let Some(d) = &self.yi_agentd_dir {
            cands.push(d.join(exe));
        }
        // Tauri sidecar 放在主程序旁边
        if let Ok(me) = std::env::current_exe() {
            if let Some(d) = me.parent() {
                cands.push(d.join(exe));
            }
        }
        if let Some(p) = deploy::local_binary(self.yi_agentd_dir.as_deref(), os, std::env::consts::ARCH) {
            cands.push(p);
        }
        cands
            .into_iter()
            .find(|p| p.is_file())
            .ok_or_else(|| anyhow!("找不到本机 yi-agentd（可设置环境变量 YI_CONSOLE_AGENTD）"))
    }

    async fn ssh_session(&self, h: &HostConfig) -> Result<Arc<SshSession>> {
        let t = h.ssh.clone().ok_or_else(|| anyhow!("缺少 SSH 配置"))?;
        let jump = if t.jump_host_id.is_empty() {
            None
        } else {
            Some(
                self.host(&t.jump_host_id)?
                    .ssh
                    .ok_or_else(|| anyhow!("跳板机不是 SSH 主机"))?,
            )
        };
        Ok(Arc::new(ssh::connect(&t, jump.as_ref(), self.known.clone()).await?))
    }

    /// 预览文件：`<令牌>/<相对路径>` -> (内容, MIME)。Tauri 的 ahfs 协议和 yi-web 的 /fsv/ 都走这里
    pub async fn serve_preview(&self, url_path: &str) -> Result<(Vec<u8>, &'static str)> {
        use base64::Engine;
        let (host, root, path) = self.previews.resolve(url_path)?;
        let r = self.rpc(&host, "fs.read", json!({ "path": path, "raw": true, "root": root })).await?;
        let b64 = r["base64"].as_str().ok_or_else(|| anyhow!("读取失败"))?;
        let data = base64::engine::general_purpose::STANDARD.decode(b64)?;
        Ok((data, crate::preview::mime(&path)))
    }

    /// 端口转发的一条连接：接到主机上的 127.0.0.1:port
    async fn open_port(&self, id: &str, port: u16) -> Result<(BoxR, BoxW)> {
        let h = self.host(id)?;
        let req = json!({"id": 0, "method": yi_proto::methods::PORT_CONNECT, "params": {"port": port}});
        match h.kind {
            HostKind::Local => {
                let s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await?;
                let (r, w) = s.into_split();
                Ok((Box::new(r), Box::new(w)))
            }
            HostKind::Ssh => {
                // 复用会话的 SSH 连接；没连上就先连主机
                let s = self.conn(id).ssh.lock().unwrap().clone().filter(|s| !s.is_closed());
                let s = match s {
                    Some(s) => s,
                    None => bail!("{} 未连接", h.name),
                };
                match s.direct_tcpip(port).await {
                    Ok(st) => {
                        let (r, w) = tokio::io::split(st);
                        Ok((Box::new(r), Box::new(w)))
                    }
                    // sshd 不允许 TCP 转发时，经 yi-agentd bridge 转
                    Err(_) => {
                        let p = deploy::detect(&s).await?;
                        let st = s.exec_stream(&p.command("bridge")).await?;
                        let (r, w) = tokio::io::split(st);
                        handshake(Box::new(r), Box::new(w), &req, &h.name).await
                    }
                }
            }
            HostKind::Relay | HostKind::Proxy => {
                // 新开一条到 yi-agentd 的连接（经电脑转发的主机先过 proxy.connect），第一条消息说要连哪个端口
                let c = HostConn::default();
                let (r, w) = self.open_transport(&h, &c).await?;
                handshake(r, w, &req, &h.name).await
            }
        }
    }

    async fn open_transport(&self, h: &HostConfig, c: &HostConn) -> Result<(BoxR, BoxW)> {
        match h.kind {
            HostKind::Local => {
                if !self.local_supported {
                    bail!("当前设备不支持本机会话");
                }
                let exe = self.local_yi_agentd()?;
                let mut cmd = tokio::process::Command::new(exe);
                // --local：告诉守护进程这是本机客户端（SSH 远程起的 bridge 不带，只限本机的方法不让调）
                cmd.args(["bridge", "--local"])
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::null())
                    .kill_on_drop(true);
                #[cfg(windows)]
                cmd.creation_flags(0x0800_0000);
                let mut child = cmd.spawn()?;
                let r = child.stdout.take().unwrap();
                let w = child.stdin.take().unwrap();
                *c.child.lock().unwrap() = Some(child);
                Ok((Box::new(r), Box::new(w)))
            }
            HostKind::Proxy => {
                // 经那台电脑的 yi-agentd 转发：新开一条中继连接，第一条消息说要连哪台，之后就是目标 yi-agentd 本身
                let via = self.host(h.via_host_id.as_deref().unwrap_or(""))?;
                let c = yi_link::Credential::parse(via.pairing.as_deref().unwrap_or(""))?;
                let d = yi_link::client_connect(&c).await?;
                let (r, mut w) = tokio::io::split(d);
                let req = json!({"id": 0, "method": yi_proto::methods::PROXY_CONNECT,
                    "params": {"hostId": h.target_id}});
                w.write_all(format!("{req}\n").as_bytes()).await?;
                w.flush().await?;
                let mut br = tokio::io::BufReader::new(r);
                let mut line = String::new();
                tokio::time::timeout(Duration::from_secs(60), br.read_line(&mut line))
                    .await
                    .map_err(|_| anyhow!("{} 连接目标主机超时", via.name))??;
                let v: Value = serde_json::from_str(line.trim()).map_err(|_| anyhow!("{} 的 yi-agentd 版本太旧，不支持转发", via.name))?;
                if let Some(m) = v["error"]["message"].as_str() {
                    bail!("{m}");
                }
                Ok((Box::new(br), Box::new(w)))
            }
            HostKind::Relay => {
                let c = yi_link::Credential::parse(h.pairing.as_deref().unwrap_or(""))?;
                let d = yi_link::client_connect(&c).await?;
                let (r, w) = tokio::io::split(d);
                Ok((Box::new(r), Box::new(w)))
            }
            HostKind::Ssh => {
                let s = self.ssh_session(h).await?;
                let p = deploy::detect(&s).await?;
                if !p.yi_agentd_present {
                    let bin = deploy::local_binary(self.yi_agentd_dir.as_deref(), &p.os, &p.arch)
                        .ok_or_else(|| {
                            anyhow!(
                                "NO_YI_AGENTD_BINARY:远程是 {}-{}，但客户端没有附带对应的 yi-agentd，请手动安装到 ~/.yi-console/bin/",
                                p.os,
                                p.arch
                            )
                        })?;
                    self.emit(json!({"type":"deploy","hostId":h.id,
                        "message":format!("首次连接，正在上传 yi-agentd（{}-{}）…", p.os, p.arch)}));
                    deploy::upload(&s, &p, &bin, |_, _| {}).await?;
                    if h.install_service {
                        self.emit(json!({"type":"deploy","hostId":h.id,"message":"正在注册自启服务…"}));
                        match deploy::install_service(&s, &p).await {
                            Ok(m) => self.emit(json!({"type":"deploy","hostId":h.id,"message":m})),
                            Err(e) => self.emit(json!({"type":"deploy","hostId":h.id,
                                "message":format!("注册服务失败（不影响使用，守护进程会按需启动）：{e}")})),
                        }
                    }
                }
                {
                    let mut st = self.store.lock().unwrap();
                    if let Some(x) = st.hosts.iter_mut().find(|x| x.id == h.id) {
                        if x.remote_os.as_deref() != Some(&p.os) {
                            x.remote_os = Some(p.os.clone());
                            x.remote_arch = Some(p.arch.clone());
                            let _ = st.save();
                        }
                    }
                }
                let stream = s.exec_stream(&p.command("bridge")).await?;
                *c.ssh.lock().unwrap() = Some(s);
                let (r, w) = tokio::io::split(stream);
                Ok((Box::new(r), Box::new(w)))
            }
        }
    }
}

/// 发第一条消息，读一行回复；成功后这条连接就是透传
async fn handshake(r: BoxR, mut w: BoxW, req: &Value, name: &str) -> Result<(BoxR, BoxW)> {
    w.write_all(format!("{req}\n").as_bytes()).await?;
    w.flush().await?;
    let mut br = tokio::io::BufReader::new(r);
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(30), br.read_line(&mut line))
        .await
        .map_err(|_| anyhow!("{name} 端口转发超时"))??;
    let v: Value = serde_json::from_str(line.trim()).map_err(|_| anyhow!("{name} 上的 yi-agentd 版本太旧，不支持端口转发，请先升级"))?;
    if let Some(m) = v["error"]["message"].as_str() {
        bail!("{m}");
    }
    if v["result"].is_null() {
        bail!("{name} 上的 yi-agentd 版本太旧，不支持端口转发，请先升级");
    }
    Ok((Box::new(br), w))
}

/// 用系统默认程序打开网址
fn open_url(url: &str) -> Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // 不经 cmd：网址里的 & 会被 cmd 当成命令分隔符
        std::process::Command::new("rundll32").args(["url.dll,FileProtocolHandler", url]).creation_flags(0x0800_0000).spawn()?;
    }
    #[cfg(target_os = "macos")]
    std::process::Command::new("open").arg(url).spawn()?;
    #[cfg(all(unix, not(target_os = "macos"), not(target_os = "android")))]
    std::process::Command::new("xdg-open").arg(url).spawn()?;
    #[cfg(target_os = "android")]
    bail!("手机上请用前端打开链接: {url}");
    #[allow(unreachable_code)]
    Ok(())
}

const REDACTED: &str = "********";

/// 发给前端的配置不带明文密码/私钥/配对密钥
fn redact(h: &HostConfig) -> HostConfig {
    let mut h = h.clone();
    if let Some(s) = h.ssh.as_mut() {
        for f in [&mut s.password, &mut s.passphrase, &mut s.key_text] {
            if !f.is_empty() {
                *f = REDACTED.into();
            }
        }
    }
    if h.pairing.is_some() {
        h.pairing = Some(REDACTED.into());
    }
    h
}

/// 配对时报给电脑的本机名字，显示在电脑的「已配对设备」里
fn device_name() -> String {
    if cfg!(target_os = "android") {
        return "Android 手机".into();
    }
    let h = gethostname::gethostname().to_string_lossy().to_string();
    if h.is_empty() || h == "localhost" { "未命名设备".into() } else { h }
}
