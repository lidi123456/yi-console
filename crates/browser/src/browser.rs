//! One Chrome process: its tabs, out-of-process iframe sessions, dialogs, console buffers and element refs.
//! Tabs are tracked from `Target.targetCreated` and attached with flattened sessions; iframes in other processes are
//! auto-attached per tab, as agent-browser does (`Target.setAutoAttach { flatten: true }`).

use crate::cdp::{Cdp, Event};
use crate::launch::{self, LaunchOptions};
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, watch};

const CONSOLE_LIMIT: usize = 200;

#[derive(Debug, Clone)]
pub struct Tab {
    /// Short id shown to the model (t1, t2, ...)
    pub id: String,
    pub target_id: String,
    pub session: String,
    pub url: String,
    pub title: String,
    pub main_frame: String,
    /// Lifecycle events seen since the main frame's last navigation (load, DOMContentLoaded, networkIdle, ...)
    pub lifecycle: HashSet<String>,
    /// Main-frame document loads (not same-document URL changes)
    pub navs: u64,
    pub console: VecDeque<String>,
    /// Digest of the last screenshot per scope, for `ifChanged`
    pub shots: HashMap<String, String>,
    /// The iframe browser_frame switched into
    pub frame: Option<FrameSel>,
    /// The last snapshot text and the options it was taken with, for browser_diff
    pub last_snapshot: Option<(String, String)>,
    /// HTTP status of the main document's last load
    pub doc_status: Option<i64>,
    /// Console errors and uncaught exceptions since the main frame's last load
    pub errors: u32,
    /// The renderer crashed; cleared by the next navigation
    pub crashed: bool,
}

#[derive(Debug, Clone)]
pub struct FrameSel {
    pub session: String,
    pub frame: String,
    /// The frame is that session's own root (an out-of-process iframe)
    pub own: bool,
    pub label: String,
}

/// browser_set state, applied to tabs opened later too
#[derive(Debug, Clone, Default)]
pub struct Emulation {
    pub viewport: Option<(u32, u32)>,
    /// Device scale factor, mobile layout and touch (with a viewport)
    pub scale: Option<f64>,
    pub mobile: bool,
    pub touch: bool,
    pub user_agent: Option<String>,
    pub color: Option<String>,
    pub reduced_motion: Option<String>,
    pub offline: bool,
    /// Network throttling: latency ms, download and upload in kbit/s
    pub throttle: Option<(f64, f64, f64)>,
    pub locale: Option<String>,
    pub timezone: Option<String>,
    pub geolocation: Option<(f64, f64, f64)>,
    /// Extra headers on every request
    pub headers: Vec<(String, String)>,
    /// Clear earlier overrides when applying
    pub reset: bool,
    /// The user agent override was removed
    pub reset_ua: bool,
    /// The viewport override was removed (back to the window size) and touch turned off
    pub reset_metrics: bool,
    /// Device preset in force, for the live view's size menu
    pub device: Option<String>,
}

impl Emulation {
    pub fn any(&self) -> bool {
        self.viewport.is_some()
            || self.user_agent.is_some()
            || self.color.is_some()
            || self.reduced_motion.is_some()
            || self.offline
            || self.throttle.is_some()
            || self.locale.is_some()
            || self.timezone.is_some()
            || self.geolocation.is_some()
            || !self.headers.is_empty()
    }
}

#[derive(Debug, Clone)]
pub struct Dialog {
    pub kind: String,
    pub message: String,
    pub default_prompt: String,
    pub session: String,
}

#[derive(Debug, Clone)]
pub struct RefEntry {
    pub tab: String,
    pub session: String,
    pub frame: String,
    pub backend: i64,
    pub role: String,
    pub name: String,
    /// role + name was unique in the snapshot that made it, so a replaced node may be found again by them
    pub unique: bool,
    pub valid: bool,
}

#[derive(Default)]
pub struct Refs {
    pub map: HashMap<String, RefEntry>,
    pub by_node: HashMap<(String, i64), String>,
    pub next: u32,
    /// Refs of the latest snapshot per tab, in output order (annotated screenshots label these)
    pub latest: HashMap<String, Vec<String>>,
}

impl Refs {
    /// The ref of a DOM node: the same one while the node lives, never a recycled number
    pub fn assign(&mut self, e: RefEntry) -> String {
        let key = (e.session.clone(), e.backend);
        if let Some(r) = self.by_node.get(&key) {
            if let Some(old) = self.map.get_mut(r) {
                if old.valid {
                    old.role = e.role;
                    old.name = e.name;
                    old.unique = e.unique;
                    return r.clone();
                }
            }
        }
        self.next += 1;
        let r = format!("e{}", self.next);
        self.by_node.insert(key, r.clone());
        self.map.insert(r.clone(), e);
        r
    }

    /// Navigation of `frame` in `session` (None: every frame of the session) ends its refs
    pub fn invalidate(&mut self, session: &str, frame: Option<&str>) {
        for e in self.map.values_mut() {
            if e.session == session && frame.is_none_or(|f| e.frame == f) {
                e.valid = false;
            }
        }
        self.by_node.retain(|(s, _), r| !(s == session && self.map.get(r).is_some_and(|e| !e.valid)));
    }

    pub fn invalidate_tab(&mut self, tab: &str) {
        for e in self.map.values_mut().filter(|e| e.tab == tab) {
            e.valid = false;
        }
        let map = &self.map;
        self.by_node.retain(|_, r| map.get(r).is_some_and(|e| e.valid));
    }
}

#[derive(Default)]
pub struct State {
    pub tabs: Vec<Tab>,
    pub current: Option<String>,
    next_tab: u32,
    attaching: HashSet<String>,
    /// Out-of-process iframe: frame id (= its target id) -> session
    pub oopif: HashMap<String, String>,
    /// Session (tab or iframe) -> tab target id
    pub session_tab: HashMap<String, String>,
    pub dialog: Option<Dialog>,
    /// Things the model should hear about with its next result (auto-accepted alerts, new tabs)
    pub notes: Vec<String>,
    pub refs: Refs,
    pub disconnected: bool,
    /// Default execution context per (session, frame id)
    pub contexts: HashMap<(String, String), i64>,
    /// Request log per tab target
    pub requests: HashMap<String, VecDeque<crate::net::Request>>,
    pub routes: Vec<crate::net::Route>,
    pub downloads: Vec<crate::net::Download>,
    /// Origins already reported as blocked by the allowlist
    pub blocked: Vec<String>,
    pub emulation: Emulation,
    /// A person used the live view since the AI's last call
    pub human: bool,
    /// Dedicated worker sessions (attached only with an allowlist, for interception)
    pub workers: HashSet<String>,
    /// A tool call is running
    pub busy: Option<String>,
    /// Where the AI's mouse is, for the live view (screencast frames have no cursor)
    pub pointer: Option<crate::view::Pointer>,
    /// The AI's current tab was closed (by the page or the person): the next call that needs it fails with this
    /// instead of acting on another tab, as agent-browser's `tab_gone`
    pub tab_gone: Option<String>,
    /// Headers for requests to one origin only (sent through interception, so they don't leak to other hosts)
    pub scoped_headers: Vec<crate::net::ScopedHeaders>,
    /// HTTP basic auth answers
    pub credentials: Vec<crate::net::Credentials>,
    /// The data of an intercepted HTML5 drag (Input.dragIntercepted), for browser_drag
    pub drag_data: Option<Value>,
}

impl State {
    pub fn tab(&self, target: &str) -> Option<&Tab> {
        self.tabs.iter().find(|t| t.target_id == target)
    }
    fn tab_mut(&mut self, target: &str) -> Option<&mut Tab> {
        self.tabs.iter_mut().find(|t| t.target_id == target)
    }
    fn tab_of_session(&mut self, session: &str) -> Option<&mut Tab> {
        let target = self.session_tab.get(session)?.clone();
        self.tab_mut(&target)
    }
    pub fn current_tab(&self) -> Option<&Tab> {
        self.current.as_deref().and_then(|c| self.tab(c))
    }
}

pub(crate) struct Inner {
    pub cdp: Cdp,
    pub state: Mutex<State>,
    /// Bumped on every event a wait (or the live view) may care about
    pub version: watch::Sender<u64>,
    /// Hosts the browser may reach; empty: no limit
    pub allowed: Vec<String>,
    pub download_dir: Option<std::path::PathBuf>,
    pub cast: Mutex<Option<crate::view::CastConfig>>,
    /// The session being screencast and how
    pub casting: Mutex<Option<(String, crate::view::CastConfig)>>,
    pub frames: watch::Sender<Option<Arc<crate::view::Frame>>>,
}

impl Inner {
    pub(crate) fn bump(&self) {
        self.version.send_modify(|v| *v += 1);
    }

    fn handle(self: &Arc<Self>, ev: Event) {
        let p = &ev.params;
        let s = |k: &str| p[k].as_str().unwrap_or("").to_string();
        match ev.method.as_str() {
            "Yi.disconnected" => self.state.lock().unwrap().disconnected = true,
            "Target.targetCreated" if p["targetInfo"]["type"] == "page" => {
                let id = p["targetInfo"]["targetId"].as_str().unwrap_or("").to_string();
                let opener = p["targetInfo"]["openerId"].as_str().is_some();
                let me = self.clone();
                tokio::spawn(async move {
                    if let Err(e) = me.attach_page(&id, opener).await {
                        eprintln!("[yi-browser] attach {id}: {e:#}");
                    }
                });
            }
            "Target.targetInfoChanged" => {
                let info = &p["targetInfo"];
                let mut st = self.state.lock().unwrap();
                if let Some(t) = st.tab_mut(info["targetId"].as_str().unwrap_or("")) {
                    t.url = info["url"].as_str().unwrap_or("").into();
                    // Without a parsed title Chrome reports the address; keep the one read from the page then
                    let title = info["title"].as_str().unwrap_or("");
                    if !t.url.contains(title) || t.title.is_empty() {
                        t.title = title.into();
                    }
                }
                drop(st);
                self.bump();
            }
            "Target.targetDestroyed" => {
                let id = s("targetId");
                let mut st = self.state.lock().unwrap();
                if let Some(i) = st.tabs.iter().position(|t| t.target_id == id) {
                    let t = st.tabs.remove(i);
                    st.refs.invalidate_tab(&id);
                    st.session_tab.retain(|_, tab| *tab != id);
                    if st.dialog.as_ref().is_some_and(|d| d.session == t.session) {
                        st.dialog = None;
                    }
                    if st.current.as_deref() == Some(id.as_str()) {
                        // The live view keeps showing a tab; the AI is told before it acts on one it didn't pick
                        st.current = st.tabs.last().map(|t| t.target_id.clone());
                        let left = st.tabs.iter().map(|t| format!("{} {}", t.id, crate::clip(&t.url, 80))).collect::<Vec<_>>().join("，");
                        st.tab_gone = Some(if left.is_empty() {
                            format!("当前标签页 {}（{}）已被关闭，没有其他标签页了：用 browser_tabs action=new 新建", t.id, crate::clip(&t.url, 120))
                        } else {
                            format!("当前标签页 {}（{}）已被关闭（页面自己关闭或用户关掉的）。还剩：{left}。用 browser_tabs action=select 选一个再继续", t.id, crate::clip(&t.url, 120))
                        });
                    }
                    st.requests.remove(&id);
                }
                drop(st);
                self.bump();
            }
            "Target.attachedToTarget" if p["targetInfo"]["type"] == "iframe" => {
                let Some(parent) = ev.session.clone() else { return };
                let child = s("sessionId");
                let frame = p["targetInfo"]["targetId"].as_str().unwrap_or("").to_string();
                {
                    let mut st = self.state.lock().unwrap();
                    let Some(tab) = st.session_tab.get(&parent).cloned() else { return };
                    st.oopif.insert(frame, child.clone());
                    st.session_tab.insert(child.clone(), tab);
                }
                let me = self.clone();
                let waiting = p["waitingForDebugger"].as_bool().unwrap_or(false);
                tokio::spawn(async move {
                    let _ = me.enable_session(&child).await;
                    if waiting {
                        // Paused so the allowlist guard is in before the frame's first script
                        let _ = me.cdp.call(Some(&child), "Runtime.runIfWaitingForDebugger", json!({})).await;
                    }
                });
            }
            "Target.attachedToTarget" if p["targetInfo"]["type"] == "worker" => {
                let child = s("sessionId");
                self.state.lock().unwrap().workers.insert(child.clone());
                let me = self.clone();
                tokio::spawn(async move {
                    let _ = me.set_fetch(&child, me.fetch_wanted()).await;
                    let _ = me.cdp.call(Some(&child), "Runtime.runIfWaitingForDebugger", json!({})).await;
                });
            }
            "Target.detachedFromTarget" => {
                let sid = s("sessionId");
                self.cdp.session_detached(&sid);
                let mut st = self.state.lock().unwrap();
                st.workers.remove(&sid);
                st.oopif.retain(|_, v| *v != sid);
                st.refs.invalidate(&sid, None);
                st.contexts.retain(|(s, _), _| *s != sid);
                for t in st.tabs.iter_mut() {
                    if t.frame.as_ref().is_some_and(|f| f.session == sid) {
                        t.frame = None;
                    }
                }
            }
            "Page.frameNavigated" => {
                let Some(sid) = ev.session else { return };
                let frame = &p["frame"];
                let fid = frame["id"].as_str().unwrap_or("");
                let mut st = self.state.lock().unwrap();
                let main = frame["parentId"].is_null() && st.tabs.iter().any(|t| t.session == sid);
                if main {
                    let target = st.session_tab.get(&sid).cloned().unwrap_or_default();
                    st.refs.invalidate_tab(&target);
                    if let Some(t) = st.tab_mut(&target) {
                        t.frame = None;
                        t.main_frame = fid.into();
                        t.url = frame["url"].as_str().unwrap_or("").into();
                        t.lifecycle.clear();
                        t.navs += 1;
                        t.errors = 0;
                        t.crashed = false;
                        // A page restored from the back/forward cache sends no lifecycle events again
                        if p["type"] == "BackForwardCacheRestore" {
                            t.lifecycle.extend(["DOMContentLoaded", "load", "networkIdle"].map(String::from));
                        }
                    }
                } else {
                    st.refs.invalidate(&sid, Some(fid));
                }
                drop(st);
                self.bump();
            }
            "Page.navigatedWithinDocument" => {
                let Some(sid) = ev.session else { return };
                let mut st = self.state.lock().unwrap();
                if let Some(t) = st.tabs.iter_mut().find(|t| t.session == sid && t.main_frame == p["frameId"].as_str().unwrap_or("")) {
                    t.url = s("url");
                }
                drop(st);
                self.bump();
            }
            "Page.lifecycleEvent" => {
                let Some(sid) = ev.session else { return };
                let mut st = self.state.lock().unwrap();
                let mut loaded = false;
                if let Some(t) = st.tabs.iter_mut().find(|t| t.session == sid && t.main_frame == p["frameId"].as_str().unwrap_or("")) {
                    let name = s("name");
                    if name == "init" {
                        t.lifecycle.clear();
                    }
                    loaded = name == "DOMContentLoaded" || name == "load";
                    t.lifecycle.insert(name);
                }
                drop(st);
                self.bump();
                if loaded {
                    // Target.targetInfoChanged doesn't always carry the parsed title; the live view shows it
                    let me = self.clone();
                    tokio::spawn(async move {
                        let r = me.cdp.call(Some(&sid), "Runtime.evaluate", json!({"expression": "document.title", "returnByValue": true})).await;
                        if let Some(title) = r.ok().and_then(|v| v["result"]["value"].as_str().map(String::from)) {
                            if let Some(t) = me.state.lock().unwrap().tabs.iter_mut().find(|t| t.session == sid) {
                                t.title = title;
                            }
                            me.bump();
                        }
                    });
                }
            }
            "Page.javascriptDialogOpening" => {
                let Some(sid) = ev.session else { return };
                let kind = s("type");
                let message = s("message");
                if kind == "alert" || kind == "beforeunload" {
                    // As agent-browser's auto_dialog: these only wait for OK
                    self.cdp.notify(Some(&sid), "Page.handleJavaScriptDialog", json!({"accept": true}));
                    let mut st = self.state.lock().unwrap();
                    st.notes.push(format!("页面弹出了 {kind} 对话框，已自动确认：{}", crate::clip(&message, 500)));
                } else {
                    let mut st = self.state.lock().unwrap();
                    st.dialog = Some(Dialog { kind, message, default_prompt: s("defaultPrompt"), session: sid });
                }
                self.bump();
            }
            "Page.javascriptDialogClosed" => {
                self.state.lock().unwrap().dialog = None;
                self.bump();
            }
            "Inspector.targetCrashed" => {
                let Some(sid) = ev.session else { return };
                let mut st = self.state.lock().unwrap();
                if let Some(t) = st.tabs.iter_mut().find(|t| t.session == sid) {
                    t.crashed = true;
                    let note = format!("标签页 {} 的页面崩溃了（{}）：用 browser_open action=reload 重新加载", t.id, crate::clip(&t.url, 120));
                    st.notes.push(note);
                }
                drop(st);
                self.bump();
            }
            "Input.dragIntercepted" => {
                self.state.lock().unwrap().drag_data = Some(p["data"].clone());
                self.bump();
            }
            "Fetch.authRequired" => {
                if let Some(sid) = ev.session.as_deref() {
                    self.on_auth(sid, p);
                }
            }
            "Runtime.consoleAPICalled" => {
                let Some(sid) = ev.session else { return };
                let text = p["args"].as_array().map(|a| a.iter().map(remote_text).collect::<Vec<_>>().join(" ")).unwrap_or_default();
                let line = format!("[{}] {}", s("type"), crate::clip(&text, 2000));
                if matches!(s("type").as_str(), "error" | "assert") {
                    if let Some(t) = self.state.lock().unwrap().tab_of_session(&sid) {
                        t.errors += 1;
                    }
                }
                self.push_console(&sid, line);
            }
            "Runtime.executionContextCreated" => {
                let Some(sid) = ev.session else { return };
                let c = &p["context"];
                if c["auxData"]["isDefault"].as_bool() == Some(true) {
                    if let (Some(f), Some(id)) = (c["auxData"]["frameId"].as_str(), c["id"].as_i64()) {
                        self.state.lock().unwrap().contexts.insert((sid, f.to_string()), id);
                    }
                }
            }
            "Runtime.executionContextDestroyed" => {
                let Some(sid) = ev.session else { return };
                let id = p["executionContextId"].as_i64();
                self.state.lock().unwrap().contexts.retain(|(s, _), v| !(*s == sid && Some(*v) == id));
            }
            "Runtime.executionContextsCleared" => {
                let Some(sid) = ev.session else { return };
                self.state.lock().unwrap().contexts.retain(|(s, _), _| *s != sid);
            }
            "Page.screencastFrame" => {
                if let Some(sid) = ev.session.as_deref() {
                    self.on_frame(sid, p);
                }
            }
            "Network.requestWillBeSent" | "Network.responseReceived" | "Network.loadingFinished" | "Network.loadingFailed" => {
                if let Some(sid) = ev.session.as_deref() {
                    self.on_network(sid, &ev.method, p);
                }
            }
            "Fetch.requestPaused" => {
                if let Some(sid) = ev.session.as_deref() {
                    self.on_paused(sid, p);
                }
            }
            "Browser.downloadWillBegin" | "Browser.downloadProgress" => self.on_download(&ev.method, p),
            "Runtime.exceptionThrown" => {
                let Some(sid) = ev.session else { return };
                let d = &p["exceptionDetails"];
                let text = d["exception"]["description"].as_str().or(d["text"].as_str()).unwrap_or("").to_string();
                if let Some(t) = self.state.lock().unwrap().tab_of_session(&sid) {
                    t.errors += 1;
                }
                self.push_console(&sid, format!("[exception] {}", crate::clip(&text, 2000)));
            }
            _ => {}
        }
    }

    pub(crate) fn push_console(&self, session: &str, line: String) {
        let mut st = self.state.lock().unwrap();
        if let Some(t) = st.tab_of_session(session) {
            t.console.push_back(line);
            while t.console.len() > CONSOLE_LIMIT {
                t.console.pop_front();
            }
        }
        drop(st);
        self.bump();
    }

    async fn enable_session(&self, sid: &str) -> Result<()> {
        let c = &self.cdp;
        c.call(Some(sid), "Page.enable", json!({})).await?;
        c.call(Some(sid), "Runtime.enable", json!({})).await?;
        c.call(Some(sid), "DOM.enable", json!({})).await?;
        c.call(Some(sid), "Accessibility.enable", json!({})).await?;
        c.call(Some(sid), "Page.setLifecycleEventsEnabled", json!({"enabled": true})).await?;
        c.call(Some(sid), "Network.enable", json!({})).await?;
        let guard = !self.allowed.is_empty();
        if guard {
            c.call(Some(sid), "Page.addScriptToEvaluateOnNewDocument", json!({"source": crate::net::guard_script(&self.allowed), "runImmediately": true})).await?;
        }
        if self.fetch_wanted() {
            self.set_fetch(sid, true).await?;
        }
        // With an allowlist, dedicated workers are attached too so their requests are intercepted
        let filter = if guard { json!([{"type": "iframe"}, {"type": "worker"}]) } else { json!([{"type": "iframe"}]) };
        c.call(Some(sid), "Target.setAutoAttach", json!({"autoAttach": true, "waitForDebuggerOnStart": guard, "flatten": true, "filter": filter})).await?;
        Ok(())
    }

    async fn attach_page(self: &Arc<Self>, target: &str, opener: bool) -> Result<()> {
        {
            let mut st = self.state.lock().unwrap();
            if st.tab(target).is_some() || !st.attaching.insert(target.to_string()) {
                return Ok(());
            }
        }
        let r = async {
            let a = self.cdp.call(None, "Target.attachToTarget", json!({"targetId": target, "flatten": true})).await?;
            let sid = a["sessionId"].as_str().ok_or_else(|| anyhow!("attach 没有返回 sessionId"))?.to_string();
            self.state.lock().unwrap().session_tab.insert(sid.clone(), target.to_string());
            self.enable_session(&sid).await?;
            // Crash events, and pages that act as focused even when the window isn't (a headed window behind
            // others, background tabs): focus/blur handlers, document.hasFocus() and timers keep working
            let _ = self.cdp.call(Some(&sid), "Inspector.enable", json!({})).await;
            let _ = self.cdp.call(Some(&sid), "Emulation.setFocusEmulationEnabled", json!({"enabled": true})).await;
            let e = self.state.lock().unwrap().emulation.clone();
            if e.any() {
                crate::extra::apply_emulation(&self.cdp, &sid, &e).await?;
            }
            let tree = self.cdp.call(Some(&sid), "Page.getFrameTree", json!({})).await?;
            let frame = &tree["frameTree"]["frame"];
            Ok::<_, anyhow::Error>((sid, frame["id"].as_str().unwrap_or("").to_string(), frame["url"].as_str().unwrap_or("").to_string()))
        }
        .await;
        let mut st = self.state.lock().unwrap();
        st.attaching.remove(target);
        let (sid, frame, url) = r?;
        st.next_tab += 1;
        let id = format!("t{}", st.next_tab);
        st.tabs.push(Tab {
            id: id.clone(),
            target_id: target.into(),
            session: sid,
            url: url.clone(),
            title: String::new(),
            main_frame: frame,
            lifecycle: HashSet::new(),
            navs: 0,
            console: VecDeque::new(),
            shots: HashMap::new(),
            frame: None,
            last_snapshot: None,
            doc_status: None,
            errors: 0,
            crashed: false,
        });
        if st.current.is_none() {
            st.current = Some(target.into());
        } else if opener {
            st.notes.push(format!("页面打开了新标签页 {id}（{url}），需要时用 browser_tabs 切换过去"));
        }
        drop(st);
        self.bump();
        Ok(())
    }
}

/// Text of a console argument (Runtime.RemoteObject)
fn remote_text(v: &Value) -> String {
    match v["value"].as_str() {
        Some(s) => s.to_string(),
        None if !v["value"].is_null() => v["value"].to_string(),
        None => v["description"].as_str().or(v["unserializableValue"].as_str()).unwrap_or(v["type"].as_str().unwrap_or("")).to_string(),
    }
}

/// Process-tree hooks supplied by the host (job object / process group), so the browser dies with the daemon
pub struct Hooks<'a> {
    pub prepare: &'a (dyn Fn(&mut Command) + Sync),
    pub attach: &'a (dyn Fn(&Child) -> Box<dyn std::any::Any + Send + Sync> + Sync),
}

pub struct Browser {
    pub(crate) inner: Arc<Inner>,
    child: tokio::sync::Mutex<Option<Child>>,
    /// The private X server of a headed browser on a Linux host without a display
    display: tokio::sync::Mutex<Option<Child>>,
    _tree: Box<dyn std::any::Any + Send + Sync>,
    pub options: LaunchOptions,
    pub version: String,
    /// Tool calls are serialized per browser
    pub(crate) op: tokio::sync::Mutex<()>,
}

impl Browser {
    pub async fn launch(mut options: LaunchOptions, hooks: Hooks<'_>) -> Result<Browser> {
        let mut display = None;
        let mut xvfb_tree = None;
        if !options.headless {
            if let Some((child, name)) = launch::private_display(&options, hooks.prepare).await? {
                xvfb_tree = Some((hooks.attach)(&child));
                options.env.push(("DISPLAY".into(), name));
                display = Some(child);
            }
        }
        let spawned = launch::spawn(&options, hooks.prepare).await?;
        let tree = (hooks.attach)(&spawned.child);
        let tree: Box<dyn std::any::Any + Send + Sync> = Box::new((tree, xvfb_tree));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let cdp = Cdp::connect(&spawned.ws_url, move |ev| {
            let _ = tx.send(ev);
        })
        .await?;
        let (version, _) = watch::channel(0);
        let (frames, _) = watch::channel(None);
        let inner = Arc::new(Inner {
            cdp,
            state: Mutex::new(State::default()),
            version,
            allowed: options.allowed_domains.iter().map(|d| d.trim().to_ascii_lowercase()).filter(|d| !d.is_empty()).collect(),
            download_dir: options.download_dir.clone(),
            cast: Mutex::new(None),
            casting: Mutex::new(None),
            frames,
        });
        let weak = Arc::downgrade(&inner);
        tokio::spawn(async move {
            while let Some(ev) = rx.recv().await {
                let Some(inner) = weak.upgrade() else { break };
                inner.handle(ev);
            }
        });
        let v = inner.cdp.call(None, "Browser.getVersion", json!({})).await?;
        // Before any page loads: where downloads go
        let downloads = match &options.download_dir {
            Some(d) => {
                std::fs::create_dir_all(d)?;
                json!({"behavior": "allowAndName", "downloadPath": d.to_string_lossy(), "eventsEnabled": true})
            }
            None => json!({"behavior": "deny", "eventsEnabled": true}),
        };
        inner.cdp.call(None, "Browser.setDownloadBehavior", downloads).await?;
        inner.cdp.call(None, "Target.setDiscoverTargets", json!({"discover": true})).await?;
        crate::view::spawn_caster(Arc::downgrade(&inner));
        let b = Browser {
            inner,
            child: tokio::sync::Mutex::new(Some(spawned.child)),
            display: tokio::sync::Mutex::new(display),
            _tree: tree,
            options,
            version: v["product"].as_str().unwrap_or("").to_string(),
            op: tokio::sync::Mutex::new(()),
        };
        // The first about:blank tab
        b.wait_until(Duration::from_secs(10), |st| st.current_tab().is_some()).await.map_err(|_| anyhow!("浏览器没有打开标签页"))?;
        Ok(b)
    }

    pub fn alive(&self) -> bool {
        !self.inner.cdp.is_closed() && !self.inner.state.lock().unwrap().disconnected
    }

    /// `alive`, plus the process hasn't exited and answers within a few seconds (a hung browser keeps its socket
    /// open), as agent-browser checks before every command
    pub async fn healthy(&self) -> bool {
        if !self.alive() {
            return false;
        }
        if let Some(c) = self.child.lock().await.as_mut() {
            if !matches!(c.try_wait(), Ok(None)) {
                return false;
            }
        }
        self.inner.cdp.call_within(None, "Browser.getVersion", json!({}), Duration::from_secs(5)).await.is_ok()
    }

    /// Close politely so cookies reach the profile, then make sure the process is gone
    pub async fn close(&self) {
        let _ = tokio::time::timeout(Duration::from_secs(3), self.inner.cdp.call(None, "Browser.close", json!({}))).await;
        if let Some(mut child) = self.child.lock().await.take() {
            if tokio::time::timeout(Duration::from_secs(5), child.wait()).await.is_err() {
                let _ = child.kill().await;
            }
        }
        if let Some(mut x) = self.display.lock().await.take() {
            let _ = x.kill().await;
        }
    }

    pub fn headless(&self) -> bool {
        self.options.headless
    }

    pub(crate) fn cdp(&self) -> &Cdp {
        &self.inner.cdp
    }

    pub(crate) fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.inner.state.lock().unwrap()
    }

    /// Wait until `cond` holds, re-checking after every relevant event
    pub(crate) async fn wait_until(&self, timeout: Duration, cond: impl Fn(&State) -> bool) -> Result<()> {
        let mut rx = self.inner.version.subscribe();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if cond(&self.state()) {
                return Ok(());
            }
            if self.state().disconnected {
                bail!("浏览器已退出");
            }
            match tokio::time::timeout_at(deadline, rx.changed()).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) => bail!("浏览器已退出"),
                Err(_) => bail!("等待超时"),
            }
        }
    }

    /// The current tab, or an error the model can act on
    pub(crate) fn current(&self) -> Result<Tab> {
        let mut st = self.state();
        if let Some(g) = st.tab_gone.take() {
            bail!("{g}");
        }
        let t = st.current_tab().cloned().ok_or_else(|| anyhow!("没有打开的标签页，请用 browser_tabs 新建一个"))?;
        if t.crashed {
            bail!("标签页 {} 的页面崩溃了：用 browser_open action=reload 重新加载", t.id);
        }
        Ok(t)
    }

    /// The current tab without the closed/crashed checks (reload of a crashed page, tab listing)
    pub(crate) fn current_raw(&self) -> Result<Tab> {
        self.state().current_tab().cloned().ok_or_else(|| anyhow!("没有打开的标签页，请用 browser_tabs 新建一个"))
    }

    pub(crate) fn take_notes(&self) -> Vec<String> {
        std::mem::take(&mut self.state().notes)
    }

    pub fn pending_dialog(&self) -> Option<Dialog> {
        self.state().dialog.clone()
    }

    /// Current tabs as (id, url, title, current)
    pub fn tab_list(&self) -> Vec<(String, String, String, bool)> {
        let st = self.state();
        st.tabs.iter().map(|t| (t.id.clone(), t.url.clone(), t.title.clone(), st.current.as_deref() == Some(t.target_id.as_str()))).collect()
    }

    pub fn current_url(&self) -> String {
        self.state().current_tab().map(|t| t.url.clone()).unwrap_or_default()
    }

    /// Evaluate an expression in a session's main world and return its value
    pub(crate) async fn eval(&self, session: &str, expr: &str) -> Result<Value> {
        let r = self
            .cdp()
            .call(Some(session), "Runtime.evaluate", json!({"expression": expr, "returnByValue": true, "awaitPromise": true}))
            .await?;
        if let Some(e) = r.get("exceptionDetails") {
            bail!("页面脚本出错：{}", e["exception"]["description"].as_str().or(e["text"].as_str()).unwrap_or(""));
        }
        Ok(r["result"]["value"].clone())
    }

    /// Call `function` with `this` = the object, returning its value
    pub(crate) async fn call_on(&self, session: &str, object: &str, function: &str, args: Vec<Value>) -> Result<Value> {
        let args: Vec<Value> = args.into_iter().map(|v| json!({"value": v})).collect();
        let r = self
            .cdp()
            .call(
                Some(session),
                "Runtime.callFunctionOn",
                json!({"objectId": object, "functionDeclaration": function, "arguments": args, "returnByValue": true, "awaitPromise": true}),
            )
            .await?;
        if let Some(e) = r.get("exceptionDetails") {
            bail!("页面脚本出错：{}", e["exception"]["description"].as_str().or(e["text"].as_str()).unwrap_or(""));
        }
        Ok(r["result"]["value"].clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(session: &str, frame: &str, backend: i64) -> RefEntry {
        RefEntry { tab: "T".into(), session: session.into(), frame: frame.into(), backend, role: "button".into(), name: "OK".into(), unique: true, valid: true }
    }

    #[test]
    fn refs_survive_while_the_node_lives_and_never_recycle() {
        let mut r = Refs::default();
        let a = r.assign(entry("s", "f", 1));
        let b = r.assign(entry("s", "f", 2));
        assert_eq!((a.as_str(), b.as_str()), ("e1", "e2"));
        // A new snapshot of the same nodes keeps the refs
        assert_eq!(r.assign(entry("s", "f", 1)), "e1");
        // Navigating an iframe ends only its refs
        let c = r.assign(entry("s", "child", 3));
        r.invalidate("s", Some("child"));
        assert!(!r.map[&c].valid && r.map[&a].valid);
        // The main frame navigated: everything in the tab goes, and the same node id gets a new number
        r.invalidate_tab("T");
        assert!(!r.map[&a].valid);
        assert_eq!(r.assign(entry("s", "f", 1)), "e4");
    }

    #[test]
    fn console_text() {
        assert_eq!(remote_text(&json!({"type": "string", "value": "hi"})), "hi");
        assert_eq!(remote_text(&json!({"type": "number", "value": 3})), "3");
        assert_eq!(remote_text(&json!({"type": "object", "description": "Error: x"})), "Error: x");
    }
}
