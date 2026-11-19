//! WASM connector runtime: wasmtime + Component Model, implementing the `yi:connector` world in `connectors/wit`.
//!
//! Installed connectors live in `<app data>/connectors/<id>/` (connector.json, the component, optional prompt.md); plugins
//! shipped with the app (Feishu, GitHub) are loaded in place from its resources and win over an installed copy. Each
//! running instance gets its own thread with its own `Store`; the engine talks to it through a channel. Limits: a
//! call may run at most `CALL_LIMIT` of guest time (epoch interruption; time spent in host calls such as HTTP does
//! not count) and the instance's memory is capped at `MEMORY_LIMIT`. A trap only takes down that instance: it is
//! marked unhealthy and re-created with a backoff (1s doubling to 30s).
//!
//! The guest gets WASI with nothing granted (no files, no sockets, no env). Network and commands only go through the
//! `host` interface and are checked against the manifest's `permissions`.

use super::{ContextPage, ContextMessage, Manifest, Permissions, ConnectorHost, ConnectorInstance, ConnectorRuntime, SendResult, Sender, TriggerEvent};
use anyhow::{anyhow, bail, Context as _, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use wasmtime::component::{Component, HasSelf, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store, StoreLimits, StoreLimitsBuilder, UpdateDeadline};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

wasmtime::component::bindgen!({
    path: "../../connectors/wit",
    world: "yi-connector",
});

use yi::connector::types as wit;

pub const CALL_LIMIT: Duration = Duration::from_secs(5);
pub const MEMORY_LIMIT: usize = 64 << 20;
const TICK: Duration = Duration::from_millis(100);
const EXEC_LIMIT: Duration = Duration::from_secs(60);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Installed connectors and their compiled components
pub struct Wasm {
    engine: Engine,
    dir: PathBuf,
    connectors: Mutex<HashMap<String, Installed>>,
    rt: tokio::runtime::Handle,
}

struct Installed {
    manifest: Manifest,
    dir: PathBuf,
    component: Option<Component>,
}

impl Wasm {
    /// Load the connectors shipped in `bundled` (read-only, one folder per connector), then every connector installed under
    /// `dir`. An installed copy of a bundled connector (from before it was bundled) is deleted; its instances and data
    /// are keyed by connector id and carry over. Call inside the tokio runtime (WebSockets run on it)
    pub fn new(dir: &Path, bundled: Option<&Path>) -> Result<Arc<Wasm>> {
        let mut config = Config::new();
        config.wasm_component_model(true);
        config.epoch_interruption(true);
        let engine = Engine::new(&config).map_err(|e| anyhow!("{e:?}"))?;
        // One ticker for all instances
        let ticker = engine.clone();
        std::thread::Builder::new().name("wasm-epoch".into()).spawn(move || loop {
            std::thread::sleep(TICK);
            ticker.increment_epoch();
        })?;
        std::fs::create_dir_all(dir)?;
        let w = Arc::new(Wasm { engine, dir: dir.to_path_buf(), connectors: Mutex::new(HashMap::new()), rt: tokio::runtime::Handle::current() });
        let connector_dirs = |d: &Path| -> Vec<PathBuf> {
            let mut v: Vec<PathBuf> = std::fs::read_dir(d).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.join("connector.json").is_file()).collect();
            v.sort();
            v
        };
        for p in bundled.map(connector_dirs).unwrap_or_default() {
            if let Err(e) = w.load_from(&p, true) {
                eprintln!("bundled connector {}: {e:#}", p.display());
            }
        }
        for p in connector_dirs(dir) {
            match read_manifest(&p) {
                Ok(m) if w.is_bundled(&m.id) => {
                    let _ = std::fs::remove_dir_all(&p);
                }
                Ok(_) => {
                    if let Err(e) = w.load(&p) {
                        eprintln!("connector {}: {e:#}", p.display());
                    }
                }
                Err(e) => eprintln!("connector {}: {e:#}", p.display()),
            }
        }
        Ok(w)
    }

    /// Shipped with the app: can't be installed over or uninstalled
    pub fn is_bundled(&self, id: &str) -> bool {
        self.connectors.lock().unwrap().get(id).is_some_and(|p| p.manifest.bundled)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// (Re)load the connector in `dir` (after install or upgrade)
    pub fn load(&self, dir: &Path) -> Result<Manifest> {
        self.load_from(dir, false)
    }

    fn load_from(&self, dir: &Path, bundled: bool) -> Result<Manifest> {
        let mut m = read_manifest(dir)?;
        m.bundled = bundled;
        self.connectors.lock().unwrap().insert(m.id.clone(), Installed { manifest: m.clone(), dir: dir.to_path_buf(), component: None });
        Ok(m)
    }

    pub fn unload(&self, id: &str) {
        self.connectors.lock().unwrap().remove(id);
    }

    pub fn installed(&self, id: &str) -> Option<Manifest> {
        self.connectors.lock().unwrap().get(id).map(|p| p.manifest.clone())
    }

    /// Whether `bytes` is a component this app can run: it compiles, its imports are satisfied and it exports the
    /// `yi:connector` world. Blocking (compiles)
    pub fn check(&self, bytes: &[u8]) -> Result<()> {
        let c = Component::new(&self.engine, bytes).map_err(|e| anyhow!("连接器不是有效的 WASM 组件: {e:?}"))?;
        let pre = linker(&self.engine)?.instantiate_pre(&c).map_err(|e| anyhow!("连接器需要 App 不提供的接口: {e:?}"))?;
        YiConnectorPre::new(pre).map_err(|e| anyhow!("连接器没有实现 yi:connector 接口: {e:?}"))?;
        Ok(())
    }

    fn component(&self, id: &str) -> Result<(Component, Manifest)> {
        let (dir, m, cached) = {
            let p = self.connectors.lock().unwrap();
            let p = p.get(id).ok_or_else(|| anyhow!("连接器不存在: {id}"))?;
            (p.dir.clone(), p.manifest.clone(), p.component.clone())
        };
        if let Some(c) = cached {
            return Ok((c, m));
        }
        let main = m.main.clone().unwrap_or_else(|| "connector.wasm".into());
        let bytes = std::fs::read(dir.join(&main)).with_context(|| format!("读取 {main} 失败"))?;
        let c = Component::new(&self.engine, &bytes).map_err(|e| anyhow!("连接器不是有效的 WASM 组件: {e:?}"))?;
        if let Some(p) = self.connectors.lock().unwrap().get_mut(id) {
            p.component = Some(c.clone());
        }
        Ok((c, m))
    }
}

/// Read and check `connector.json` (plus prompt.md) in an unpacked connector directory
pub fn read_manifest(dir: &Path) -> Result<Manifest> {
    let raw = std::fs::read_to_string(dir.join("connector.json")).context("缺少 connector.json")?;
    let mut m: Manifest = serde_json::from_str(&raw).map_err(|e| anyhow!("connector.json 格式不对: {e}"))?;
    check_manifest(&m)?;
    if m.prompt.is_none() {
        if let Ok(p) = std::fs::read_to_string(dir.join("prompt.md")) {
            m.prompt = Some(p.trim().to_string()).filter(|p| !p.is_empty());
        }
    }
    Ok(m)
}

/// The rules every installable connector follows
pub fn check_manifest(m: &Manifest) -> Result<()> {
    if m.id.is_empty() || m.id.len() > 40 || !m.id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') {
        bail!("连接器 id 只能用小写字母、数字和 -：{}", m.id);
    }
    if super::builtin::RESERVED.contains(&m.id.as_str()) {
        bail!("连接器 id 和内置连接器重名：{}", m.id);
    }
    match m.runtime.as_str() {
        "wasm" => {}
        "node" => bail!("当前版本不支持 Node 连接器"),
        r => bail!("不认识的连接器运行时：{r}"),
    }
    if !super::SUPPORTED_APIS.contains(&m.api.as_str()) {
        bail!("连接器接口版本 {} 不受支持，请升级 App", m.api);
    }
    if let Some(main) = &m.main {
        if main.contains("..") || main.starts_with('/') || main.contains(':') || main.contains('\\') {
            bail!("main 路径不合法：{main}");
        }
    }
    for c in &m.capabilities {
        if !["trigger", "reply", "context", "notify", "tools"].contains(&c.as_str()) {
            bail!("不认识的能力：{c}");
        }
    }
    Ok(())
}

impl ConnectorRuntime for Wasm {
    fn kind(&self) -> &'static str {
        "wasm"
    }

    fn manifests(&self) -> Vec<Manifest> {
        let mut v: Vec<Manifest> = self.connectors.lock().unwrap().values().map(|p| p.manifest.clone()).collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        v
    }

    fn start(&self, connector_id: &str, host: Arc<dyn ConnectorHost>) -> Result<Arc<dyn ConnectorInstance>> {
        let (component, manifest) = self.component(connector_id)?;
        let (tx, rx) = mpsc::channel();
        let ctx = Ctx { engine: self.engine.clone(), component, perms: manifest.permissions.clone(), host, tx: tx.clone(), rt: self.rt.clone() };
        let name = format!("plugin-{}", ctx.host.instance_id());
        std::thread::Builder::new().name(name).spawn(move || run(ctx, rx))?;
        Ok(Arc::new(WasmInstance { tx: Mutex::new(tx) }))
    }
}

// ---------- the instance thread ----------

enum Call {
    Reply { thread: String, reply_to: Option<String>, key: String, text: String },
    Notify { text: String, key: String },
    ReadContext { thread: String, cursor: Option<String> },
    Tools,
    CallTool { name: String, input: String },
}

enum Out {
    Send(SendResult),
    Page(ContextPage),
    Tools(Vec<Value>),
    Text(String),
}

enum Msg {
    Call(Call, mpsc::Sender<Result<Out>>),
    Ws(u32, String, Vec<u8>),
    Stop,
}

/// Everything an instance thread needs to (re)create its instance
struct Ctx {
    engine: Engine,
    component: Component,
    perms: Permissions,
    host: Arc<dyn ConnectorHost>,
    tx: mpsc::Sender<Msg>,
    rt: tokio::runtime::Handle,
}

struct Timer {
    id: u32,
    due: Instant,
    every: Option<Duration>,
}

enum WsOut {
    Send(Vec<u8>, bool),
    Close,
}

/// Store data: WASI plus what the `host` interface needs
struct State {
    wasi: WasiCtx,
    table: ResourceTable,
    limits: StoreLimits,
    host: Arc<dyn ConnectorHost>,
    perms: Permissions,
    tx: mpsc::Sender<Msg>,
    rt: tokio::runtime::Handle,
    http: ureq::Agent,
    timers: Vec<Timer>,
    next_id: u32,
    ws: HashMap<u32, tokio::sync::mpsc::UnboundedSender<WsOut>>,
    /// Guest time accounting for the current call: host calls don't count against `CALL_LIMIT`
    call_start: Instant,
    host_time: Duration,
}

impl WasiView for State {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView { ctx: &mut self.wasi, table: &mut self.table }
    }
}

/// WASI (nothing granted) plus the `host` interface
fn linker(engine: &Engine) -> Result<Linker<State>> {
    let mut linker: Linker<State> = Linker::new(engine);
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker).map_err(|e| anyhow!("{e:?}"))?;
    YiConnector::add_to_linker::<State, HasSelf<State>>(&mut linker, |s| s).map_err(|e| anyhow!("{e:?}"))?;
    Ok(linker)
}

/// A live instance: its store and bindings
struct Live {
    store: Store<State>,
    bindings: YiConnector,
}

impl Live {
    fn new(ctx: &Ctx) -> Result<Live> {
        let linker = linker(&ctx.engine)?;
        let wasi = WasiCtx::builder().allow_tcp(false).allow_udp(false).allow_ip_name_lookup(false).build();
        let http: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(60)))
            .http_status_as_error(false)
            .build()
            .into();
        let state = State {
            wasi,
            table: ResourceTable::new(),
            limits: StoreLimitsBuilder::new().memory_size(MEMORY_LIMIT).instances(16).build(),
            host: ctx.host.clone(),
            perms: ctx.perms.clone(),
            tx: ctx.tx.clone(),
            rt: ctx.rt.clone(),
            http,
            timers: vec![],
            next_id: 1,
            ws: HashMap::new(),
            call_start: Instant::now(),
            host_time: Duration::ZERO,
        };
        let mut store = Store::new(&ctx.engine, state);
        store.limiter(|s| &mut s.limits);
        store.set_epoch_deadline(1);
        store.epoch_deadline_callback(|s| {
            let d = s.data();
            let guest = d.call_start.elapsed().saturating_sub(d.host_time);
            Ok(if guest > CALL_LIMIT { UpdateDeadline::Interrupt } else { UpdateDeadline::Continue(1) })
        });
        let bindings = YiConnector::instantiate(&mut store, &ctx.component, &linker).map_err(|e| anyhow!("实例化失败: {e:?}"))?;
        Ok(Live { store, bindings })
    }

    fn begin(&mut self) {
        let d = self.store.data_mut();
        d.call_start = Instant::now();
        d.host_time = Duration::ZERO;
        self.store.set_epoch_deadline(1);
    }

    /// Outer error: the instance trapped and must be re-created. Inner error: the connector said no
    fn call(&mut self, c: Call) -> Result<Result<Out>> {
        self.begin();
        let g = self.bindings.yi_connector_connector();
        let s = &mut self.store;
        let t = |e: wasmtime::Error| anyhow!("{e:?}");
        Ok(match c {
            Call::Reply { thread, reply_to, key, text } => {
                g.call_reply(s, &thread, reply_to.as_deref(), &key, &text).map_err(t)?.map(|r| Out::Send(send_result(r))).map_err(|e| anyhow!(e))
            }
            Call::Notify { text, key } => g.call_notify(s, &text, &key).map_err(t)?.map(|r| Out::Send(send_result(r))).map_err(|e| anyhow!(e)),
            Call::ReadContext { thread, cursor } => g
                .call_read_context(s, &thread, cursor.as_deref())
                .map_err(t)?
                .map(|p| Out::Page(page(p)))
                .map_err(|e| anyhow!(e)),
            Call::Tools => Ok(Out::Tools(
                g.call_tools(s)
                    .map_err(t)?
                    .into_iter()
                    .map(|d| {
                        json!({
                            "name": d.name,
                            "description": d.description,
                            "inputSchema": serde_json::from_str::<Value>(&d.input_schema).unwrap_or(json!({"type": "object"})),
                            "readOnly": d.read_only,
                        })
                    })
                    .collect(),
            )),
            Call::CallTool { name, input } => g.call_call_tool(s, &name, &input).map_err(t)?.map(Out::Text).map_err(|e| anyhow!(e)),
        })
    }

    fn start(&mut self) -> Result<Result<(), String>> {
        self.begin();
        self.bindings.yi_connector_connector().call_start(&mut self.store).map_err(|e| anyhow!("{e:?}"))
    }

    fn stop(&mut self) {
        self.begin();
        let _ = self.bindings.yi_connector_connector().call_stop(&mut self.store);
        for (_, w) in self.store.data_mut().ws.drain() {
            let _ = w.send(WsOut::Close);
        }
    }

    fn on_ws(&mut self, id: u32, kind: &str, data: &[u8]) -> Result<()> {
        if kind == "close" || kind == "error" {
            self.store.data_mut().ws.remove(&id);
        }
        self.begin();
        self.bindings.yi_connector_connector().call_on_ws(&mut self.store, id, kind, data).map_err(|e| anyhow!("{e:?}"))
    }

    fn next_due(&self) -> Option<Instant> {
        self.store.data().timers.iter().map(|t| t.due).min()
    }

    fn fire_timers(&mut self) -> Result<()> {
        let now = Instant::now();
        let due: Vec<u32> = self.store.data().timers.iter().filter(|t| t.due <= now).map(|t| t.id).collect();
        for id in due {
            // Reschedule or drop first: the guest may clear or re-add it during the call
            {
                let timers = &mut self.store.data_mut().timers;
                if let Some(pos) = timers.iter().position(|t| t.id == id) {
                    match timers[pos].every {
                        Some(every) => timers[pos].due = now + every,
                        None => {
                            timers.remove(pos);
                        }
                    }
                }
            }
            self.begin();
            self.bindings.yi_connector_connector().call_on_timer(&mut self.store, id).map_err(|e| anyhow!("{e:?}"))?;
        }
        Ok(())
    }
}

fn send_result(r: wit::SendResult) -> SendResult {
    match r {
        wit::SendResult::Sent => SendResult::Sent,
        wit::SendResult::Gone => SendResult::Gone,
    }
}

fn sender(s: wit::Sender) -> Sender {
    Sender { id: s.id, name: s.name, bot: s.bot }
}

fn page(p: wit::ContextPage) -> ContextPage {
    ContextPage {
        messages: p
            .messages
            .into_iter()
            .map(|m| ContextMessage { id: m.id, sender: sender(m.sender), at: m.at as i64, text: m.text, url: m.url })
            .collect(),
        next: p.next,
    }
}

/// The instance thread: create, start, serve calls and timers; re-create after a trap
fn run(ctx: Ctx, rx: mpsc::Receiver<Msg>) {
    let mut backoff = Duration::from_secs(1);
    loop {
        let started = Instant::now();
        let failure = match Live::new(&ctx) {
            Err(e) => format!("{e:#}"),
            Ok(mut live) => {
                ctx.host.health("starting", "");
                match live.start() {
                    Err(trap) => format!("启动时崩溃: {trap:#}"),
                    Ok(Err(e)) => {
                        // The connector refused (bad config, auth failed): keep serving, it may recover on its own timers
                        ctx.host.health("error", &e);
                        match serve(&mut live, &rx) {
                            None => return,
                            Some(trap) => trap,
                        }
                    }
                    Ok(Ok(())) => {
                        ctx.host.health("ok", "");
                        match serve(&mut live, &rx) {
                            None => return,
                            Some(trap) => trap,
                        }
                    }
                }
            }
        };
        ctx.host.health("error", &format!("{failure}（{}s 后重启）", backoff.as_secs()));
        ctx.host.log("error", &failure);
        // A long healthy run resets the backoff
        if started.elapsed() > Duration::from_secs(120) {
            backoff = Duration::from_secs(1);
        }
        let until = Instant::now() + backoff;
        loop {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            match rx.recv_timeout(left) {
                Ok(Msg::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => return,
                Ok(Msg::Call(_, reply)) => {
                    let _ = reply.send(Err(anyhow!("连接器正在重启")));
                }
                Ok(Msg::Ws(..)) | Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// Serve until stopped (None) or the instance traps (Some(why))
fn serve(live: &mut Live, rx: &mpsc::Receiver<Msg>) -> Option<String> {
    loop {
        let msg = match live.next_due() {
            Some(due) => match rx.recv_timeout(due.saturating_duration_since(Instant::now())) {
                Ok(m) => Some(m),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => Some(Msg::Stop),
            },
            None => Some(rx.recv().unwrap_or(Msg::Stop)),
        };
        let r = match msg {
            None => live.fire_timers(),
            Some(Msg::Stop) => {
                live.stop();
                return None;
            }
            Some(Msg::Ws(id, kind, data)) => live.on_ws(id, &kind, &data),
            Some(Msg::Call(c, reply)) => match live.call(c) {
                Ok(r) => {
                    let _ = reply.send(r);
                    Ok(())
                }
                Err(trap) => {
                    let _ = reply.send(Err(anyhow!("连接器崩溃: {trap:#}")));
                    Err(trap)
                }
            },
        };
        if let Err(trap) = r {
            return Some(format!("连接器崩溃: {trap:#}"));
        }
    }
}

/// The engine's handle on an instance thread
struct WasmInstance {
    tx: Mutex<mpsc::Sender<Msg>>,
}

impl WasmInstance {
    /// Blocks until the instance answers: call from a blocking context (the engine uses spawn_blocking)
    fn ask(&self, c: Call) -> Result<Out> {
        let (tx, rx) = mpsc::channel();
        self.tx.lock().unwrap().send(Msg::Call(c, tx)).map_err(|_| anyhow!("连接器已停止"))?;
        rx.recv().map_err(|_| anyhow!("连接器已停止"))?
    }
}

impl ConnectorInstance for WasmInstance {
    fn stop(&self) {
        let _ = self.tx.lock().unwrap().send(Msg::Stop);
    }

    fn reply(&self, thread: &str, reply_to: Option<&str>, key: &str, text: &str) -> Result<SendResult> {
        match self.ask(Call::Reply { thread: thread.into(), reply_to: reply_to.map(str::to_string), key: key.into(), text: text.into() })? {
            Out::Send(r) => Ok(r),
            _ => bail!("连接器返回值不对"),
        }
    }

    fn notify(&self, text: &str, key: &str) -> Result<SendResult> {
        match self.ask(Call::Notify { text: text.into(), key: key.into() })? {
            Out::Send(r) => Ok(r),
            _ => bail!("连接器返回值不对"),
        }
    }

    fn read_context(&self, thread: &str, cursor: Option<&str>) -> Result<ContextPage> {
        match self.ask(Call::ReadContext { thread: thread.into(), cursor: cursor.map(str::to_string) })? {
            Out::Page(p) => Ok(p),
            _ => bail!("连接器返回值不对"),
        }
    }

    fn tools(&self) -> Vec<Value> {
        match self.ask(Call::Tools) {
            Ok(Out::Tools(t)) => t,
            _ => vec![],
        }
    }

    fn call_tool(&self, name: &str, input: &Value) -> Result<String> {
        match self.ask(Call::CallTool { name: name.into(), input: input.to_string() })? {
            Out::Text(t) => Ok(t),
            _ => bail!("连接器返回值不对"),
        }
    }
}

// ---------- the host interface ----------

/// Whether `host` is allowed by the `permissions.net` patterns: exact names, or `*.example.com` for subdomains
pub fn net_allowed(perms: &Permissions, host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    perms.net.iter().any(|p| {
        let p = p.trim().to_ascii_lowercase();
        match p.strip_prefix("*.") {
            Some(base) => host.ends_with(&format!(".{base}")),
            None => p == host,
        }
    })
}

fn url_host(url: &str, schemes: &[&str]) -> Result<String, String> {
    let (scheme, rest) = url.split_once("://").ok_or("URL 不完整")?;
    if !schemes.contains(&scheme.to_ascii_lowercase().as_str()) {
        return Err(format!("不支持的协议：{scheme}"));
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let authority = authority.rsplit('@').next().unwrap_or("");
    let host = if let Some(v6) = authority.strip_prefix('[') {
        v6.split(']').next().unwrap_or("").to_string()
    } else {
        authority.split(':').next().unwrap_or("").to_string()
    };
    if host.is_empty() {
        return Err("URL 里没有主机名".into());
    }
    Ok(host)
}

impl State {
    /// Run a host call; its wall time does not count as guest time
    fn timed<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        let t = Instant::now();
        let r = f(self);
        self.host_time += t.elapsed();
        r
    }

    fn check_net(&self, url: &str, schemes: &[&str]) -> Result<(), String> {
        let host = url_host(url, schemes)?;
        if !net_allowed(&self.perms, &host) {
            return Err(format!("连接器没有访问 {host} 的权限"));
        }
        Ok(())
    }

    fn fetch(&mut self, request: &str) -> Result<String, String> {
        let r: Value = serde_json::from_str(request).map_err(|e| format!("请求不是 JSON: {e}"))?;
        let url = r["url"].as_str().ok_or("缺少 url")?.to_string();
        self.check_net(&url, &["http", "https"])?;
        let method = r["method"].as_str().unwrap_or("GET").to_ascii_uppercase();
        let mut b = ureq::http::Request::builder().method(method.as_str()).uri(&url);
        if let Some(h) = r["headers"].as_object() {
            for (k, v) in h {
                b = b.header(k, v.as_str().unwrap_or(&v.to_string()));
            }
        }
        let body: Vec<u8> = if let Some(s) = r["body"].as_str() {
            s.as_bytes().to_vec()
        } else if !r["json"].is_null() {
            b = b.header("content-type", "application/json");
            r["json"].to_string().into_bytes()
        } else {
            vec![]
        };
        let req = b.body(body).map_err(|e| format!("请求不合法: {e}"))?;
        let mut resp = self.http.run(req).map_err(|e| format!("请求失败: {e}"))?;
        let status = resp.status().as_u16();
        let headers: serde_json::Map<String, Value> =
            resp.headers().iter().map(|(k, v)| (k.as_str().to_string(), json!(v.to_str().unwrap_or("")))).collect();
        let bytes = resp.body_mut().with_config().limit(16 << 20).read_to_vec().map_err(|e| format!("读取响应失败: {e}"))?;
        Ok(json!({"status": status, "headers": headers, "body": String::from_utf8_lossy(&bytes)}).to_string())
    }

    fn open_ws(&mut self, url: &str, headers: &str) -> Result<u32, String> {
        self.check_net(url, &["ws", "wss"])?;
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue};
        let mut req = url.into_client_request().map_err(|e| format!("URL 不合法: {e}"))?;
        if let Ok(Value::Object(h)) = serde_json::from_str::<Value>(headers) {
            for (k, v) in h {
                if let (Ok(n), Ok(v)) = (HeaderName::from_bytes(k.as_bytes()), HeaderValue::from_str(v.as_str().unwrap_or(""))) {
                    req.headers_mut().insert(n, v);
                }
            }
        }
        let id = self.next_id;
        self.next_id += 1;
        let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<WsOut>();
        self.ws.insert(id, out_tx);
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            use futures_util::{SinkExt, StreamExt};
            use tokio_tungstenite::tungstenite::Message;
            let ev = |kind: &str, data: Vec<u8>| {
                let _ = tx.send(Msg::Ws(id, kind.into(), data));
            };
            let mut ws = match tokio_tungstenite::connect_async(req).await {
                Ok((ws, _)) => ws,
                Err(e) => return ev("error", e.to_string().into_bytes()),
            };
            ev("open", vec![]);
            loop {
                tokio::select! {
                    out = out_rx.recv() => match out {
                        Some(WsOut::Send(data, true)) => {
                            if let Err(e) = ws.send(Message::binary(data)).await {
                                return ev("error", e.to_string().into_bytes());
                            }
                        }
                        Some(WsOut::Send(data, false)) => {
                            if let Err(e) = ws.send(Message::text(String::from_utf8_lossy(&data).into_owned())).await {
                                return ev("error", e.to_string().into_bytes());
                            }
                        }
                        Some(WsOut::Close) | None => {
                            let _ = ws.close(None).await;
                            return;
                        }
                    },
                    msg = ws.next() => match msg {
                        Some(Ok(Message::Text(t))) => ev("text", t.as_bytes().to_vec()),
                        Some(Ok(Message::Binary(b))) => ev("binary", b.to_vec()),
                        Some(Ok(Message::Close(_))) | None => return ev("close", vec![]),
                        Some(Ok(_)) => {}
                        Some(Err(e)) => return ev("error", e.to_string().into_bytes()),
                    },
                }
            }
        });
        Ok(id)
    }

    fn exec(&mut self, cmd: &str, args: &[String], stdin: Option<&str>) -> Result<String, String> {
        if !self.perms.exec.iter().any(|c| c == cmd) {
            return Err(format!("连接器没有执行 {cmd} 的权限"));
        }
        let mut c = std::process::Command::new(cmd);
        c.args(args)
            .stdin(if stdin.is_some() { std::process::Stdio::piped() } else { std::process::Stdio::null() })
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            c.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = c.spawn().map_err(|e| format!("无法运行 {cmd}: {e}"))?;
        if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
            let input = input.to_string();
            std::thread::spawn(move || {
                let _ = pipe.write_all(input.as_bytes());
            });
        }
        let read = |p: Option<Box<dyn Read + Send>>| {
            std::thread::spawn(move || {
                let mut buf = vec![];
                if let Some(mut p) = p {
                    let _ = p.read_to_end(&mut buf);
                }
                buf
            })
        };
        let out = read(child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
        let err = read(child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
        let until = Instant::now() + EXEC_LIMIT;
        let status = loop {
            match child.try_wait() {
                Ok(Some(s)) => break s,
                Ok(None) if Instant::now() < until => std::thread::sleep(Duration::from_millis(20)),
                Ok(None) => {
                    let _ = child.kill();
                    return Err(format!("{cmd} 超过 {} 秒没有结束", EXEC_LIMIT.as_secs()));
                }
                Err(e) => return Err(e.to_string()),
            }
        };
        let out = String::from_utf8_lossy(&out.join().unwrap_or_default()).into_owned();
        let err = String::from_utf8_lossy(&err.join().unwrap_or_default()).into_owned();
        if status.success() {
            Ok(out)
        } else {
            Err(format!("{cmd} 退出码 {}: {}", status.code().unwrap_or(-1), err.trim()))
        }
    }
}

impl wit::Host for State {}

impl yi::connector::host::Host for State {
    fn config(&mut self) -> String {
        self.host.config().to_string()
    }

    fn emit(&mut self, e: wit::TriggerEvent) {
        let meta = serde_json::from_str(&e.meta).unwrap_or(Value::Null);
        self.host.emit(TriggerEvent {
            key: e.key,
            thread: e.thread,
            sender: sender(e.sender),
            addressed: e.addressed,
            text: e.text,
            url: e.url,
            at: e.at as i64,
            meta,
        });
    }

    fn kv_get(&mut self, key: String) -> Option<String> {
        self.timed(|s| s.host.kv_get(&key))
    }

    fn kv_set(&mut self, key: String, value: Option<String>) {
        self.timed(|s| s.host.kv_set(&key, value.as_deref()))
    }

    fn log(&mut self, level: String, message: String) {
        self.host.log(&level, &message);
    }

    fn health(&mut self, state: String, detail: String) {
        self.host.health(&state, &detail);
    }

    fn now(&mut self) -> u64 {
        chrono::Local::now().timestamp_millis() as u64
    }

    fn http_fetch(&mut self, request: String) -> Result<String, String> {
        self.timed(|s| s.fetch(&request))
    }

    fn ws_open(&mut self, url: String, headers: String) -> Result<u32, String> {
        self.open_ws(&url, &headers)
    }

    fn ws_send(&mut self, id: u32, data: Vec<u8>, binary: bool) -> Result<(), String> {
        let w = self.ws.get(&id).ok_or("连接已关闭")?;
        w.send(WsOut::Send(data, binary)).map_err(|_| "连接已关闭".to_string())
    }

    fn ws_close(&mut self, id: u32) {
        if let Some(w) = self.ws.remove(&id) {
            let _ = w.send(WsOut::Close);
        }
    }

    fn exec(&mut self, cmd: String, args: Vec<String>, stdin: Option<String>) -> Result<String, String> {
        self.timed(|s| s.exec(&cmd, &args, stdin.as_deref()))
    }

    fn set_timer(&mut self, ms: u64, repeat: bool) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        let d = Duration::from_millis(ms.max(10));
        self.timers.push(Timer { id, due: Instant::now() + d, every: repeat.then_some(d) });
        id
    }

    fn clear_timer(&mut self, id: u32) {
        self.timers.retain(|t| t.id != id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn net_patterns() {
        let p = Permissions { net: vec!["api.github.com".into(), "*.feishu.cn".into()], exec: vec![] };
        assert!(net_allowed(&p, "api.github.com"));
        assert!(net_allowed(&p, "API.GitHub.com"));
        assert!(!net_allowed(&p, "github.com"));
        assert!(net_allowed(&p, "open.feishu.cn"));
        assert!(net_allowed(&p, "a.b.feishu.cn"));
        assert!(!net_allowed(&p, "feishu.cn.evil.com"));
        assert!(!net_allowed(&p, "evilfeishu.cn"));
        assert_eq!(url_host("https://user@api.github.com:443/x?y", &["https"]).unwrap(), "api.github.com");
        assert_eq!(url_host("http://[::1]:80/", &["http"]).unwrap(), "::1");
        assert!(url_host("file:///etc/passwd", &["http", "https"]).is_err());
    }

    #[test]
    fn manifests_are_checked() {
        let mut m = Manifest {
            id: "github".into(),
            version: "0.1.0".into(),
            api: "0.1".into(),
            runtime: "wasm".into(),
            main: None,
            name: Default::default(),
            description: Default::default(),
            capabilities: vec!["trigger".into()],
            permissions: Default::default(),
            config: Value::Null,
            prompt: None,
            singleton: false,
            bundled: false,
        };
        check_manifest(&m).unwrap();
        m.runtime = "node".into();
        assert!(check_manifest(&m).unwrap_err().to_string().contains("Node"));
        m.runtime = "wasm".into();
        m.id = "Bad_Id".into();
        assert!(check_manifest(&m).is_err());
        m.id = "cron".into();
        assert!(check_manifest(&m).is_err());
        m.id = "x".into();
        m.main = Some("../evil.wasm".into());
        assert!(check_manifest(&m).is_err());
    }

    #[test]
    fn bundled_connectors_win_over_installed_copies() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _g = rt.enter();
        let write = |dir: &Path, id: &str, extra: &str| {
            std::fs::create_dir_all(dir.join(id)).unwrap();
            let json = format!(r#"{{"id":"{id}","version":"1.0.0","api":"0.1","runtime":"wasm","name":{{"zh":"x","en":"x"}},"capabilities":["trigger"]{extra}}}"#);
            std::fs::write(dir.join(id).join("connector.json"), json).unwrap();
        };
        let shipped = tempfile::tempdir().unwrap();
        let installed = tempfile::tempdir().unwrap();
        write(shipped.path(), "github", "");
        write(installed.path(), "github", "");
        // connector.json can't claim to be bundled
        write(installed.path(), "mine", r#","bundled":true"#);
        let w = Wasm::new(installed.path(), Some(shipped.path())).unwrap();
        assert!(w.is_bundled("github") && !w.is_bundled("mine"));
        assert!(w.installed("mine").is_some());
        assert!(!installed.path().join("github").exists(), "the installed copy is removed");
        assert!(shipped.path().join("github").exists());
    }

    struct TestHost {
        config: Value,
        events: Mutex<Vec<TriggerEvent>>,
        kv: Mutex<HashMap<String, String>>,
        health: Mutex<Vec<(String, String)>>,
    }

    impl ConnectorHost for TestHost {
        fn instance_id(&self) -> &str {
            "t1"
        }
        fn config(&self) -> Value {
            self.config.clone()
        }
        fn emit(&self, event: TriggerEvent) {
            self.events.lock().unwrap().push(event);
        }
        fn kv_get(&self, key: &str) -> Option<String> {
            self.kv.lock().unwrap().get(key).cloned()
        }
        fn kv_set(&self, key: &str, value: Option<&str>) {
            let mut kv = self.kv.lock().unwrap();
            match value {
                Some(v) => kv.insert(key.into(), v.into()),
                None => kv.remove(key),
            };
        }
        fn log(&self, _level: &str, _message: &str) {}
        fn health(&self, state: &str, detail: &str) {
            self.health.lock().unwrap().push((state.into(), detail.into()));
        }
    }

    /// A one-request-per-connection HTTP mock: records (path, body), serves one event and accepts replies
    fn mock_server() -> (u16, Arc<Mutex<Vec<(String, String)>>>) {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(vec![]));
        let s2 = seen.clone();
        std::thread::spawn(move || {
            for c in l.incoming().flatten() {
                let mut c = c;
                let mut buf = vec![];
                let mut tmp = [0u8; 4096];
                let (head, body) = loop {
                    let n = c.read(&mut tmp).unwrap_or(0);
                    if n == 0 {
                        break (String::new(), String::new());
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    let text = String::from_utf8_lossy(&buf).to_string();
                    if let Some((h, b)) = text.split_once("\r\n\r\n") {
                        let len = h
                            .lines()
                            .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0)))
                            .unwrap_or(0);
                        if b.len() >= len {
                            break (h.to_string(), b.to_string());
                        }
                    }
                };
                let path = head.split_whitespace().nth(1).unwrap_or("").to_string();
                let resp = if path == "/events?after=0" {
                    json!([{"seq": 1, "key": "e1", "thread": "t:1", "sender": "alice", "addressed": true, "text": "hi"}])
                } else if path.starts_with("/events") || path.starts_with("/context") {
                    json!([])
                } else {
                    json!({"ok": true})
                };
                s2.lock().unwrap().push((path, body));
                let b = resp.to_string();
                let _ = write!(c, "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{b}", b.len());
            }
        });
        (port, seen)
    }

    fn wait(what: &str, mut f: impl FnMut() -> bool) {
        let until = Instant::now() + Duration::from_secs(20);
        while !f() {
            assert!(Instant::now() < until, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Runs the real components built by `plugins/` (set YI_TEST_CONNECTORS to its dist directory); skipped otherwise
    #[test]
    fn testkit_component() {
        let Some(dist) = std::env::var_os("YI_TEST_CONNECTORS").map(PathBuf::from) else {
            eprintln!("YI_TEST_CONNECTORS not set, skipped");
            return;
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _g = rt.enter();
        let tmp = tempfile::tempdir().unwrap();
        let wasm = Wasm::new(tmp.path(), None).unwrap();
        for id in ["github", "feishu", "testkit"] {
            let m = wasm.load(&dist.join(id)).unwrap();
            wasm.check(&std::fs::read(dist.join(id).join(m.main.as_deref().unwrap_or("connector.wasm"))).unwrap()).unwrap();
        }
        let (port, seen) = mock_server();
        let host = Arc::new(TestHost {
            config: json!({"server": format!("http://127.0.0.1:{port}"), "pollMs": 200}),
            events: Mutex::new(vec![]),
            kv: Mutex::new(HashMap::new()),
            health: Mutex::new(vec![]),
        });
        let inst = wasm.start("testkit", host.clone()).unwrap();
        wait("the event", || host.events.lock().unwrap().iter().any(|e| e.text == "hi" && e.sender.id == "alice"));
        assert_eq!(host.kv_get("after").as_deref(), Some("1"));

        assert_eq!(inst.reply("t:1", Some("e1"), "k1", "hello").unwrap(), SendResult::Sent);
        assert!(seen.lock().unwrap().iter().any(|(p, b)| p == "/reply" && b.contains("\"hello\"")));
        assert_eq!(inst.tools().len(), 5);
        assert_eq!(inst.call_tool("echo", &json!({"text": "x"})).unwrap(), "x");
        // The sandbox: no commands, no hosts outside permissions.net
        let r = inst.call_tool("exec", &json!({})).unwrap();
        assert!(r.starts_with("refused"), "{r}");
        let r = inst.call_tool("fetch_denied", &json!({})).unwrap();
        assert!(r.starts_with("refused"), "{r}");

        // A runaway call is cut off by the time limit, then the instance comes back
        let t = Instant::now();
        let e = inst.call_tool("spin", &json!({})).unwrap_err().to_string();
        assert!(e.contains("崩溃"), "{e}");
        assert!(t.elapsed() < CALL_LIMIT + Duration::from_secs(5));
        wait("the restart", || inst.call_tool("echo", &json!({"text": "back"})).is_ok_and(|r| r == "back"));
        assert!(host.health.lock().unwrap().iter().any(|(s, d)| s == "error" && d.contains("崩溃")));
        inst.stop();
    }
}
