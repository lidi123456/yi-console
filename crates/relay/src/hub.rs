//! Connection handling: relay-layer authentication, device control connections, splicing clients to streams.

use crate::limits::{self, Cidr, ConnSlot, Limits};
use crate::store::{now, Store};
use anyhow::{anyhow, Result};
use futures_util::{SinkExt, StreamExt};
use yi_link::auth::{self, AuthRequest};
use yi_link::{b64, device_id_of, unb64_32};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, WebSocketConfig};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

pub trait Io: AsyncRead + AsyncWrite + Unpin + Send + 'static {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> Io for T {}
pub type BoxIo = Box<dyn Io>;
type WsIo = WebSocketStream<BoxIo>;

/// Max concurrent streams per device
const MAX_STREAMS_PER_DEVICE: usize = 32;
/// Max per client
const MAX_STREAMS_PER_CLIENT: usize = 8;
/// Max concurrent connections per device that came in with a pairing ticket
const MAX_PAIRING: usize = 2;
/// Invites issued by devices: one-time, 10 minutes
const DEVICE_INVITE_TTL: u64 = 600;
/// Time limits for each step: TCP connect to WebSocket upgrade, and upgrade to authentication
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

pub struct Config {
    pub open: bool,
    pub device_invites: bool,
    pub trusted: Vec<Cidr>,
}

struct Waiting {
    tx: oneshot::Sender<WsIo>,
}

struct Active {
    client: [u8; 32],
    pairing: bool,
    kill: Option<oneshot::Sender<()>>,
}

struct Device {
    control: Option<(u64, mpsc::UnboundedSender<Message>)>,
    waiting: HashMap<String, Waiting>,
    /// Client allow list pushed by yi-agentd (signing public keys)
    acl: HashSet<[u8; 32]>,
    /// Valid pairing tickets and their expiry
    tickets: Vec<([u8; 32], u64)>,
    streams: HashMap<u64, Active>,
}

impl Device {
    fn new() -> Device {
        Device { control: None, waiting: HashMap::new(), acl: HashSet::new(), tickets: Vec::new(), streams: HashMap::new() }
    }

    fn count(&self, client: &[u8; 32]) -> usize {
        self.streams.values().filter(|a| &a.client == client).count()
    }
}

pub struct State {
    pub cfg: Config,
    pub store: Store,
    /// Serializes writes at runtime (command-line processes are separate, see store.rs)
    store_lock: Mutex<()>,
    /// Public keys of registered devices (cache of the file, reread when the file changes)
    registered: RwLock<BTreeMap<String, [u8; 32]>>,
    devices: Mutex<HashMap<String, Device>>,
    /// Notifies all of a device's connections when it's revoked
    kills: Mutex<HashMap<String, watch::Sender<bool>>>,
    seq: AtomicU64,
}

impl State {
    pub fn new(cfg: Config, store: Store) -> Arc<State> {
        let registered = store.device_keys();
        Arc::new(State {
            cfg,
            store,
            store_lock: Mutex::new(()),
            registered: RwLock::new(registered),
            devices: Mutex::new(HashMap::new()),
            kills: Mutex::new(HashMap::new()),
            seq: AtomicU64::new(1),
        })
    }

    fn next_id(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::Relaxed)
    }

    fn kill_rx(&self, device: &str) -> watch::Receiver<bool> {
        self.kills.lock().unwrap().entry(device.to_string()).or_insert_with(|| watch::channel(false).0).subscribe()
    }

    /// Reread the registration file; devices no longer in it are disconnected immediately
    pub fn reload(&self) {
        let fresh = self.store.device_keys();
        let gone: Vec<String> = {
            let old = self.registered.read().unwrap();
            old.keys().filter(|k| !fresh.contains_key(*k)).cloned().collect()
        };
        *self.registered.write().unwrap() = fresh;
        for d in gone {
            eprintln!("device revoked: {d}");
            if let Some(k) = self.kills.lock().unwrap().remove(&d) {
                let _ = k.send(true);
            }
            self.devices.lock().unwrap().remove(&d);
        }
    }

    fn is_registered(&self, device: &str, key: &[u8; 32]) -> bool {
        self.registered.read().unwrap().get(device) == Some(key)
    }
}

/// Every 5 seconds, check whether the registration file was changed from the command line
pub async fn watch_store(state: Arc<State>) {
    let mut last = state.store.devices_mtime();
    loop {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let m = state.store.devices_mtime();
        if m != last {
            last = m;
            state.reload();
        }
    }
}

fn reject(code: StatusCode, msg: &str) -> ErrorResponse {
    let mut r = ErrorResponse::new(Some(msg.to_string()));
    *r.status_mut() = code;
    r
}

async fn close(ws: &mut WsIo, code: u16, reason: &str) {
    let _ = ws.close(Some(CloseFrame { code: CloseCode::from(code), reason: reason.to_string().into() })).await;
}

/// All handling of a connection after TLS
pub async fn handle(state: Arc<State>, limits: Arc<Limits>, mut slot: ConnSlot, io: BoxIo, peer: IpAddr) {
    let cfg = WebSocketConfig::default().max_message_size(Some(4 << 20)).max_frame_size(Some(1 << 20));
    let mut path = String::new();
    let mut xff = None;
    let upgraded = tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        tokio_tungstenite::accept_hdr_async_with_config(
            io,
            |req: &Request, resp: Response| {
                path = req.uri().path().to_string();
                xff = req.headers().get("x-forwarded-for").and_then(|v| v.to_str().ok()).map(str::to_string);
                // Also accept it when the reverse proxy didn't strip the path prefix
                if !["/v1/agent", "/v1/client", "/v1/stream"].iter().any(|p| path.ends_with(p)) {
                    return Err(reject(StatusCode::NOT_FOUND, "not found"));
                }
                Ok(resp)
            },
            Some(cfg),
        ),
    )
    .await;
    let ip = limits::real_ip(&state.cfg.trusted, peer, xff.as_deref());
    let mut ws = match upgraded {
        Ok(Ok(ws)) => ws,
        _ => {
            limits.record_fail(ip);
            return;
        }
    };
    // Behind a proxy: only now do we know the real IP
    if limits::trusted(&state.cfg.trusted, peer) && !slot.bind(ip) {
        close(&mut ws, auth::CLOSE_BUSY, "连接过多或已被暂时封禁").await;
        return;
    }
    let authed = match tokio::time::timeout(HANDSHAKE_TIMEOUT, authenticate(&state, &mut ws)).await {
        Ok(Ok(a)) => a,
        Ok(Err((code, reason, guess))) => {
            // Only failures that could be guessing (bad signature, garbage, guessing invite codes / pairing tickets) count toward banning;
            // a valid identity without permission (a removed device still auto-reconnecting) doesn't, so devices behind the same NAT aren't banned along with it
            if guess {
                limits.record_fail(ip);
            }
            eprintln!("{ip}: 鉴权失败 {code} {reason}");
            close(&mut ws, code, &reason).await;
            return;
        }
        Err(_) => {
            limits.record_fail(ip);
            close(&mut ws, auth::CLOSE_UNAUTHORIZED, "鉴权超时").await;
            return;
        }
    };
    let _slot = slot;
    let device = authed.req.device.clone();
    match authed.req.role.as_str() {
        auth::ROLE_AGENT => agent(state, device, ws).await,
        auth::ROLE_STREAM => {
            let tx = state.devices.lock().unwrap().get_mut(&device).and_then(|d| d.waiting.remove(&authed.req.stream));
            match tx {
                Some(w) => {
                    let _ = w.tx.send(ws);
                }
                None => close(&mut ws, auth::CLOSE_NO_RESPONSE, "数据流已过期").await,
            }
        }
        _ => client(state, device, authed.req.pub_key, authed.ticket, ws).await,
    }
}

struct Authed {
    req: AuthRequest,
    /// The client came in with a pairing ticket
    ticket: Option<[u8; 32]>,
}

/// (close code, reason, counts toward banning)
type AuthErr = (u16, String, bool);

/// Signature / format errors: count toward banning
fn deny(code: u16, msg: &str) -> AuthErr {
    (code, msg.to_string(), true)
}

/// Identity proven but not permitted: doesn't count toward banning
fn refuse(code: u16, msg: &str) -> AuthErr {
    (code, msg.to_string(), false)
}

async fn authenticate(state: &Arc<State>, ws: &mut WsIo) -> std::result::Result<Authed, AuthErr> {
    let nonce = yi_link::random_bytes::<32>();
    ws.send(Message::Text(auth::challenge_msg(&nonce).into()))
        .await
        .map_err(|_| deny(auth::CLOSE_UNAUTHORIZED, "连接断开"))?;
    let text = loop {
        match ws.next().await {
            Some(Ok(Message::Text(t))) => break t.to_string(),
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
            _ => return Err(deny(auth::CLOSE_UNAUTHORIZED, "第一条消息应为 auth")),
        }
    };
    let v: Value = serde_json::from_str(&text).map_err(|_| deny(auth::CLOSE_UNAUTHORIZED, "auth 消息格式错误"))?;
    let req = AuthRequest::parse(&v).map_err(|e| deny(auth::CLOSE_UNAUTHORIZED, &format!("auth 消息格式错误：{e}")))?;
    if !req.verify(&nonce) {
        return Err(deny(auth::CLOSE_UNAUTHORIZED, "签名无效"));
    }
    let mut ticket = None;
    match req.role.as_str() {
        auth::ROLE_AGENT => {
            if req.device != device_id_of(&req.pub_key) {
                return Err(deny(auth::CLOSE_UNAUTHORIZED, "设备 id 与公钥不符"));
            }
            if !state.is_registered(&req.device, &req.pub_key) {
                admit(state, &req)?;
            }
        }
        auth::ROLE_STREAM => {
            if req.device != device_id_of(&req.pub_key) || !state.is_registered(&req.device, &req.pub_key) {
                return Err(refuse(auth::CLOSE_FORBIDDEN, "设备未登记"));
            }
        }
        auth::ROLE_CLIENT => {
            let devs = state.devices.lock().unwrap();
            let d = match devs.get(&req.device) {
                Some(d) if d.control.is_some() => d,
                _ => return Err(refuse(auth::CLOSE_OFFLINE, "设备不在线")),
            };
            // With a pairing ticket, treat it as pairing (paired devices re-pairing with their existing identity send one too); the ticket is passed on to the computer,
            // which uses it to find the pairing code and handshake with the key derived from it
            if let Some(tk) = req.ticket {
                let t = now();
                if !d.tickets.iter().any(|(h, exp)| *exp > t && yi_link::ct_eq(h, &tk)) {
                    return Err(deny(auth::CLOSE_FORBIDDEN, "配对二维码已失效"));
                }
                if d.streams.values().filter(|a| a.pairing).count() >= MAX_PAIRING {
                    return Err(refuse(auth::CLOSE_BUSY, "配对请求过多，请稍后再试"));
                }
                ticket = Some(tk);
            } else if !d.acl.contains(&req.pub_key) {
                return Err(refuse(auth::CLOSE_FORBIDDEN, "这台电脑没有授权你连接（可能已被移除），请重新配对"));
            }
        }
        _ => return Err(deny(auth::CLOSE_UNAUTHORIZED, "未知角色")),
    }
    ws.send(Message::Text(json!({"type": "ok"}).to_string().into()))
        .await
        .map_err(|_| deny(auth::CLOSE_UNAUTHORIZED, "连接断开"))?;
    Ok(Authed { req, ticket })
}

/// Unregistered device: register with an invite code, or directly in open mode
fn admit(state: &Arc<State>, req: &AuthRequest) -> std::result::Result<(), AuthErr> {
    let _g = state.store_lock.lock().unwrap();
    let (via, by) = if !req.invite.is_empty() {
        match state.store.consume_invite(&req.invite) {
            Ok(Some(inv)) => (format!("invite:{}", inv.id), if inv.by == "admin" { String::new() } else { inv.by }),
            Ok(None) if state.cfg.open => ("open".to_string(), String::new()),
            Ok(None) => return Err(deny(auth::CLOSE_FORBIDDEN, "邀请码无效、已用过或已过期，请向中继运营者要一个新的")),
            Err(e) => return Err(refuse(auth::CLOSE_FORBIDDEN, &format!("登记失败：{e}"))),
        }
    } else if state.cfg.open {
        ("open".to_string(), String::new())
    } else {
        return Err(refuse(
            auth::CLOSE_FORBIDDEN,
            "这台电脑还没在中继上登记：需要中继的邀请链接（在中继服务器上运行 yi-relay invite 生成）",
        ));
    };
    state
        .store
        .admit(&req.device, &req.pub_key, &via, &by, "")
        .map_err(|e| refuse(auth::CLOSE_FORBIDDEN, &format!("登记失败：{e}")))?;
    state.registered.write().unwrap().insert(req.device.clone(), req.pub_key);
    eprintln!("device admitted: {} ({via})", req.device);
    Ok(())
}

/// yi-agentd's control connection
async fn agent(state: Arc<State>, device: String, ws: WsIo) {
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
    let conn_id = state.next_id();
    let mut kill = state.kill_rx(&device);
    {
        let mut devs = state.devices.lock().unwrap();
        let d = devs.entry(device.clone()).or_insert_with(Device::new);
        // A new connection replaces the old one (the old one may not have timed out yet when yi-agentd reconnects)
        d.control = Some((conn_id, tx.clone()));
    }
    {
        let _g = state.store_lock.lock().unwrap();
        state.store.touch(&device);
    }
    eprintln!("device online: {device}");
    let (mut sink, mut stream) = ws.split();
    let mut ping = tokio::time::interval(Duration::from_secs(25));
    let mut last_seen = tokio::time::Instant::now();
    loop {
        tokio::select! {
            m = rx.recv() => match m {
                Some(m) => if sink.send(m).await.is_err() { break },
                None => break,
            },
            m = stream.next() => match m {
                Some(Ok(Message::Text(t))) => {
                    last_seen = tokio::time::Instant::now();
                    if let Some(reply) = on_control(&state, &device, &t) {
                        let _ = tx.send(Message::Text(reply.to_string().into()));
                    }
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                Some(Ok(_)) => last_seen = tokio::time::Instant::now(),
            },
            _ = ping.tick() => {
                if last_seen.elapsed() > Duration::from_secs(90) { break; }
                if sink.send(Message::Ping(Vec::new().into())).await.is_err() { break; }
            }
            _ = kill.changed() => {
                let _ = sink.send(Message::Close(Some(CloseFrame { code: CloseCode::from(auth::CLOSE_REVOKED), reason: "这台电脑已被中继运营者移除".into() }))).await;
                break;
            }
        }
    }
    let mut devs = state.devices.lock().unwrap();
    if let Some(d) = devs.get_mut(&device) {
        if d.control.as_ref().map(|(id, _)| *id) == Some(conn_id) {
            d.control = None;
            d.acl.clear();
            d.tickets.clear();
            eprintln!("device offline: {device}");
        }
    }
}

/// Messages from yi-agentd on the control connection: acl allow list, invite request
fn on_control(state: &Arc<State>, device: &str, text: &str) -> Option<Value> {
    let v: Value = serde_json::from_str(text).ok()?;
    match v["type"].as_str()? {
        "acl" => {
            let acl: HashSet<[u8; 32]> = v["clients"]
                .as_array()
                .map(|a| a.iter().filter_map(|x| unb64_32(x.as_str()?).ok()).collect())
                .unwrap_or_default();
            let tickets: Vec<([u8; 32], u64)> = v["tickets"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|x| Some((unb64_32(x["hash"].as_str()?).ok()?, x["exp"].as_u64()?)))
                        .collect()
                })
                .unwrap_or_default();
            let mut devs = state.devices.lock().unwrap();
            let d = devs.get_mut(device)?;
            // Clients not on the allow list (except those pairing) are disconnected immediately
            for a in d.streams.values_mut() {
                if !a.pairing && !acl.contains(&a.client) {
                    if let Some(k) = a.kill.take() {
                        let _ = k.send(());
                    }
                }
            }
            d.acl = acl;
            d.tickets = tickets;
            None
        }
        "invite" => Some(if !state.cfg.device_invites {
            json!({"type": "invite", "error": "这个中继没有开启电脑代发邀请，请在中继服务器上运行 yi-relay invite 生成邀请链接"})
        } else {
            let _g = state.store_lock.lock().unwrap();
            match state.store.create_invite(DEVICE_INVITE_TTL, 1, "", device) {
                Ok((rec, code)) => {
                    eprintln!("device {device} issued invite {}", rec.id);
                    json!({"type": "invite", "code": code})
                }
                Err(e) => json!({"type": "invite", "error": e.to_string()}),
            }
        }),
        _ => None,
    }
}

/// Client: ask yi-agentd to open a stream and splice them together
async fn client(state: Arc<State>, device: String, client_key: [u8; 32], ticket: Option<[u8; 32]>, mut ws: WsIo) {
    let sid = yi_link::random_id();
    let (tx, rx) = oneshot::channel();
    let sent: Result<()> = (|| {
        let mut devs = state.devices.lock().unwrap();
        let d = devs.get_mut(&device).filter(|d| d.control.is_some()).ok_or_else(|| anyhow!("设备不在线"))?;
        if d.streams.len() + d.waiting.len() >= MAX_STREAMS_PER_DEVICE || d.count(&client_key) >= MAX_STREAMS_PER_CLIENT {
            return Err(anyhow!("连接数超过上限"));
        }
        d.waiting.insert(sid.clone(), Waiting { tx });
        let mut msg = json!({"type": "open", "stream": sid, "client": b64(&client_key)});
        if let Some(t) = ticket {
            msg["ticket"] = json!(b64(&t));
        }
        let (_, c) = d.control.as_ref().unwrap();
        c.send(Message::Text(msg.to_string().into())).map_err(|_| anyhow!("设备不在线"))
    })();
    if let Err(e) = sent {
        let code = if e.to_string().contains("上限") { auth::CLOSE_BUSY } else { auth::CLOSE_OFFLINE };
        close(&mut ws, code, &e.to_string()).await;
        return;
    }
    let agent_ws = match tokio::time::timeout(Duration::from_secs(15), rx).await {
        Ok(Ok(w)) => w,
        _ => {
            if let Some(d) = state.devices.lock().unwrap().get_mut(&device) {
                d.waiting.remove(&sid);
            }
            close(&mut ws, auth::CLOSE_NO_RESPONSE, "设备没有响应").await;
            return;
        }
    };
    let id = state.next_id();
    let (ktx, krx) = oneshot::channel::<()>();
    if let Some(d) = state.devices.lock().unwrap().get_mut(&device) {
        d.streams.insert(id, Active { client: client_key, pairing: ticket.is_some(), kill: Some(ktx) });
    }
    let mut dev_kill = state.kill_rx(&device);
    let stop = async move {
        tokio::select! {
            r = krx => if r.is_err() { std::future::pending::<()>().await },
            _ = dev_kill.changed() => {}
        }
    };
    splice(ws, agent_ws, stop).await;
    if let Some(d) = state.devices.lock().unwrap().get_mut(&device) {
        d.streams.remove(&id);
    }
}

/// Forward data frames both ways (each leg handles its own Ping/Pong, not forwarded); when `stop` fires, both sides disconnect
async fn splice(a: WsIo, b: WsIo, stop: impl std::future::Future<Output = ()>) {
    let (mut a_tx, mut a_rx) = a.split();
    let (mut b_tx, mut b_rx) = b.split();
    let ab = async {
        while let Some(Ok(m)) = a_rx.next().await {
            match m {
                Message::Binary(_) | Message::Text(_) => {
                    if b_tx.send(m).await.is_err() {
                        break;
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
    };
    let ba = async {
        while let Some(Ok(m)) = b_rx.next().await {
            match m {
                Message::Binary(_) | Message::Text(_) => {
                    if a_tx.send(m).await.is_err() {
                        break;
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
    };
    let revoked = tokio::select! {
        _ = ab => false,
        _ = ba => false,
        _ = stop => true,
    };
    let frame = revoked.then(|| CloseFrame { code: CloseCode::from(auth::CLOSE_REVOKED), reason: "连接已被移除".into() });
    let _ = a_tx.send(Message::Close(frame.clone())).await;
    let _ = b_tx.send(Message::Close(frame)).await;
}

#[cfg(test)]
mod tests {
    //! Run a relay + simulated yi-agentd + client in-process through the full flow
    use super::*;
    use yi_link::{agent_accept, agent_control, auth::psk_of, auth::ticket_of, client_connect, client_pair, next_control, send_control};
    use yi_link::{AgentLink, Control, Credential, Identity, PairOffer};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    struct Relay {
        url: String,
        state: Arc<State>,
    }

    async fn relay(open: bool, device_invites: bool) -> Relay {
        let dir = std::env::temp_dir().join(format!("yi-relay-it-{}", yi_link::random_id()));
        let store = Store::new(&dir).unwrap();
        let state = State::new(Config { open, device_invites, trusted: vec![] }, store);
        let limits = Limits::new(1000, 1000);
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", l.local_addr().unwrap());
        let st = state.clone();
        tokio::spawn(async move {
            loop {
                let (tcp, peer) = l.accept().await.unwrap();
                let slot = limits.acquire(Some(peer.ip())).unwrap();
                tokio::spawn(handle(st.clone(), limits.clone(), slot, Box::new(tcp), peer.ip()));
            }
        });
        Relay { url, state }
    }

    /// Simulated yi-agentd: keeps authorized clients and pairing codes, handles open
    struct Agent {
        link: AgentLink,
        out: mpsc::UnboundedSender<Value>,
        invite: Arc<Mutex<Option<oneshot::Sender<std::result::Result<String, String>>>>>,
        closed: Arc<tokio::sync::Notify>,
    }

    type Shared = Arc<Mutex<(HashSet<[u8; 32]>, Vec<[u8; 32]>)>>;

    fn acl(s: &Shared) -> Value {
        let g = s.lock().unwrap();
        let exp = now() + 600;
        json!({"type": "acl",
            "clients": g.0.iter().map(|k| b64(k)).collect::<Vec<_>>(),
            "tickets": g.1.iter().map(|c| json!({"hash": b64(&ticket_of(c)), "exp": exp})).collect::<Vec<_>>()})
    }

    async fn agent(r: &Relay, invite: &str, shared: Shared) -> Result<Agent> {
        let link = AgentLink { relay: r.url.clone(), fp: String::new(), me: Identity::generate() };
        let mut ws = agent_control(&link, invite).await?;
        send_control(&mut ws, &acl(&shared)).await?;
        let (out, mut rx) = mpsc::unbounded_channel::<Value>();
        let invite_slot: Arc<Mutex<Option<oneshot::Sender<_>>>> = Arc::new(Mutex::new(None));
        let closed = Arc::new(tokio::sync::Notify::new());
        let (l2, inv2, closed2, out2) = (link.clone(), invite_slot.clone(), closed.clone(), out.clone());
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    m = rx.recv() => { let Some(m) = m else { break }; if send_control(&mut ws, &m).await.is_err() { break } }
                    c = next_control(&mut ws) => match c {
                        Ok(Control::Open(req)) => {
                            let (l3, sh, out3) = (l2.clone(), shared.clone(), out2.clone());
                            tokio::spawn(async move {
                                let code = req.ticket.and_then(|t| sh.lock().unwrap().1.iter().find(|c| ticket_of(c) == t).copied());
                                let psk = code.map(|c| psk_of(&c));
                                let sh2 = sh.clone();
                                let r = agent_accept(&l3, &req, psk, None, |h| {
                                    if h.sign_pub != req.client { return Err(anyhow!("身份不一致")) }
                                    let mut g = sh2.lock().unwrap();
                                    if let Some(c) = code {
                                        g.1.retain(|x| *x != c);
                                        g.0.insert(h.sign_pub);
                                        drop(g);
                                        let _ = out3.send(acl(&sh2));
                                        Ok(())
                                    } else if g.0.contains(&h.sign_pub) { Ok(()) } else { Err(anyhow!("未授权")) }
                                }).await;
                                if let Ok((mut d, _)) = r {
                                    // echo
                                    let mut buf = [0u8; 4096];
                                    while let Ok(n) = d.read(&mut buf).await { if n == 0 { break } if d.write_all(&buf[..n]).await.is_err() { break } }
                                }
                            });
                        }
                        Ok(Control::Invite(r)) => { if let Some(tx) = inv2.lock().unwrap().take() { let _ = tx.send(r); } }
                        Ok(Control::Other) => {}
                        Err(_) => break,
                    }
                }
            }
            closed2.notify_waiters();
        });
        Ok(Agent { link, out, invite: invite_slot, closed })
    }

    fn offer(a: &Agent, code: [u8; 32]) -> PairOffer {
        PairOffer { relay: a.link.relay.clone(), fp: String::new(), device: a.link.device(), apk: a.link.me.noise_pub(), code, name: "pc".into() }
    }

    async fn echo(c: &Credential) -> Result<tokio::io::DuplexStream> {
        let mut d = client_connect(c).await?;
        d.write_all(b"ping").await?;
        let mut b = [0u8; 4];
        tokio::time::timeout(Duration::from_secs(5), d.read_exact(&mut b)).await??;
        assert_eq!(&b, b"ping");
        Ok(d)
    }

    #[tokio::test]
    async fn full_flow() {
        let r = relay(false, false).await;
        let shared: Shared = Default::default();
        // Can't register without an invite
        let e = agent(&r, "", shared.clone()).await.err().unwrap();
        assert!(e.to_string().contains("4403"), "{e}");
        let (_, code) = r.state.store.create_invite(600, 1, "", "admin").unwrap();
        let a = agent(&r, &code, shared.clone()).await.unwrap();
        // An invite works only once, but a registered device can connect without one
        assert!(agent(&r, &code, Default::default()).await.is_err());
        let a2 = AgentLink { relay: r.url.clone(), fp: String::new(), me: a.link.me.clone() };

        // Pairing
        let pc = yi_link::random_bytes::<32>();
        shared.lock().unwrap().1.push(pc);
        a.out.send(acl(&shared)).unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let cred = client_pair(&offer(&a, pc), "phone", None).await.unwrap();
        // A pairing code is invalid once used
        assert!(client_pair(&offer(&a, pc), "phone2", None).await.is_err());
        // Wrong pairing code
        assert!(client_pair(&offer(&a, yi_link::random_bytes()), "x", None).await.is_err());
        tokio::time::sleep(Duration::from_millis(100)).await;
        let mut d = echo(&cred).await.unwrap();

        // Clients not on the allow list are refused by the relay
        let mut stranger = cred.clone();
        stranger.me = Identity::generate();
        let e = client_connect(&stranger).await.err().unwrap();
        assert!(e.to_string().contains("4403"), "{e}");

        // The same device pairs again: it keeps its identity, so it's still the same client on the computer, with no duplicate
        let pc2 = yi_link::random_bytes::<32>();
        shared.lock().unwrap().1.push(pc2);
        a.out.send(acl(&shared)).unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let cred2 = client_pair(&offer(&a, pc2), "phone", Some(cred.me.clone())).await.unwrap();
        assert!(cred2.me == cred.me);
        assert_eq!(shared.lock().unwrap().0.len(), 1);
        tokio::time::sleep(Duration::from_millis(100)).await;
        drop(echo(&cred2).await.unwrap());

        // Revocation: after a new allow list is pushed, existing connections are dropped and reconnecting fails
        shared.lock().unwrap().0.clear();
        a.out.send(acl(&shared)).unwrap();
        let mut b = [0u8; 1];
        assert_eq!(tokio::time::timeout(Duration::from_secs(5), d.read(&mut b)).await.unwrap().unwrap(), 0);
        assert!(client_connect(&cred).await.is_err());

        // Device-issued invites are off by default
        let (tx, rx) = oneshot::channel();
        *a.invite.lock().unwrap() = Some(tx);
        a.out.send(json!({"type": "invite"})).unwrap();
        assert!(rx.await.unwrap().is_err());

        // A registered device connects without an invite (the new connection replaces the old control connection, so this is tested last)
        let again = agent_control(&a2, "").await.unwrap();

        // Operator removes the device: the control connection drops and it can't connect afterwards
        let closed = a.closed.clone();
        let wait = closed.notified();
        r.state.store.revoke(&a.link.device(), false).unwrap();
        r.state.reload();
        tokio::time::timeout(Duration::from_secs(5), wait).await.unwrap();
        drop(again);
        let e = agent_control(&a2, "").await.err().unwrap();
        assert!(e.to_string().contains("4403"), "{e}");
    }

    #[tokio::test]
    async fn device_invites_and_open_mode() {
        let r = relay(false, true).await;
        let (_, code) = r.state.store.create_invite(600, 1, "", "admin").unwrap();
        let a = agent(&r, &code, Default::default()).await.unwrap();
        let (tx, rx) = oneshot::channel();
        *a.invite.lock().unwrap() = Some(tx);
        a.out.send(json!({"type": "invite"})).unwrap();
        let code2 = rx.await.unwrap().unwrap();
        let b = agent(&r, &code2, Default::default()).await.unwrap();
        let devs = r.state.store.devices();
        assert_eq!(devs[&b.link.device()].invited_by, a.link.device());

        let r = relay(true, false).await;
        assert!(agent(&r, "", Default::default()).await.is_ok());
    }

    #[tokio::test]
    async fn refused_clients_do_not_get_ip_banned() {
        // A removed device keeps auto-reconnecting; it's often behind the same NAT as healthy devices, so its IP mustn't get banned for it
        let r = relay(true, false).await;
        let shared: Shared = Default::default();
        let ok = Identity::generate();
        shared.lock().unwrap().0.insert(ok.sign_pub());
        let a = agent(&r, "", shared.clone()).await.unwrap();
        let cred = |me: Identity| Credential {
            relay: r.url.clone(),
            fp: String::new(),
            device: a.link.device(),
            apk: a.link.me.noise_pub(),
            name: String::new(),
            me,
        };
        let stranger = cred(Identity::generate());
        for _ in 0..15 {
            assert!(client_connect(&stranger).await.is_err());
        }
        echo(&cred(ok)).await.unwrap();
    }

    #[tokio::test]
    async fn offline_device() {
        let r = relay(true, false).await;
        let cred = Credential {
            relay: r.url.clone(),
            fp: String::new(),
            device: "nobody".into(),
            apk: [0; 32],
            name: String::new(),
            me: Identity::generate(),
        };
        let e = client_connect(&cred).await.err().unwrap();
        assert!(e.to_string().contains("4004"), "{e}");
    }
}
