//! ClientHub: the frontend's single entry point. Tauri commands and the web mode's WebSocket all go to `call`,
//! and events are pushed to the frontend through `Sink`.
//!
//! Events pushed to the frontend:
//! - `{"type":"host.status","hostId","status":"connecting|connected|disconnected|error","error"?,"code"?,"hello"?,"versionMismatch"?}`
//! - `{"type":"deploy","hostId","message"}` deployment progress
//! - `{"type":"notification","hostId","method","params"}` yi-agentd notifications forwarded as is;
//!   the backlog replayed after a reconnect is pushed with method = "session.backlog" (params = the session.subscribe result + sessionId)

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
    bulk: tokio::sync::Mutex<Bulk>,
}

/// SSH hosts: a second `yi-agentd bridge` channel on the same SSH connection for large responses.
/// Each SSH channel has its own flow-control window, so a multi-MB backlog on this one doesn't hold up
/// requests and live events on the main channel. Opened on first use; reset whenever the transport is reopened
#[derive(Default)]
struct Bulk {
    /// Command that starts the bridge on the host; only set for SSH transports
    cmd: Option<String>,
    /// Local host uses direct local IPC sockets/pipes for bulk requests once connected
    local: bool,
    rpc: Option<Arc<RpcConn>>,
}

/// Read-only methods whose replies can be large; they go over `Bulk` when there is one
const BULK_METHODS: &[&str] = &[
    "session.history",
    "session.userMessages",
    "history.read",
    "history.list",
    "fs.read",
    "fs.grep",
    "nav.references",
    "git.changes",
    "git.show",
    "git.blob",
    "git.branchDiff",
    "git.refDiff",
    "git.reviewScope",
];

/// The local host's id (the daemon on this machine)
const LOCAL_ID: &str = "local";

pub struct ClientHub {
    yi_agentd_dir: Option<PathBuf>,
    data_dir: PathBuf,
    settings: Mutex<ClientSettings>,
    sink: Sink,
    store: Mutex<HostStore>,
    conns: Mutex<HashMap<String, Arc<HostConn>>>,
    known: Arc<KnownHosts>,
    /// Platforms without a local yi-agentd, such as phones
    local_supported: bool,
    /// Hosts being upgraded: only done once the version matches after reconnecting
    upgrading: Mutex<HashSet<String>>,
    /// Hosts already auto-upgraded during this run; if they still mismatch afterwards, don't retry, to avoid restart loops
    auto_tried: Mutex<HashSet<String>>,
    /// Port forwards, keyed by `<host id>/<port on the host>`
    forwards: Mutex<HashMap<String, Arc<Forward>>>,
    /// File server for previewing HTML in browser tabs (see preview.rs)
    previews: crate::preview::Previews,
    /// Local services of the desktop app (Bots), see ext.rs
    ext: std::sync::OnceLock<Arc<dyn crate::ext::Extension>>,
}

/// Errors not worth retrying (they need the user to act)
fn fatal_code(e: &str) -> Option<&'static str> {
    // The relay or the computer explicitly rejected this device (removed, pairings reset, identity mismatch): reconnecting won't help, so don't keep retrying
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
            ext: std::sync::OnceLock::new(),
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

    /// Push an event to the frontend (extensions use it for `bots.changed` and the like). The local daemon passes it
    /// on to phones and yi-web controlling this app (`client.event`)
    pub fn emit_event(&self, v: Value) {
        let rpc = self.conns.lock().unwrap().get(LOCAL_ID).and_then(|c| c.rpc.lock().unwrap().clone());
        if let (Some(rpc), true, Ok(rt)) = (rpc, self.ext.get().is_some(), tokio::runtime::Handle::try_current()) {
            let ev = v.clone();
            rt.spawn(async move {
                let _ = rpc.call("client.event", json!({"event": ev})).await;
            });
        }
        self.emit(v);
    }

    /// Plug in the desktop app's local services. Hosts already connected learn this client's identity at once
    pub fn set_extension(self: &Arc<Self>, ext: Arc<dyn crate::ext::Extension>) {
        if self.ext.set(ext).is_err() {
            return;
        }
        let rpcs: Vec<(String, Arc<RpcConn>)> = self
            .conns
            .lock()
            .unwrap()
            .iter()
            .filter_map(|(id, c)| c.rpc.lock().unwrap().clone().map(|r| (id.clone(), r)))
            .collect();
        for (_, rpc) in rpcs {
            let me = self.clone();
            tokio::spawn(async move { me.client_hello(&rpc).await });
        }
    }

    pub fn extension(&self) -> Option<Arc<dyn crate::ext::Extension>> {
        self.ext.get().cloned()
    }

    /// Tell a host who we are, so it can relay client MCP calls here. Older daemons don't know the method
    async fn client_hello(&self, rpc: &RpcConn) {
        if let Some(ext) = self.ext.get() {
            let me = ext.identity();
            let _ = rpc
                .call("client.hello", json!({"clientId": me.client_id, "kind": me.kind, "name": me.name}))
                .await;
        }
    }

    /// Whether a host is connected right now
    pub fn connected(&self, id: &str) -> bool {
        self.conns
            .lock()
            .unwrap()
            .get(id)
            .and_then(|c| c.rpc.lock().unwrap().clone())
            .is_some_and(|r| !r.is_closed())
    }

    /// Host id -> display name
    pub fn host_names(&self) -> HashMap<String, String> {
        self.store.lock().unwrap().hosts.iter().map(|h| (h.id.clone(), h.name.clone())).collect()
    }

    /// Call a method on a host (same as the frontend's `rpc` command)
    pub async fn host_rpc(&self, id: &str, method: &str, params: Value) -> Result<Value> {
        self.rpc(id, method, params).await
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
        if let Some(ext) = self.ext.get() {
            ext.host_status(id, &v);
        }
        self.emit(v);
    }

    fn host(&self, id: &str) -> Result<HostConfig> {
        self.store
            .lock()
            .unwrap()
            .get(id)
            .ok_or_else(|| anyhow!("主机不存在: {id}"))
    }

    /// Relay hosts already paired with device `device`, together with their pairing credentials; if there are several, prefer `prefer`, the one being edited
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
                // The frontend gets a redacted config; if the password wasn't changed, keep the old value
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
                    // A freshly pasted / scanned pairing link: complete pairing first; what's saved is the pairing credential (including this device's own private key)
                    let cred = if uri.trim().starts_with("yiconsole://pair?") {
                        let offer = yi_link::PairOffer::parse(&uri)?;
                        let me = args["clientName"].as_str().map(str::to_string).filter(|s| !s.trim().is_empty()).unwrap_or_else(device_name);
                        // Already paired with this computer: keep the existing identity and host entry; re-pairing is an update, so neither side gets a duplicate
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
            // Recognize a link pasted by the user and preview it in the UI; keys and tokens are never returned
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
            // Relay servers used by saved relay devices (deduplicated), for one-click reuse in "enable relay on this machine"
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
            // Enable the same relay on the target host as a saved relay device: ask that computer to get a one-time invite from the relay
            // (the relay must have --device-invites); the invite link only passes through here and never reaches the UI
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
            // ---------- Port forwarding ----------
            // {id, port, localPort?, label?} -> ForwardInfo; returned directly if already forwarding
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
            // {id?} -> [ForwardInfo]: lists all without id
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
            // {id, root} -> {token}: lets browser tabs preview web pages in this directory
            "preview.root" => {
                let id = id()?;
                let root = args["root"].as_str().filter(|s| !s.is_empty()).ok_or_else(|| anyhow!("缺少 root"))?;
                json!({ "token": self.previews.token(&id, root) })
            }
            // {id, path, kind?} -> {token, path, size?, modified?, mimeType?}: issues a streaming preview token for a file
            "preview.file" => {
                let id = id()?;
                let raw_path = args["path"].as_str().filter(|s| !s.is_empty()).ok_or_else(|| anyhow!("缺少 path"))?;
                let kind = args["kind"].as_str().unwrap_or("");
                let h = self.host(&id)?;
                let max_bytes = if kind == "image" { 10 * 1024 * 1024 } else { 50 * 1024 * 1024 };
                if h.kind == HostKind::Local {
                    let p = crate::localipc::expand_path(raw_path);
                    let strict_image = kind == "image";
                    let (resolved, size, modified, identity) = tokio::task::spawn_blocking(move || -> Result<(String, u64, i64, crate::preview::FileIdentity)> {
                        let file = open_local_preview_file(&p)?;
                        let meta = file.metadata()?;
                        if !meta.is_file() {
                            bail!("不是有效的文件");
                        }
                        let size = meta.len();
                        if size > max_bytes {
                            bail!(if strict_image { "图片超过 10MB，无法预览" } else { "文件超过 50MB，太大了" });
                        }
                        let identity = local_file_identity(&file, &meta)?;
                        let modified = meta
                            .modified()
                            .ok()
                            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                            .map(|d| d.as_millis() as i64)
                            .unwrap_or(0);
                        Ok((p.display().to_string(), size, modified, identity))
                    })
                    .await??;
                    let token = self.previews.file_token(&id, &resolved, Some(identity), max_bytes);
                    json!({ "token": token, "path": resolved, "size": size, "modified": modified })
                } else if kind == "image" {
                    let r = self.rpc(&id, "fs.read", json!({ "path": raw_path, "image": true, "probe": true })).await?;
                    if r["tooBig"].as_bool() == Some(true) {
                        bail!("图片超过 10MB，无法预览");
                    }
                    let resolved = r["path"].as_str().unwrap_or(raw_path);
                    let token = self.previews.file_token(&id, resolved, None, max_bytes);
                    json!({ "token": token, "path": resolved, "size": r["size"], "mimeType": r["mimeType"] })
                } else {
                    let token = self.previews.file_token(&id, raw_path, None, max_bytes);
                    json!({ "token": token, "path": raw_path })
                }
            }
            // Open a URL in the system default browser (http / https only)
            "url.open" => {
                let url = args["url"].as_str().unwrap_or("").to_string();
                if !(url.starts_with("http://") || url.starts_with("https://")) {
                    bail!("只能打开 http / https 地址");
                }
                tokio::task::spawn_blocking(move || open_url(&url)).await??;
                Value::Null
            }
            "hosts.importSshConfig" => json!(tokio::task::spawn_blocking(crate::sshconfig::load_default).await?),
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
                        let exe = self.local_yi_agentd_async().await?;
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
                // Only change the parts passed in (e.g. just proxy); keep the rest of the current settings
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
                // Only affects connections made later; existing ones aren't interrupted
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
            _ => match self.ext.get().filter(|e| e.handles(cmd)).cloned() {
                Some(ext) => ext.call(cmd, args).await?,
                None => bail!("未知命令: {cmd}"),
            },
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
        let r = match method {
            "session.subscribe" => self.subscribe(&c, &rpc, params).await?,
            m if BULK_METHODS.contains(&m) => match self.bulk_rpc(&c).await {
                Some(b) => match b.call(method, params.clone()).await {
                    // The bulk channel dropped on its own: the main channel still works
                    Err(_) if b.is_closed() => rpc.call(method, params).await?,
                    r => r?,
                },
                None => rpc.call(method, params).await?,
            },
            _ => rpc.call(method, params).await?,
        };
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
            // Keep the cached host status in step, so a later host.status replay doesn't bring back the old tool list.
            // No event here: a connected host.status makes the frontend refetch sessions; the caller updates its own state
            ("tools.detect", _) => {
                let mut st = c.status.lock().unwrap();
                if st["hello"].is_object() {
                    st["hello"]["tools"] = r.clone();
                }
            }
            _ => {}
        }
        Ok(r)
    }

    /// The bulk RPC channel (direct local IPC on Local hosts, extra SSH exec stream on SSH hosts), opened on first use.
    async fn bulk_rpc(&self, c: &HostConn) -> Option<Arc<RpcConn>> {
        let mut b = c.bulk.lock().await;
        if let Some(r) = b.rpc.as_ref().filter(|r| !r.is_closed()) {
            return Some(r.clone());
        }
        if let Some(stale) = b.rpc.take() {
            stale.close();
        }
        if b.local {
            let stream = tokio::time::timeout(Duration::from_secs(5), crate::localipc::connect())
                .await
                .ok()?
                .ok()?;
            let (r, w) = tokio::io::split(stream);
            let rpc = RpcConn::start(r, w, Arc::new(|_| {}));
            b.rpc = Some(rpc.clone());
            return Some(rpc);
        }
        let cmd = b.cmd.clone()?;
        let s = c.ssh.lock().unwrap().clone().filter(|s| !s.is_closed())?;
        let stream = tokio::time::timeout(Duration::from_secs(15), s.exec_stream(&cmd)).await.ok()?.ok()?;
        let (r, w) = tokio::io::split(stream);
        // Notifications are already delivered on the main channel, and nothing subscribes here
        let rpc = RpcConn::start(r, w, Arc::new(|_| {}));
        b.rpc = Some(rpc.clone());
        Some(rpc)
    }

    /// session.subscribe. With a bulk channel, the subscription is made on the main channel with only the newest event
    /// (registers it and fixes the cursor), and the backlog window up to that event is fetched over the bulk channel
    /// with session.history. Later events arrive as notifications on the main channel, so nothing falls in between
    async fn subscribe(&self, c: &HostConn, rpc: &RpcConn, mut params: Value) -> Result<Value> {
        let Some(bulk) = self.bulk_rpc(c).await else {
            return rpc.call("session.subscribe", params).await;
        };
        let since = params["sinceSeq"].as_i64().unwrap_or(0);
        let tail = params["tail"].as_u64().unwrap_or(200);
        params["tail"] = json!(1);
        let mut r = rpc.call("session.subscribe", params.clone()).await?;
        // A window (rather than the events after sinceSeq) comes back for sinceSeq 0, or when the daemon reports a reset; older daemons have no `reset`
        let window = since == 0 || r["reset"].as_bool() == Some(true);
        let last = r["events"].as_array().and_then(|a| a.last()).and_then(|e| e["seq"].as_i64());
        if let (true, true, Some(last)) = (window, r["hasMore"].as_bool() == Some(true), last) {
            let hp = json!({"sessionId": params["sessionId"], "beforeSeq": last + 1, "limit": tail});
            let mut h = match bulk.call("session.history", hp.clone()).await {
                Err(_) if bulk.is_closed() => rpc.call("session.history", hp).await?,
                h => h?,
            };
            r["events"] = h["events"].take();
            r["hasMore"] = h["hasMore"].take();
        }
        Ok(r)
    }

    async fn disconnect(&self, id: &str) {
        let c = self.conn(id);
        c.stop.store(true, Ordering::SeqCst);
        c.kick.notify_waiters();
        if let Some(rpc) = c.rpc.lock().unwrap().take() {
            rpc.close();
        }
        {
            let mut b = c.bulk.lock().await;
            if let Some(brpc) = b.rpc.take() {
                brpc.close();
            }
            *b = Bulk::default();
        }
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
                        rpc.close();
                        if let Some(old) = c.rpc.lock().unwrap().take() {
                            old.close();
                        }
                        {
                            let mut b = c.bulk.lock().await;
                            if let Some(brpc) = b.rpc.take() {
                                brpc.close();
                            }
                            *b = Bulk::default();
                        }
                        c.ssh.lock().unwrap().take();
                        if c.stop.load(Ordering::SeqCst) {
                            break;
                        }
                        self.set_status(&id, json!({"status":"disconnected","error":"连接中断，正在重连"}));
                    }
                    Err(e) => {
                        {
                            let mut b = c.bulk.lock().await;
                            if let Some(brpc) = b.rpc.take() {
                                brpc.close();
                            }
                            *b = Bulk::default();
                        }
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
        let hello = match rpc.call("hello", json!({"client": "yi-console"})).await {
            Ok(v) => v,
            Err(e) => {
                rpc.close();
                return Err(e);
            }
        };
        if let Some(old) = c.rpc.lock().unwrap().replace(rpc.clone()) {
            old.close();
        }
        self.client_hello(&rpc).await;
        // The same version string can still be two different builds (builds from copied sources are all +dev, and uncommitted changes keep the commit hash), so compare the build time too
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

    /// Sync shareable hosts (and their jump hosts) and trusted host fingerprints to the local yi-agentd, so paired devices can be forwarded through this machine
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
        let known = tokio::fs::read_to_string(&self.known.app_file).await.unwrap_or_default();
        let _ = self
            .rpc(&local, yi_proto::methods::PROXY_SET_HOSTS, json!({"hosts": send, "knownHosts": known}))
            .await;
    }

    /// Which hosts a relay host (computer) shares: synced as local forwarded hosts with id `<relay host id>~<host id over there>`
    async fn discover_proxies(self: Arc<Self>, via: String) {
        // Older yi-agentd has no proxy.list and doesn't support forwarding
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

    /// Upgrade progress: phase is waiting (for turns to end) / uploading / restarting / done / error
    fn upgrade_event(&self, id: &str, phase: &str, message: &str, progress: Option<f64>) {
        self.emit(json!({"type":"upgrade","hostId":id,"phase":phase,"message":message,"progress":progress}));
    }

    /// Local: the binary is the one shipped in the installer, so a version mismatch means restart.
    /// SSH: only upgrade when the client is newer than the daemon, so two computers on different versions don't keep overwriting each other. Relays can't transfer files, so just notify
    fn should_auto_upgrade(&self, h: &HostConfig, hello: &Value) -> bool {
        match h.kind {
            HostKind::Local => true,
            HostKind::Ssh => hello["buildTime"].as_u64().unwrap_or(0) < yi_proto::build_time(),
            HostKind::Relay | HostKind::Proxy => false,
        }
    }

    /// Auto-upgrade: if turns are running, wait for them to finish (usable as normal meanwhile), then upgrade
    async fn auto_upgrade(self: Arc<Self>, id: String) {
        loop {
            // Meanwhile the user clicked "Upgrade now", or another client already swapped it: nothing to do once versions match
            let st = self.conn(&id).status.lock().unwrap().clone();
            if st["status"] != "connected" || st["versionMismatch"] != json!(true) {
                return;
            }
            let busy = match self.rpc(&id, "session.list", json!({})).await {
                Ok(v) => v
                    .as_array()
                    .map(|a| a.iter().filter(|s| matches!(s["status"].as_str(), Some("running" | "waiting-permission"))).count())
                    .unwrap_or(0),
                // Connection lost: it will be re-evaluated after reconnecting
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

    /// Swap in the new yi-agentd and make the old process exit; bridge / the system service restart it with the new binary, and attempt confirms the version after reconnecting
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
                // Already synced on connect; confirm once more here
                self.upgrade_event(id, "uploading", "正在准备新版 yi-agentd…", Some(1.0));
                self.local_yi_agentd_async().await?;
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
                    // Report every 5% to avoid flooding
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
            // Reconnect timeout fallback: report an error if not reconnected within a minute, so the UI isn't blocked forever
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

    /// After reconnecting, catch up events from the largest seq received for each session
    async fn resubscribe(&self, id: &str, c: &HostConn, rpc: &RpcConn) {
        let subs: Vec<(String, i64)> = c
            .subs
            .lock()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect();
        for (sid, since) in subs {
            match self
                .subscribe(c, rpc, json!({"sessionId": sid, "sinceSeq": since}))
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
        if n.method == "client.call" {
            // Remote control from a phone or yi-web, relayed by the local daemon only
            let me = self.clone();
            let c = c.clone();
            let local = id == LOCAL_ID;
            tokio::spawn(async move {
                let call_id = n.params["callId"].clone();
                let cmd = n.params["cmd"].as_str().unwrap_or("").to_string();
                let r = match me.ext.get().cloned().filter(|e| local && e.handles(&cmd)) {
                    Some(ext) => ext.call(&cmd, n.params["args"].clone()).await,
                    None => Err(anyhow!("桌面端不能执行这个命令：{cmd}")),
                };
                let reply = match r {
                    Ok(v) => json!({"callId": call_id, "result": v}),
                    Err(e) => json!({"callId": call_id, "error": format!("{e:#}")}),
                };
                let rpc = c.rpc.lock().unwrap().clone();
                if let Some(rpc) = rpc {
                    let _ = rpc.call("client.result", reply).await;
                }
            });
            return;
        }
        if n.method == "clientMcp.call" {
            // A tool call for this client's own MCP servers (the Bot tools): answered here, never shown to the frontend
            let me = self.clone();
            let c = c.clone();
            let id = id.to_string();
            tokio::spawn(async move {
                let call_id = n.params["callId"].clone();
                let r = match me.ext.get().cloned() {
                    Some(ext) => ext.client_mcp(&id, n.params).await,
                    None => Err(anyhow!("这个客户端没有提供 MCP 工具")),
                };
                let reply = match r {
                    Ok(v) => json!({"callId": call_id, "result": v}),
                    Err(e) => json!({"callId": call_id, "error": format!("{e:#}")}),
                };
                let rpc = c.rpc.lock().unwrap().clone();
                if let Some(rpc) = rpc {
                    let _ = rpc.call("clientMcp.result", reply).await;
                }
            });
            return;
        }
        if let Some(ext) = self.ext.get() {
            ext.observe(id, &n.method, &n.params);
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

    /// The yi-agentd used by the local daemon: first sync the one bundled in the installer to `~/.yi-console/bin`, then use that copy
    async fn local_yi_agentd_async(&self) -> Result<PathBuf> {
        let dir = self.yi_agentd_dir.clone();
        tokio::task::spawn_blocking(move || local_yi_agentd_sync(dir.as_deref())).await?
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

    /// Preview files: `<token>/<relative path>` -> (content, MIME). Both Tauri's ahfs protocol and yi-web's /fsv/ go through here
    pub async fn serve_preview(&self, url_path: &str) -> Result<(Vec<u8>, &'static str)> {
        use base64::Engine;
        let resolved = self.previews.resolve(url_path)?;
        if self.host(&resolved.host).is_ok_and(|h| h.kind == HostKind::Local) {
            if resolved.is_file_token && resolved.identity.is_none() {
                self.previews.invalidate_file_token(&resolved.token);
                bail!("预览地址已失效");
            }
            let path_c = resolved.path.clone();
            let root_c = resolved.root.clone();
            let expected_id = resolved.identity;
            let max_bytes = resolved.max_bytes.unwrap_or(50 * 1024 * 1024);
            let res = tokio::task::spawn_blocking(move || {
                read_local_preview(&path_c, root_c.as_deref(), expected_id, max_bytes)
            })
            .await?;
            let data = match res {
                Ok(d) => d,
                Err(LocalPreviewErr::IdentityChanged) => {
                    if resolved.is_file_token {
                        self.previews.invalidate_file_token(&resolved.token);
                    }
                    bail!("预览地址已失效");
                }
                Err(LocalPreviewErr::Other(e)) => return Err(e),
            };
            return Ok((data, crate::preview::mime(&resolved.path)));
        }
        let r = self
            .rpc(
                &resolved.host,
                "fs.read",
                json!({ "path": resolved.path, "raw": true, "root": resolved.root }),
            )
            .await?;
        let b64 = r["base64"].as_str().ok_or_else(|| anyhow!("读取失败"))?;
        let data = base64::engine::general_purpose::STANDARD.decode(b64)?;
        Ok((data, crate::preview::mime(&resolved.path)))
    }

    /// One port-forward connection: connects to 127.0.0.1:port on the host
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
                // Reuse the session's SSH connection; connect to the host first if not connected
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
                    // When sshd doesn't allow TCP forwarding, go through yi-agentd bridge
                    Err(_) => {
                        let p = deploy::detect(&s).await?;
                        let st = s.exec_stream(&p.command("bridge")).await?;
                        let (r, w) = tokio::io::split(st);
                        handshake(Box::new(r), Box::new(w), &req, &h.name).await
                    }
                }
            }
            HostKind::Relay | HostKind::Proxy => {
                // Open a new connection to yi-agentd (hosts forwarded through a computer go through proxy.connect first); the first message says which port to connect to
                let c = HostConn::default();
                let (r, w) = self.open_transport(&h, &c).await?;
                handshake(r, w, &req, &h.name).await
            }
        }
    }

    async fn open_transport(&self, h: &HostConfig, c: &HostConn) -> Result<(BoxR, BoxW)> {
        {
            let mut b = c.bulk.lock().await;
            if let Some(brpc) = b.rpc.take() {
                brpc.close();
            }
            *b = Bulk::default();
        }
        match h.kind {
            HostKind::Local => {
                if !self.local_supported {
                    bail!("当前设备不支持本机会话");
                }
                let exe = self.local_yi_agentd_async().await?;
                match crate::localipc::connect().await {
                    Ok(stream) => {
                        c.bulk.lock().await.local = true;
                        let (r, w) = tokio::io::split(stream);
                        Ok((Box::new(r), Box::new(w)))
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Err(anyhow!(e)),
                    Err(_) => {
                        let mut cmd = tokio::process::Command::new(exe);
                        // --local: tells the daemon this is a local client (bridges started remotely over SSH don't pass it, so local-only methods are refused)
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
                        c.bulk.lock().await.local = true;
                        Ok((Box::new(r), Box::new(w)))
                    }
                }
            }
            HostKind::Proxy => {
                // Forwarded through that computer's yi-agentd: open a new relay connection whose first message says which host to reach; after that it's the target yi-agentd itself
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
                c.bulk.lock().await.cmd = Some(p.command("bridge"));
                let (r, w) = tokio::io::split(stream);
                Ok((Box::new(r), Box::new(w)))
            }
        }
    }
}

/// Send the first message and read one line of reply; on success the connection becomes a passthrough
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

/// Open a URL with the system default program
fn open_url(url: &str) -> Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // Don't go through cmd: it treats & in URLs as a command separator
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

/// Config sent to the frontend carries no plaintext passwords / private keys / pairing keys
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

/// This machine's name reported to the computer when pairing, shown in its "Paired devices"
pub fn device_name() -> String {
    if cfg!(target_os = "android") {
        return "Android 手机".into();
    }
    let h = gethostname::gethostname().to_string_lossy().to_string();
    if h.is_empty() || h == "localhost" { "未命名设备".into() } else { h }
}

fn local_yi_agentd_sync(yi_agentd_dir: Option<&std::path::Path>) -> Result<PathBuf> {
    let bundled = bundled_yi_agentd_from(yi_agentd_dir)?;
    if let Some(dir) = yi_agentd_dir {
        crate::localbin::sync_extras(dir);
    }
    crate::localbin::sync(&bundled).or_else(|e| {
        eprintln!("同步本机 yi-agentd 失败，直接使用安装包里的: {e:#}");
        Ok(bundled)
    })
}

fn bundled_yi_agentd_from(yi_agentd_dir: Option<&std::path::Path>) -> Result<PathBuf> {
    let exe = if cfg!(windows) { "yi-agentd.exe" } else { "yi-agentd" };
    let os = match std::env::consts::OS {
        "macos" => "macos",
        o => o,
    };
    let mut cands = vec![];
    if let Ok(p) = std::env::var("YI_CONSOLE_AGENTD") {
        cands.push(PathBuf::from(p));
    }
    if let Some(d) = yi_agentd_dir {
        cands.push(d.join(exe));
    }
    if let Ok(me) = std::env::current_exe() {
        if let Some(d) = me.parent() {
            cands.push(d.join(exe));
        }
    }
    if let Some(p) = deploy::local_binary(yi_agentd_dir, os, std::env::consts::ARCH) {
        cands.push(p);
    }
    cands
        .into_iter()
        .find(|p| p.is_file())
        .ok_or_else(|| anyhow!("找不到本机 yi-agentd（可设置环境变量 YI_CONSOLE_AGENTD）"))
}

#[derive(Debug)]
enum LocalPreviewErr {
    IdentityChanged,
    Other(anyhow::Error),
}

fn open_local_preview_file(p: &std::path::Path) -> std::io::Result<std::fs::File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(p)
    }
    #[cfg(not(unix))]
    {
        std::fs::File::open(p)
    }
}

fn local_file_identity(file: &std::fs::File, meta: &std::fs::Metadata) -> Result<crate::preview::FileIdentity> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let _ = file;
        Ok(crate::preview::FileIdentity {
            dev: meta.dev(),
            ino: meta.ino(),
        })
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION};
        let _ = meta;
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, &mut info) };
        if ok == 0 {
            bail!("获取本地文件标识失败: {}", std::io::Error::last_os_error());
        }
        Ok(crate::preview::FileIdentity {
            dev: info.dwVolumeSerialNumber as u64,
            ino: ((info.nFileIndexHigh as u64) << 32) | (info.nFileIndexLow as u64),
        })
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (file, meta);
        bail!("当前平台不支持校验本地预览文件标识")
    }
}

fn read_local_preview(
    path: &str,
    root: Option<&str>,
    expected_id: Option<crate::preview::FileIdentity>,
    max_bytes: u64,
) -> std::result::Result<Vec<u8>, LocalPreviewErr> {
    use std::io::Read;
    let mut p = crate::localipc::expand_path(path);
    if let Some(root) = root {
        let root = std::fs::canonicalize(crate::localipc::expand_path(root)).map_err(|e| LocalPreviewErr::Other(e.into()))?;
        let real = std::fs::canonicalize(&p).map_err(|e| LocalPreviewErr::Other(e.into()))?;
        if !real.starts_with(&root) {
            return Err(LocalPreviewErr::Other(anyhow!("不能读取预览目录以外的文件")));
        }
        p = real;
    }
    let file = open_local_preview_file(&p).map_err(|e| {
        if expected_id.is_some() && e.kind() == std::io::ErrorKind::NotFound {
            LocalPreviewErr::IdentityChanged
        } else {
            LocalPreviewErr::Other(e.into())
        }
    })?;
    let meta = file.metadata().map_err(|e| LocalPreviewErr::Other(e.into()))?;
    if !meta.is_file() {
        return Err(if expected_id.is_some() {
            LocalPreviewErr::IdentityChanged
        } else {
            LocalPreviewErr::Other(anyhow!("不是有效的文件"))
        });
    }
    if let Some(expected) = expected_id {
        let actual = local_file_identity(&file, &meta).map_err(LocalPreviewErr::Other)?;
        if actual != expected {
            return Err(LocalPreviewErr::IdentityChanged);
        }
    }
    if meta.len() > max_bytes {
        return Err(LocalPreviewErr::Other(anyhow!("文件太大，超过限制")));
    }
    let mut buf = Vec::new();
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut buf)
        .map_err(|e| LocalPreviewErr::Other(e.into()))?;
    if buf.len() as u64 > max_bytes {
        return Err(LocalPreviewErr::Other(anyhow!("文件太大，超过限制")));
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::{local_file_identity, open_local_preview_file, read_local_preview, LocalPreviewErr};

    #[test]
    fn local_preview_enforces_root_and_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let site = dir.path().join("site");
        let outside = dir.path().join("secret.txt");
        std::fs::create_dir_all(&site).unwrap();
        std::fs::write(&outside, "secret").unwrap();
        std::fs::write(site.join("ok.txt"), "hello").unwrap();

        assert_eq!(
            read_local_preview(&site.join("ok.txt").display().to_string(), Some(&site.display().to_string()), None, 50 * 1024 * 1024).unwrap(),
            b"hello"
        );
        assert!(read_local_preview(&outside.display().to_string(), Some(&site.display().to_string()), None, 50 * 1024 * 1024).is_err());
        assert!(read_local_preview(&site.display().to_string(), None, None, 50 * 1024 * 1024).is_err());
    }

    #[test]
    fn local_preview_rejects_replaced_file_and_enforces_limit() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("a.txt");
        let secret = dir.path().join("secret.txt");
        std::fs::write(&target, "original").unwrap();
        std::fs::write(&secret, "secret-data").unwrap();

        let f = open_local_preview_file(&target).unwrap();
        let meta = f.metadata().unwrap();
        let id_a = local_file_identity(&f, &meta).unwrap();
        drop(f);

        assert_eq!(
            read_local_preview(&target.display().to_string(), None, Some(id_a), 1024).unwrap(),
            b"original"
        );
        assert!(matches!(
            read_local_preview(&target.display().to_string(), None, Some(id_a), 4),
            Err(LocalPreviewErr::Other(_))
        ));

        std::fs::remove_file(&target).unwrap();
        std::fs::rename(&secret, &target).unwrap();

        assert!(matches!(
            read_local_preview(&target.display().to_string(), None, Some(id_a), 1024),
            Err(LocalPreviewErr::IdentityChanged)
        ));
    }
}
