//! The Bot engine: keeps each enabled Bot's session alive, admits plugin events, delivers them to the Bot in merged
//! messages, runs the Bot's tools (spawning and steering tasks), relays task reports, and asks people for help.
//!
//! Everything here runs in the desktop app. Hosts only see ordinary sessions with a `BotLink`, an appended system
//! prompt and a client MCP server (`yi_bot` / `yi_task`) whose calls come back here through `clientMcp.call`.

use crate::config::{Accept, Ambient, BotConfig, Routine, AI_TOOLS};
use crate::plugins::cron::Cron;
use crate::plugins::builtin::{self, Builtin, APP_INSTANCE};
use crate::plugins::wasm::Wasm;
use crate::plugins::{self, package, Manifest, PluginHost, PluginInstance, PluginRuntime, SendResult, TriggerEvent};
use crate::prompt;
use crate::store::{ApprovalRow, Attention, BotRow, EventRow, Instance, OutboxRow, RoutineRun, SessionRef, Store, TaskRow};
use crate::tools::{self, BOT_SERVER, OFFLINE_MARK, TASK_SERVER};
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex, Weak};
use yi_client::{ClientHub, Identity};

/// kv namespace of the app's own plugin settings
const APP_KV: &str = "_app";
/// Attempts before an outgoing message is given up
const OUTBOX_ATTEMPTS: u32 = 8;
/// Drafts the owner doesn't decide on within this many hours are dropped
const APPROVAL_HOURS: i64 = 2;
/// A routine run still open after this long is closed as failed
const RUN_HOURS: i64 = 6;

/// Run a plugin call off the async threads: WASM instances answer through a channel and may do HTTP meanwhile
async fn blocking<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> Result<R> {
    tokio::task::spawn_blocking(f).await.map_err(|e| anyhow!("插件调用失败: {e}"))
}

pub fn now_ms() -> i64 {
    chrono::Local::now().timestamp_millis()
}

fn short_id(prefix: &str) -> String {
    format!("{prefix}_{}", &uuid::Uuid::new_v4().simple().to_string()[..10])
}

/// `UserMessage.origin` of what the engine sends to Bot and task sessions: the app shows it as a system note
const ENGINE_ORIGIN: &str = "bot-engine";

/// Characters of a task's text that go into a report card; the rest is read with task_status
const REPORT_CHARS: usize = 600;

/// A task's text for a report card: long text is cut, with a pointer to the full text
fn clip(s: &str) -> String {
    let n = s.chars().count();
    if n <= REPORT_CHARS {
        return s.to_string();
    }
    format!("{}\n[{n} characters in all; task_status gives the full text]", truncate(s, REPORT_CHARS))
}

/// The assistant text a session's history ends with: the text blocks after its last tool call, user message or
/// the end of the turn before
fn final_text(events: &[Value]) -> String {
    let mut parts = vec![];
    let mut ends = 0;
    for e in events.iter().rev() {
        match e["kind"].as_str() {
            Some("assistant_text") => parts.push(e["text"].as_str().unwrap_or("").trim().to_string()),
            Some("tool_call" | "tool_result" | "computer_use" | "user_message") => break,
            Some("turn_end") => {
                ends += 1;
                if ends > 1 {
                    break;
                }
            }
            _ => {}
        }
    }
    parts.reverse();
    parts.retain(|p| !p.is_empty());
    parts.join("\n\n")
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut t: String = s.chars().take(max).collect();
    t.push('…');
    t
}

/// Something waiting to be told to a Bot
#[derive(Clone)]
struct Inbox {
    text: String,
    /// Trigger events this item delivers (an event card)
    events: Vec<String>,
    /// false: a follow-up note that rides along with the next message instead of waking the Bot
    wake: bool,
    /// The owner's own words (the first message of a new Bot): the turn counts as the owner's
    owner: bool,
    /// The routine run this item starts
    run: Option<String>,
}

/// A configuration change the Bot asked for that needs the owner's confirmation in the app
#[derive(Clone)]
struct PendingChange {
    id: String,
    config: BotConfig,
    instances: Vec<Value>,
    summary: String,
    /// What makes it sensitive: "accept", "allow", "permission"
    items: Vec<String>,
}

/// The Bot asked the owner to enter (or replace) secrets of a plugin instance it uses
#[derive(Clone)]
struct SecretRequest {
    id: String,
    instance_id: String,
    fields: Vec<String>,
    reason: String,
}

#[derive(Default)]
struct BotRt {
    /// The Bot session's status as last seen (idle / running / exited …); None until known
    status: Option<String>,
    context: Option<f64>,
    inbox: Vec<Inbox>,
    /// ensure_session is running
    starting: bool,
    /// A message is being sent
    sending: bool,
    /// Why the session could not be started or reached
    error: Option<String>,
    /// Attach the notes to the next message (new session or after compaction)
    notes_due: bool,
    compacting: bool,
    /// Trigger events delivered in the current turn: a spawn_task without event ids during that turn is about them
    turn_events: Vec<String>,
    /// The engine started the current turn (events, reports), not the owner typing in the Bot session
    engine_turn: bool,
    pending: Option<PendingChange>,
    /// Secrets the Bot asked the owner for with request_secret (memory only); missing required secrets of the
    /// instances it uses need no request, their cards are worked out from the configuration
    secret_requests: Vec<SecretRequest>,
    /// Routine runs delivered in the current turn: closed as done when it ends without routine_done
    turn_runs: Vec<String>,
    /// Next scheduled run per routine id, with the schedule it was computed from
    routine_due: HashMap<String, (String, i64)>,
}

pub struct Engine {
    me: Weak<Engine>,
    hub: Weak<ClientHub>,
    pub store: Store,
    identity: Identity,
    /// The tokio runtime the engine was created in (plugin threads hop onto it)
    rt: tokio::runtime::Handle,
    runtimes: Vec<Arc<dyn PluginRuntime>>,
    /// Installed WASM plugins; None when the runtime could not start
    wasm: Option<Arc<Wasm>>,
    /// Packages read by `plugins.inspect`, waiting for the person to confirm
    staged: Mutex<HashMap<String, package::Staged>>,
    instances: Mutex<HashMap<String, Arc<dyn PluginInstance>>>,
    health: Mutex<HashMap<String, Value>>,
    bots: Mutex<HashMap<String, BotRt>>,
    /// Tasks whose host connection came back: offline calls are being caught up
    catching_up: Mutex<HashSet<String>>,
    /// Whether each task session was last seen busy (a turn, background work or queued messages); a busy session
    /// that settles while its task is still working ended its turn without a report
    task_busy: Mutex<HashMap<String, bool>>,
}

/// A plugin instance's view of the engine. WASM instances call it from their own threads, outside the tokio
/// runtime: anything async goes through `rt`
struct InstanceHost {
    engine: Weak<Engine>,
    id: String,
    config: Value,
    rt: tokio::runtime::Handle,
}

impl PluginHost for InstanceHost {
    fn instance_id(&self) -> &str {
        &self.id
    }
    fn config(&self) -> Value {
        self.config.clone()
    }
    fn emit(&self, event: TriggerEvent) {
        if let Some(e) = self.engine.upgrade() {
            let id = self.id.clone();
            self.rt.spawn(async move { e.ingest(&id, event).await });
        }
    }
    fn kv_get(&self, key: &str) -> Option<String> {
        self.engine.upgrade()?.store.kv_get(&self.id, key).ok().flatten()
    }
    fn kv_set(&self, key: &str, value: Option<&str>) {
        if let Some(e) = self.engine.upgrade() {
            let _ = e.store.kv_set(&self.id, key, value);
        }
    }
    fn log(&self, level: &str, message: &str) {
        eprintln!("[plugin {}] {level}: {message}", self.id);
    }
    fn health(&self, state: &str, detail: &str) {
        if let Some(e) = self.engine.upgrade() {
            let v = json!({"state": state, "detail": detail, "at": now_ms()});
            let same = e.health.lock().unwrap().get(&self.id).is_some_and(|o| o["state"] == v["state"] && o["detail"] == v["detail"]);
            e.health.lock().unwrap().insert(self.id.clone(), v);
            // Polling plugins report ok on every round: only announce real changes
            if !same {
                self.rt.spawn(async move { e.changed() });
            }
        }
    }
}

impl Engine {
    /// `plugins_dir`: where WASM plugins are installed (None: built-in plugins only); `bundled`: WASM plugins shipped
    /// with the app
    pub fn new(hub: &Arc<ClientHub>, store: Store, identity: Identity, plugins_dir: Option<&Path>, bundled: Option<&Path>) -> Arc<Engine> {
        let wasm = plugins_dir.and_then(|d| match Wasm::new(d, bundled) {
            Ok(w) => Some(w),
            Err(e) => {
                eprintln!("WASM plugin runtime: {e:#}");
                None
            }
        });
        Arc::new_cyclic(|me: &Weak<Engine>| {
            let sink_hub = Arc::downgrade(hub);
            let app_notify: builtin::AppNotify = Arc::new(move |title: &str, body: &str| {
                if let Some(h) = sink_hub.upgrade() {
                    h.emit_event(json!({"type": "bots.notify", "title": title, "body": body}));
                }
            });
            Engine {
                me: me.clone(),
                hub: Arc::downgrade(hub),
                store,
                identity,
                rt: tokio::runtime::Handle::current(),
                runtimes: {
                    let mut r: Vec<Arc<dyn PluginRuntime>> = vec![Arc::new(Builtin { app_notify })];
                    if let Some(w) = &wasm {
                        r.push(w.clone());
                    }
                    r
                },
                wasm,
                staged: Mutex::new(HashMap::new()),
                instances: Mutex::new(HashMap::new()),
                health: Mutex::new(HashMap::new()),
                bots: Mutex::new(HashMap::new()),
                catching_up: Mutex::new(HashSet::new()),
                task_busy: Mutex::new(HashMap::new()),
            }
        })
    }

    #[cfg(test)]
    fn for_tests(store: Store) -> Arc<Engine> {
        Arc::new_cyclic(|me| Engine {
            me: me.clone(),
            hub: Weak::new(),
            store,
            identity: Identity { client_id: "test".into(), kind: "desktop".into(), name: "test".into() },
            rt: tokio::runtime::Handle::current(),
            runtimes: vec![],
            wasm: None,
            staged: Mutex::new(HashMap::new()),
            instances: Mutex::new(HashMap::new()),
            health: Mutex::new(HashMap::new()),
            bots: Mutex::new(HashMap::new()),
            catching_up: Mutex::new(HashSet::new()),
            task_busy: Mutex::new(HashMap::new()),
        })
    }

    fn arc(&self) -> Arc<Engine> {
        self.me.upgrade().expect("engine alive")
    }

    fn hub(&self) -> Result<Arc<ClientHub>> {
        self.hub.upgrade().ok_or_else(|| anyhow!("App 正在退出"))
    }

    pub fn identity(&self) -> Identity {
        self.identity.clone()
    }

    pub fn client_id(&self) -> &str {
        &self.identity.client_id
    }

    /// Tell the frontend to refetch
    fn changed(&self) {
        if let Ok(h) = self.hub() {
            h.emit_event(json!({"type": "bots.changed"}));
        }
    }

    /// Call a host, connecting first if needed
    async fn rpc(&self, host: &str, method: &str, params: Value) -> Result<Value> {
        let hub = self.hub()?;
        if !hub.connected(host) {
            hub.call("hosts.connect", json!({"id": host})).await?;
        }
        hub.host_rpc(host, method, params).await
    }

    pub fn manifests(&self) -> Vec<Manifest> {
        self.runtimes.iter().flat_map(|r| r.manifests()).collect()
    }

    fn manifest(&self, plugin_id: &str) -> Option<Manifest> {
        self.manifests().into_iter().find(|m| m.id == plugin_id)
    }

    // ---------- startup ----------

    /// Start plugin instances and the enabled Bots
    pub fn start(&self) {
        for i in self.store.instances().unwrap_or_default() {
            if i.enabled {
                self.start_instance(&i);
            }
        }
        // The app notification always exists
        let app = Instance {
            id: APP_INSTANCE.into(),
            plugin_id: builtin::APP_NOTIFY.into(),
            name: "App".into(),
            config: json!({}),
            enabled: true,
        };
        self.start_instance(&app);
        for mut r in self.store.open_runs().unwrap_or_default() {
            r.state = "failed".into();
            r.summary = "Interrupted: the app was closed".into();
            r.ended_at = Some(now_ms());
            let _ = self.store.save_run(&r);
        }
        for b in self.store.bots().unwrap_or_default() {
            if b.config.enabled {
                let me = self.arc();
                tokio::spawn(async move {
                    let _ = me.ensure_session(&b.config.id).await;
                });
            }
        }
        // Housekeeping: timeouts, queued tasks, retries
        let weak = self.me.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                let Some(e) = weak.upgrade() else { return };
                e.tick().await;
            }
        });
    }

    /// Plugins switched off as a whole (their instances don't run)
    fn disabled_plugins(&self) -> Vec<String> {
        self.store.kv_get(APP_KV, "disabledPlugins").ok().flatten().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
    }

    fn start_instance(&self, i: &Instance) {
        self.stop_instance(&i.id);
        if self.disabled_plugins().contains(&i.plugin_id) {
            return;
        }
        let Some(m) = self.manifest(&i.plugin_id) else {
            self.health.lock().unwrap().insert(i.id.clone(), json!({"state": "error", "detail": "插件不存在"}));
            return;
        };
        let Some(rt) = self.runtimes.iter().find(|r| r.kind() == m.runtime).cloned() else { return };
        let host = Arc::new(InstanceHost { engine: self.me.clone(), id: i.id.clone(), config: i.config.clone(), rt: self.rt.clone() });
        match rt.start(&i.plugin_id, host) {
            Ok(inst) => {
                self.instances.lock().unwrap().insert(i.id.clone(), inst);
            }
            Err(e) => {
                self.health.lock().unwrap().insert(i.id.clone(), json!({"state": "error", "detail": format!("{e:#}"), "at": now_ms()}));
            }
        }
    }

    fn stop_instance(&self, id: &str) {
        if let Some(old) = self.instances.lock().unwrap().remove(id) {
            old.stop();
        }
        self.health.lock().unwrap().remove(id);
    }

    fn instance(&self, id: &str) -> Option<Arc<dyn PluginInstance>> {
        self.instances.lock().unwrap().get(id).cloned()
    }

    // ---------- the Bot session ----------

    fn link(&self, c: &BotConfig, role: &str, task_id: Option<&str>) -> Value {
        json!({"botId": c.id, "botName": c.name, "role": role, "taskId": task_id, "clientId": self.identity.client_id})
    }

    fn bot_prompt(&self, c: &BotConfig) -> String {
        let instances = self.store.instances().unwrap_or_default();
        let manifests = self.manifests();
        let notes = |plugin: &str| {
            let m = manifests.iter().find(|m| m.id == plugin)?;
            let mut n = m.prompt.clone().unwrap_or_default();
            if m.has("tools") {
                n.push_str("\nThis source has tools: list them with plugin_tools, run them with plugin_call.");
            }
            if m.has("reply") {
                n.push_str("\nYou can answer in its conversations with reply.");
            }
            Some(n.trim().to_string()).filter(|n| !n.is_empty())
        };
        let names = self.host_names();
        let host = |id: &str| names.get(id).cloned().unwrap_or_else(|| id.to_string());
        prompt::bot_prompt(c, &instances, &notes, &host)
    }

    fn host_names(&self) -> HashMap<String, String> {
        self.hub().map(|h| h.host_names()).unwrap_or_default()
    }

    /// Make sure the Bot has a live session with the current prompt and tools; returns where it is
    pub async fn ensure_session(&self, bot_id: &str) -> Result<SessionRef> {
        {
            let mut bots = self.bots.lock().unwrap();
            let rt = bots.entry(bot_id.into()).or_default();
            if rt.starting {
                bail!("Bot 会话正在启动");
            }
            rt.starting = true;
        }
        let r = self.ensure_session_inner(bot_id).await;
        {
            let mut bots = self.bots.lock().unwrap();
            let rt = bots.entry(bot_id.into()).or_default();
            rt.starting = false;
            rt.error = r.as_ref().err().map(|e| format!("{e:#}"));
        }
        self.changed();
        if r.is_ok() {
            self.deliver(bot_id).await;
        }
        r
    }

    async fn ensure_session_inner(&self, bot_id: &str) -> Result<SessionRef> {
        let row = self.store.bot(bot_id)?;
        let c = &row.config;
        let mcp = json!([tools::client_mcp(BOT_SERVER, &self.identity.client_id)]);
        let system = self.bot_prompt(c);
        if let Some(s) = row.session.as_ref().filter(|s| s.host == c.bot.host) {
            let p = |extra: Value| {
                let mut v = json!({"sessionId": s.session_id});
                v.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
                v
            };
            match self.rpc(&s.host, "session.setBot", p(json!({"bot": self.link(c, "bot", None)}))).await {
                Ok(info) => {
                    self.rpc(&s.host, "session.setClientMcp", p(json!({"clientMcp": mcp}))).await?;
                    let info = self.rpc(&s.host, "session.setSystemPrompt", p(json!({"prompt": system}))).await.unwrap_or(info);
                    self.note_status(bot_id, &info);
                    return Ok(s.clone());
                }
                Err(e) if format!("{e:#}").contains("会话不存在") => {}
                Err(e) => return Err(e),
            }
        }
        // A new session
        let cwd = if c.bot.cwd.is_empty() { format!("~/.yi-console/bots/{}", c.id) } else { c.bot.cwd.clone() };
        if c.bot.cwd.is_empty() {
            // Already there is fine
            let _ = self.rpc(&c.bot.host, "fs.mkdir", json!({"path": cwd})).await;
        }
        let info = self
            .rpc(
                &c.bot.host,
                "session.create",
                json!({
                    "tool": c.bot.tool,
                    "cwd": cwd,
                    "title": format!("Bot · {}", c.name),
                    "model": c.bot.model,
                    "effort": c.bot.effort,
                    "permissionMode": c.bot.permission_mode,
                    "appendSystemPrompt": system,
                    "clientMcp": mcp,
                    "bot": self.link(c, "bot", None),
                }),
            )
            .await?;
        let sid = info["id"].as_str().ok_or_else(|| anyhow!("创建 Bot 会话失败"))?.to_string();
        let s = SessionRef { host: c.bot.host.clone(), session_id: sid };
        self.store.set_bot_session(bot_id, Some(&s))?;
        self.note_status(bot_id, &info);
        if !row.notes.trim().is_empty() {
            self.bots.lock().unwrap().entry(bot_id.into()).or_default().notes_due = true;
        }
        Ok(s)
    }

    fn note_status(&self, bot_id: &str, info: &Value) {
        let mut bots = self.bots.lock().unwrap();
        let rt = bots.entry(bot_id.into()).or_default();
        if let Some(s) = info["status"].as_str() {
            rt.status = Some(s.to_string());
        }
        rt.context = info["contextPercent"].as_f64();
    }

    fn push(&self, bot_id: &str, text: String, wake: bool) {
        self.push_item(bot_id, Inbox { text, events: vec![], wake, owner: false, run: None });
    }

    fn push_item(&self, bot_id: &str, item: Inbox) {
        self.bots.lock().unwrap().entry(bot_id.into()).or_default().inbox.push(item);
        let me = self.arc();
        let id = bot_id.to_string();
        tokio::spawn(async move { me.deliver(&id).await });
    }

    /// Start (or refresh) a Bot session in the background. Boxed with an explicit `Send`: ensure_session and deliver
    /// reach each other, and the compiler can't prove a recursive future `Send` on its own
    fn spawn_ensure(&self, bot_id: &str) {
        let me = self.arc();
        let id = bot_id.to_string();
        let fut: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> = Box::pin(async move {
            let _ = me.ensure_session(&id).await;
        });
        tokio::spawn(fut);
    }

    /// Send what is waiting to an idle Bot session, merged into one message
    async fn deliver(&self, bot_id: &str) {
        let Ok(row) = self.store.bot(bot_id) else { return };
        if !row.config.enabled {
            return;
        }
        let Some(s) = row.session.clone() else {
            if self.bots.lock().unwrap().get(bot_id).is_some_and(|r| r.inbox.iter().any(|i| i.wake) && !r.starting) {
                self.spawn_ensure(bot_id);
            }
            return;
        };
        let (items, notes_due) = {
            let mut bots = self.bots.lock().unwrap();
            let rt = bots.entry(bot_id.into()).or_default();
            let idle = matches!(rt.status.as_deref(), Some("idle" | "exited") | None);
            if !idle || rt.starting || rt.sending || rt.compacting || !rt.inbox.iter().any(|i| i.wake) {
                return;
            }
            rt.sending = true;
            rt.engine_turn = !items_are_owner(&rt.inbox);
            (std::mem::take(&mut rt.inbox), std::mem::take(&mut rt.notes_due))
        };
        let mut text = String::new();
        if notes_due && !row.notes.trim().is_empty() {
            text.push_str(&format!("<notes>\nYour notes from before:\n{}\n</notes>\n\n", row.notes.trim()));
        }
        for i in &items {
            text.push_str(&i.text);
            text.push_str("\n\n");
        }
        // Only the owner's own words (a new Bot's role) show as theirs; anything the engine adds is a system note
        let theirs = text.is_empty() || items.iter().all(|i| i.owner && !i.text.starts_with('<'));
        let theirs = theirs && !text.starts_with("<notes>");
        let mut msg = json!({"sessionId": s.session_id, "text": text.trim_end()});
        if !theirs {
            msg["origin"] = json!(ENGINE_ORIGIN);
        }
        let r = self.rpc(&s.host, "session.send", msg).await;
        let mut bots = self.bots.lock().unwrap();
        let rt = bots.entry(bot_id.into()).or_default();
        rt.sending = false;
        match r {
            Ok(_) => {
                rt.status = Some("running".into());
                rt.error = None;
                rt.turn_events = items.iter().flat_map(|i| i.events.iter().cloned()).collect();
                rt.turn_runs = items.iter().filter_map(|i| i.run.clone()).collect();
                for id in &rt.turn_runs {
                    if let Ok(mut r) = self.store.run(id) {
                        r.state = "running".into();
                        let _ = self.store.save_run(&r);
                    }
                }
            }
            Err(e) => {
                // Put it back in front; the tick or the next status change retries
                let mut back = items;
                back.append(&mut rt.inbox);
                rt.inbox = back;
                rt.notes_due |= notes_due;
                let msg = format!("{e:#}");
                if msg.contains("会话不存在") {
                    drop(bots);
                    let _ = self.store.set_bot_session(bot_id, None);
                    return;
                }
                rt.error = Some(msg);
            }
        }
    }

    // ---------- host notifications ----------

    pub fn observe(&self, host: &str, method: &str, params: &Value) {
        let instances: Vec<_> = self.instances.lock().unwrap().values().cloned().collect();
        for inst in instances {
            inst.on_host_event(host, method, params);
        }
        match method {
            "session.updated" => {
                let link = &params["bot"];
                if link["clientId"].as_str() != Some(self.client_id()) {
                    return;
                }
                let bot_id = link["botId"].as_str().unwrap_or("").to_string();
                match link["role"].as_str() {
                    Some("bot") => self.bot_updated(&bot_id, params),
                    Some("task") => {
                        if let Some(t) = link["taskId"].as_str() {
                            self.task_updated(t, params);
                        }
                    }
                    _ => {}
                }
            }
            "session.removed" => {
                let sid = params["sessionId"].as_str().unwrap_or("");
                for b in self.store.bots().unwrap_or_default() {
                    if b.session.as_ref().is_some_and(|s| s.host == host && s.session_id == sid) {
                        let _ = self.store.set_bot_session(&b.config.id, None);
                        self.bots.lock().unwrap().entry(b.config.id.clone()).or_default().status = None;
                        self.changed();
                    }
                }
                for mut t in self.store.active_tasks().unwrap_or_default() {
                    if t.host == host && t.session_id.as_deref() == Some(sid) {
                        t.state = "stopped".into();
                        t.hold = true;
                        t.ended_at = Some(now_ms());
                        let _ = self.store.save_task(&t);
                        self.changed();
                    }
                }
            }
            _ => {}
        }
    }

    fn bot_updated(&self, bot_id: &str, info: &Value) {
        let status = info["status"].as_str().unwrap_or("").to_string();
        let compact_at = self.store.bot(bot_id).map(|b| b.config.compact_at).unwrap_or(70) as f64;
        let mut ended_runs = vec![];
        let compact = {
            let mut bots = self.bots.lock().unwrap();
            let rt = bots.entry(bot_id.into()).or_default();
            let was_compacting = rt.compacting;
            rt.status = Some(status.clone());
            rt.context = info["contextPercent"].as_f64();
            let idle = status == "idle";
            if idle && !rt.sending {
                rt.turn_events.clear();
                rt.engine_turn = false;
                ended_runs = std::mem::take(&mut rt.turn_runs);
            }
            if was_compacting && idle {
                // The compaction turn ended: the notes go with the next message
                rt.compacting = false;
                rt.notes_due = true;
            }
            let over = rt.context.is_some_and(|c| c >= compact_at);
            if idle && over && !rt.compacting {
                rt.compacting = true;
                true
            } else {
                false
            }
        };
        // The turn ended without routine_done: the run counts as done
        for id in ended_runs {
            if let Ok(mut r) = self.store.run(&id) {
                if r.state == "running" {
                    r.state = "done".into();
                    r.ended_at = Some(now_ms());
                    let _ = self.store.save_run(&r);
                }
            }
        }
        self.count_tokens(bot_id, info);
        self.changed();
        let me = self.arc();
        let id = bot_id.to_string();
        let session = self.store.bot(bot_id).ok().and_then(|b| b.session);
        tokio::spawn(async move {
            if compact {
                if let Some(s) = session {
                    if me.rpc(&s.host, "session.compact", json!({"sessionId": s.session_id})).await.is_err() {
                        me.bots.lock().unwrap().entry(id.clone()).or_default().compacting = false;
                    }
                }
                return;
            }
            me.deliver(&id).await;
        });
    }

    fn task_updated(&self, task_id: &str, info: &Value) {
        let Ok(mut t) = self.store.task(task_id) else { return };
        self.count_tokens(&t.bot_id, info);
        let pending = &info["pending"];
        let mut changed = false;
        if t.state == "working" && !pending.is_null() {
            t.state = "needsYou".into();
            changed = true;
            let title = pending["title"].as_str().unwrap_or("").to_string();
            let text = if pending["question"] == true {
                format!("Task \"{}\" has a question: {title}", t.title)
            } else {
                format!("Task \"{}\" is waiting for approval: {title}", t.title)
            };
            self.attention(&t.bot_id, Some(&t.id), &text, "normal");
        } else if t.state == "needsYou" && pending.is_null() && t.last_report.as_ref().map_or(true, |r| r["state"] != "blocked") {
            t.state = "working".into();
            changed = true;
            let _ = self.store.resolve_attention(None, Some(&t.id), now_ms());
        }
        if changed {
            let _ = self.store.save_task(&t);
            self.changed();
        }
        // A turn ends once nothing runs for the session any more: Claude's background shells and subagents wake it
        // again when they finish, and queued messages start the next turn
        let status = info["status"].as_str().unwrap_or("");
        let has = |k: &str| info[k].as_array().is_some_and(|a| !a.is_empty());
        let busy = !matches!(status, "idle" | "exited" | "error") || has("backgroundTasks") || has("queued");
        let was = self.task_busy.lock().unwrap().insert(t.id.clone(), busy);
        if was == Some(true) && !busy && t.state == "working" && !t.manual && !t.hold {
            let me = self.arc();
            let (id, error) = (t.id.clone(), status == "error");
            tokio::spawn(async move {
                if let Err(e) = me.turn_ended(&id, error).await {
                    eprintln!("turn end of {id}: {e:#}");
                }
            });
        }
    }

    /// A task's turn ended while it was still working, so without a ready / blocked / failed / dropped report: the
    /// Bot gets its last words as a `waiting` report instead of finding out at the timeout
    async fn turn_ended(&self, task_id: &str, error: bool) -> Result<()> {
        let t = self.store.task(task_id)?;
        let sid = t.session_id.clone().unwrap_or_default();
        let r = self.rpc(&t.host, "session.history", json!({"sessionId": sid, "limit": 200})).await?;
        let text = final_text(r["events"].as_array().map(Vec::as_slice).unwrap_or_default());
        // A report may have come in meanwhile
        let mut t = self.store.task(task_id)?;
        if t.state != "working" || t.manual || t.hold {
            return Ok(());
        }
        let ended = if error { "ended with an error" } else { "ended" };
        let body = if text.is_empty() {
            format!("The task's turn {ended} without a report and without a final message. Ask it for a status report with send_task, or stop it.")
        } else {
            format!("The task's turn {ended} without a report. Its last message:\n{}", clip(&text))
        };
        t.last_report = Some(json!({"state": "waiting", "summary": text, "auto": true, "at": now_ms()}));
        self.store.save_task(&t)?;
        self.push(&t.bot_id, format!("<report task=\"{}\" title=\"{}\" state=\"waiting\" auto=\"true\">\n{body}\n</report>", t.id, attr(&t.title)), true);
        self.changed();
        Ok(())
    }

    pub fn host_status(&self, host: &str, status: &Value) {
        if status["status"] != "connected" {
            return;
        }
        let me = self.arc();
        let host = host.to_string();
        tokio::spawn(async move {
            for b in me.store.bots().unwrap_or_default() {
                if b.config.enabled && b.config.bot.host == host {
                    let needs = b.session.is_none() || me.bots.lock().unwrap().get(&b.config.id).map_or(true, |r| r.status.is_none());
                    if needs {
                        let _ = me.ensure_session(&b.config.id).await;
                    } else {
                        me.deliver(&b.config.id).await;
                    }
                }
            }
            for t in me.store.active_tasks().unwrap_or_default() {
                if t.host == host && t.session_id.is_some() {
                    me.catch_up(&t.id).await;
                }
            }
            me.start_queued_for_host("", &host).await;
        });
    }

    /// Replay `yi_task` calls a task made while this app was offline: their results carry `OFFLINE_MARK`
    async fn catch_up(&self, task_id: &str) {
        if !self.catching_up.lock().unwrap().insert(task_id.into()) {
            return;
        }
        let r = self.catch_up_inner(task_id).await;
        self.catching_up.lock().unwrap().remove(task_id);
        if let Err(e) = r {
            eprintln!("catch up {task_id}: {e:#}");
        }
    }

    async fn catch_up_inner(&self, task_id: &str) -> Result<()> {
        let t = self.store.task(task_id)?;
        let sid = t.session_id.clone().unwrap_or_default();
        let r = self.rpc(&t.host, "session.history", json!({"sessionId": sid, "limit": 1000})).await?;
        let events = r["events"].as_array().cloned().unwrap_or_default();
        let mut calls: HashMap<String, (String, Value)> = HashMap::new();
        let mut max = t.seen_seq;
        for e in &events {
            let seq = e["seq"].as_i64().unwrap_or(0);
            match e["kind"].as_str() {
                Some("tool_call") => {
                    calls.insert(e["id"].as_str().unwrap_or("").into(), (e["name"].as_str().unwrap_or("").into(), e["input"].clone()));
                }
                Some("tool_result") if seq > t.seen_seq => {
                    max = max.max(seq);
                    if !e["output"].as_str().unwrap_or("").contains(OFFLINE_MARK) {
                        continue;
                    }
                    let Some((name, input)) = calls.get(e["id"].as_str().unwrap_or("")) else { continue };
                    if let Some((TASK_SERVER, tool)) = tools::parse_call_name(name) {
                        let _ = self.task_tool(task_id, tool, input.clone()).await;
                    }
                }
                _ => max = max.max(seq),
            }
        }
        let mut t = self.store.task(task_id)?;
        t.seen_seq = max;
        self.store.save_task(&t)?;
        Ok(())
    }

    // ---------- events ----------

    pub async fn ingest(&self, instance_id: &str, ev: TriggerEvent) {
        let plugin = self.store.instances().unwrap_or_default().into_iter().find(|i| i.id == instance_id).map(|i| i.plugin_id);
        let system_source = matches!(plugin.as_deref(), Some(builtin::CRON | builtin::SESSION_EVENTS));
        for b in self.store.bots().unwrap_or_default() {
            let c = &b.config;
            if !c.enabled || !c.triggers.iter().any(|t| t == instance_id) {
                continue;
            }
            let mut row = EventRow {
                id: short_id("ev"),
                instance_id: instance_id.into(),
                bot_id: c.id.clone(),
                key: ev.key.clone(),
                thread: ev.thread.clone(),
                sender: ev.sender.id.clone(),
                sender_name: ev.sender.name.clone().unwrap_or_default(),
                sender_bot: ev.sender.bot,
                addressed: ev.addressed,
                text: ev.text.clone(),
                url: ev.url.clone(),
                meta: ev.meta.clone(),
                at: if ev.at > 0 { ev.at } else { now_ms() },
                disposition: "queued".into(),
                task_id: None,
                note: String::new(),
            };
            // A follow-up in a conversation a task is working on goes to that task
            let owner = self
                .store
                .active_tasks()
                .unwrap_or_default()
                .into_iter()
                .find(|t| t.bot_id == c.id && t.threads.contains(&ev.thread));
            let explicit = ev.addressed && !ev.sender.bot && (system_source || c.allowed(instance_id, &ev.sender.id)); // same rule as Engine::explicit
            if let Some(t) = owner {
                row.disposition = "attached".into();
                row.task_id = Some(t.id.clone());
                if !self.store.add_event(&row).unwrap_or(false) {
                    continue;
                }
                if !t.manual && t.session_id.is_some() {
                    let _ = self
                        .rpc(&t.host, "session.send", json!({"sessionId": t.session_id, "text": follow_up(&row), "queue": true, "origin": ENGINE_ORIGIN}))
                        .await;
                }
                self.push(&c.id, format!("<follow-up event=\"{}\" task=\"{}\">went to the task working on this conversation</follow-up>", row.id, t.id), false);
                continue;
            }
            // Bot accounts never start work by themselves
            let filtered = if c.accept == Accept::Off {
                Some("accept off")
            } else if ev.sender.bot {
                Some("bot sender")
            } else if c.ambient == Ambient::Record && !system_source && !ev.addressed && !c.allowed(instance_id, &ev.sender.id) {
                // Chatter nobody addressed to the Bot doesn't wake it; it stays readable through read_thread
                Some("ambient: not addressed, sender not allowed")
            } else {
                None
            };
            if let Some(why) = filtered {
                row.disposition = "filtered".into();
                row.note = why.into();
            } else if !system_source && !in_active_hours(c) {
                // Waits for the hours to start (tick releases it); the owner's own sources never wait
                row.disposition = "held".into();
                row.note = "outside active hours".into();
            }
            if !self.store.add_event(&row).unwrap_or(false) || row.disposition != "queued" {
                self.changed();
                continue;
            }
            self.push_item(&c.id, Inbox { text: card(&row, explicit, &self.instance_label(instance_id)), events: vec![row.id.clone()], wake: true, owner: false, run: None });
            self.changed();
        }
    }

    /// Whether an event is an explicit request: addressed to the Bot by a person who is on the allow list, or from a
    /// built-in source (the owner's own schedule, the owner's sessions)
    fn explicit(&self, c: &BotConfig, e: &EventRow) -> bool {
        let plugin = self.store.instances().unwrap_or_default().into_iter().find(|i| i.id == e.instance_id).map(|i| i.plugin_id);
        let system_source = matches!(plugin.as_deref(), Some(builtin::CRON | builtin::SESSION_EVENTS));
        e.addressed && !e.sender_bot && (system_source || c.allowed(&e.instance_id, &e.sender))
    }

    fn instance_label(&self, id: &str) -> String {
        self.store
            .instances()
            .unwrap_or_default()
            .into_iter()
            .find(|i| i.id == id)
            .map(|i| format!("{} ({})", i.id, i.plugin_id))
            .unwrap_or_else(|| id.to_string())
    }

    // ---------- attention ----------

    fn attention(&self, bot_id: &str, task_id: Option<&str>, text: &str, urgency: &str) {
        let a = Attention {
            id: short_id("at"),
            bot_id: bot_id.into(),
            task_id: task_id.map(str::to_string),
            text: truncate(text, 500),
            urgency: urgency.into(),
            at: now_ms(),
        };
        let _ = self.store.add_attention(&a);
        self.notify_owner(bot_id, &a.id, &a.text);
        if let Ok(h) = self.hub() {
            h.emit_event(json!({"type": "bots.attention", "botId": bot_id, "taskId": task_id, "text": a.text}));
        }
        self.changed();
    }

    /// Send a line to the Bot's notify channels; `key` makes retries idempotent
    fn notify_owner(&self, bot_id: &str, key: &str, text: &str) {
        let Ok(b) = self.store.bot(bot_id) else { return };
        for target in &b.config.notify {
            let o = OutboxRow {
                key: format!("notify:{key}:{target}"),
                instance_id: target.clone(),
                kind: "notify".into(),
                bot_id: Some(bot_id.into()),
                text: format!("{}\n{text}", b.config.name),
                ..Default::default()
            };
            let me = self.arc();
            tokio::spawn(async move {
                me.send_out(o).await;
            });
        }
    }

    // ---------- cost ----------

    /// Count what a Bot or task session used since it was last seen toward the Bot's day, and remind the owner once
    /// a day when it passes `budget.tokensPerDay`. Kept in plugin_kv under `bot:<id>`
    fn count_tokens(&self, bot_id: &str, info: &Value) {
        let sid = info["id"].as_str().unwrap_or("");
        if sid.is_empty() || info["usage"].is_null() {
            return;
        }
        let total = session_tokens(&info["usage"]);
        let ns = format!("bot:{bot_id}");
        let key = format!("seen:{sid}");
        let seen = self.store.kv_get(&ns, &key).ok().flatten().and_then(|v| v.parse::<u64>().ok());
        if seen == Some(total) {
            return;
        }
        let delta = match seen {
            Some(s) if total >= s => total - s,
            // Usage was reset (/clear)
            Some(_) => total,
            None if info["createdAt"].as_i64().unwrap_or(0) >= local_midnight(0) => total,
            // An older session seen for the first time: count from here
            None => 0,
        };
        let _ = self.store.kv_set(&ns, &key, Some(&total.to_string()));
        if delta == 0 {
            return;
        }
        let day = local_day(now_ms());
        let sum = self.tokens_on(bot_id, &day) + delta;
        let _ = self.store.kv_set(&ns, &format!("tokens:{day}"), Some(&sum.to_string()));
        let Ok(b) = self.store.bot(bot_id) else { return };
        let limit = b.config.budget.tokens_per_day;
        if limit > 0 && sum >= limit && self.store.kv_get(&ns, "reminded").ok().flatten().as_deref() != Some(day.as_str()) {
            let _ = self.store.kv_set(&ns, "reminded", Some(&day));
            self.attention(
                bot_id,
                None,
                &format!("Used {} tokens today, past the daily reminder of {}. Nothing was paused.", fmt_tokens(sum), fmt_tokens(limit)),
                "low",
            );
        }
    }

    fn tokens_on(&self, bot_id: &str, day: &str) -> u64 {
        self.store.kv_get(&format!("bot:{bot_id}"), &format!("tokens:{day}")).ok().flatten().and_then(|v| v.parse().ok()).unwrap_or(0)
    }

    // ---------- routines ----------

    /// Start the routines whose time has come. Times are worked out from now, so runs missed while the app was
    /// closed are skipped
    fn schedule_routines(&self, c: &BotConfig) {
        let now = chrono::Local::now();
        let mut due = vec![];
        {
            let mut bots = self.bots.lock().unwrap();
            let rt = bots.entry(c.id.clone()).or_default();
            rt.routine_due.retain(|id, _| c.routines.iter().any(|r| &r.id == id && r.enabled));
            for r in c.routines.iter().filter(|r| r.enabled) {
                let Ok(cron) = Cron::parse(&r.schedule) else { continue };
                let next = || cron.next(now).map(|t| t.timestamp_millis()).unwrap_or(i64::MAX);
                match rt.routine_due.get(&r.id) {
                    Some((schedule, at)) if *schedule == r.schedule => {
                        if now.timestamp_millis() >= *at {
                            due.push(r.clone());
                            rt.routine_due.insert(r.id.clone(), (r.schedule.clone(), next()));
                        }
                    }
                    _ => {
                        rt.routine_due.insert(r.id.clone(), (r.schedule.clone(), next()));
                    }
                }
            }
        }
        for r in due {
            let _ = self.start_routine(c, &r, false);
        }
    }

    /// One run: recorded, then handed to the Bot like any other message. A run while the previous one is still open
    /// is skipped (scheduled) or refused (manual)
    fn start_routine(&self, c: &BotConfig, r: &Routine, manual: bool) -> Result<RoutineRun> {
        let now = now_ms();
        let mut run = RoutineRun {
            id: short_id("run"),
            bot_id: c.id.clone(),
            routine_id: r.id.clone(),
            name: r.name.clone(),
            state: "queued".into(),
            manual,
            started_at: now,
            ..Default::default()
        };
        let busy = self.store.runs(&c.id, Some(&r.id), 1)?.into_iter().any(|x| matches!(x.state.as_str(), "queued" | "running"));
        if busy {
            if manual {
                bail!("这个例行任务上一次还没结束");
            }
            run.state = "skipped".into();
            run.summary = "The previous run had not finished".into();
            run.ended_at = Some(now);
            self.store.save_run(&run)?;
            self.changed();
            return Ok(run);
        }
        self.store.save_run(&run)?;
        let text = format!(
            "<routine run=\"{}\" name=\"{}\"{}>\n{}\n</routine>",
            run.id,
            attr(&r.name),
            if manual { " manual=\"true\"" } else { "" },
            r.prompt.trim()
        );
        self.push_item(&c.id, Inbox { text, events: vec![], wake: true, owner: false, run: Some(run.id.clone()) });
        self.changed();
        Ok(run)
    }

    // ---------- approvals ----------

    /// The engine (not the owner) started the Bot's current turn
    fn engine_turn(&self, bot_id: &str) -> bool {
        self.bots.lock().unwrap().get(bot_id).is_some_and(|r| r.engine_turn)
    }

    /// Keep an outgoing action for the owner; the Bot's tool call returns at once
    fn draft(&self, c: &BotConfig, mut a: ApprovalRow) -> Result<Value> {
        a.id = short_id("ap");
        a.bot_id = c.id.clone();
        a.state = "pending".into();
        a.created_at = now_ms();
        a.source = self
            .store
            .instances()
            .ok()
            .and_then(|l| l.into_iter().find(|i| i.id == a.instance_id))
            .map(|i| i.name)
            .unwrap_or_else(|| a.instance_id.clone());
        if let Some(e) = a.event_id.as_deref().and_then(|id| self.store.event(id).ok()) {
            a.quote_from = if e.sender_name.is_empty() { e.sender.clone() } else { e.sender_name.clone() };
            a.quote = truncate(e.text.trim(), 400);
        }
        self.store.save_approval(&a)?;
        let what = if a.kind == "reply" { "Reply waiting for your approval" } else { "Action waiting for your approval" };
        self.notify_owner(&c.id, &a.id, &format!("{what}: {}", truncate(&a.text, 200)));
        if let Ok(h) = self.hub() {
            h.emit_event(json!({"type": "bots.approval", "botId": c.id, "name": c.name, "id": a.id, "kind": a.kind, "text": truncate(&a.text, 200)}));
        }
        self.changed();
        Ok(json!(format!(
            "Drafted for the owner's approval (id {}); nothing was carried out yet. Don't retry or rephrase it: the \
             outcome arrives as a message. Undecided drafts are dropped after {APPROVAL_HOURS} hours.",
            a.id
        )))
    }

    /// The owner's decision on a draft: carry it out (a reply may come back edited) and tell the Bot
    async fn decide(&self, id: &str, approve: bool, text: Option<String>) -> Result<Value> {
        let mut ap = self.store.approval(id)?;
        if ap.state != "pending" {
            bail!("这条操作已经处理过了");
        }
        ap.decided_at = Some(now_ms());
        let what = if ap.kind == "reply" { "reply" } else { "action" };
        if !approve {
            ap.state = "rejected".into();
            self.store.save_approval(&ap)?;
            self.push(&ap.bot_id, format!("<approval id=\"{}\" state=\"rejected\" kind=\"{}\" text=\"{}\">The owner rejected your drafted {what} ({}); it was not carried out.</approval>", ap.id, ap.kind, attr(&truncate(&ap.text, 120)), truncate(&ap.text, 200)), false);
            return Ok(json!(ap));
        }
        let edited = ap.kind == "reply" && text.as_deref().map(str::trim).is_some_and(|t| !t.is_empty() && t != ap.text.trim());
        if edited {
            ap.text = text.unwrap_or_default().trim().to_string();
        }
        ap.state = "approved".into();
        // Saved before carrying it out, so a second click can't run it twice
        self.store.save_approval(&ap)?;
        let result = if ap.kind == "reply" {
            let o = self
                .send_out(OutboxRow {
                    key: format!("reply:{}", ap.id),
                    instance_id: ap.instance_id.clone(),
                    kind: "reply".into(),
                    bot_id: Some(ap.bot_id.clone()),
                    thread: ap.thread.clone(),
                    reply_to: ap.reply_to.clone(),
                    text: ap.text.clone(),
                    ..Default::default()
                })
                .await;
            match o.state.as_str() {
                "sent" => "sent".to_string(),
                "gone" => "the conversation no longer accepts replies".to_string(),
                _ => format!("not sent yet ({}); it is retried automatically", o.error.unwrap_or_default()),
            }
        } else {
            match self.instance(&ap.instance_id) {
                None => "not run: the plugin instance is not running".to_string(),
                Some(inst) => {
                    let (name, input) = (ap.tool.clone().unwrap_or_default(), ap.input.clone());
                    match blocking(move || inst.call_tool(&name, &input)).await {
                        Ok(Ok(v)) => truncate(&v, 1500),
                        Ok(Err(e)) | Err(e) => format!("failed: {e:#}"),
                    }
                }
            }
        };
        ap.result = Some(result.clone());
        self.store.save_approval(&ap)?;
        self.push(
            &ap.bot_id,
            format!(
                "<approval id=\"{}\" state=\"approved\" kind=\"{}\" text=\"{}\">The owner approved your drafted {what}{} ({}). Result: {result}</approval>",
                ap.id,
                ap.kind,
                attr(&truncate(&ap.text, 120)),
                if edited { " after editing it" } else { "" },
                truncate(&ap.text, 200)
            ),
            true,
        );
        Ok(json!(ap))
    }

    // ---------- tools ----------

    /// A `clientMcp.call` relayed by a host
    pub async fn client_mcp(&self, host: &str, params: Value) -> Result<Value> {
        let sid = params["sessionId"].as_str().unwrap_or("");
        let tool = params["tool"].as_str().unwrap_or("");
        let args = params["arguments"].clone();
        let r = match params["server"].as_str() {
            Some(BOT_SERVER) => {
                let bot = self
                    .store
                    .bots()?
                    .into_iter()
                    .find(|b| b.session.as_ref().is_some_and(|s| s.host == host && s.session_id == sid))
                    .ok_or_else(|| anyhow!("这个会话不是 Bot 会话"))?;
                self.bot_tool(&bot, tool, args).await
            }
            Some(TASK_SERVER) => {
                let t = self
                    .store
                    .tasks(None, 10_000)?
                    .into_iter()
                    .find(|t| t.host == host && t.session_id.as_deref() == Some(sid))
                    .ok_or_else(|| anyhow!("这个会话不是任务"))?;
                self.task_tool(&t.id, tool, args).await
            }
            _ => Err(anyhow!("未知的 MCP: {}", params["server"])),
        };
        self.changed();
        Ok(match r {
            Ok(Value::String(s)) => json!({"content": [{"type": "text", "text": s}]}),
            Ok(v) => json!({"content": [{"type": "text", "text": serde_json::to_string_pretty(&v).unwrap_or_default()}]}),
            Err(e) => json!({"isError": true, "content": [{"type": "text", "text": format!("{e:#}")}]}),
        })
    }

    fn bot_event(&self, bot_id: &str, id: &str) -> Result<EventRow> {
        let e = self.store.event(id)?;
        if e.bot_id != bot_id {
            bail!("事件不存在: {id}");
        }
        Ok(e)
    }

    async fn bot_tool(&self, bot: &BotRow, tool: &str, a: Value) -> Result<Value> {
        let c = &bot.config;
        let s = |k: &str| a[k].as_str().unwrap_or("").to_string();
        match tool {
            "spawn_task" => {
                let mut event_ids: Vec<String> = serde_json::from_value(a["event_ids"].clone()).unwrap_or_default();
                // No ids while this turn delivers trigger events: it is about those events, not an owner request
                if event_ids.is_empty() {
                    event_ids = self.bots.lock().unwrap().get(&c.id).map(|r| r.turn_events.clone()).unwrap_or_default();
                }
                let events: Vec<EventRow> = event_ids.iter().map(|id| self.bot_event(&c.id, id)).collect::<Result<_>>()?;
                // Admission guards trigger events; the owner asking in the session directly is always carried out.
                // A task starts only in auto mode and only for explicit requests (addressed, a person, allowed sender)
                if !events.is_empty() && (c.accept != Accept::Auto || !events.iter().all(|e| self.explicit(c, e))) {
                    let why = if c.accept == Accept::Auto { "not an explicit request from an allowed sender" } else { "observe mode" };
                    for mut e in events {
                        e.disposition = "observed".into();
                        e.note = format!("would spawn task \"{}\" ({why})", s("title"));
                        let _ = self.store.update_event(&e);
                    }
                    return Ok(json!(format!(
                        "Not started ({why}): recorded what you would do; no task is running. Do not tell anyone a task is running."
                    )));
                }
                if c.budget.tasks_per_day > 0 {
                    if self.store.tasks_since(&c.id, local_midnight(0))? >= c.budget.tasks_per_day {
                        self.attention(&c.id, None, "今天的任务数已到上限", "normal");
                        bail!("Daily task budget used up ({}). Tell the owner if this is urgent.", c.budget.tasks_per_day);
                    }
                }
                let ws_index = a["workspace"].as_u64().map(|i| i as usize);
                let ws = c.workspace(ws_index).cloned().ok_or_else(|| anyhow!("No workspace is configured for this Bot."))?;
                let allowed = c.task_tools();
                let tool = match a["tool"].as_str().filter(|t| !t.is_empty()) {
                    Some(t) if allowed.contains(&t) => t.to_string(),
                    Some(t) => bail!("Tool \"{t}\" is not allowed for tasks; use one of: {}.", allowed.join(", ")),
                    None => c.task.tool.clone(),
                };
                let computer_use = a["computer_use"].as_bool().unwrap_or(false);
                if computer_use && !c.task.computer_use {
                    bail!("Computer use is not enabled for this Bot's tasks; ask the owner if they want `task.computerUse` turned on.");
                }
                // The configured model and effort belong to the default tool
                let default_tool = tool == c.task.tool;
                let mut t = TaskRow {
                    id: short_id("task"),
                    bot_id: c.id.clone(),
                    host: ws.host.clone(),
                    session_id: None,
                    state: "queued".into(),
                    title: truncate(&s("title"), 80),
                    event_ids: event_ids.clone(),
                    threads: events.iter().map(|e| e.thread.clone()).collect::<HashSet<_>>().into_iter().collect(),
                    cwd: String::new(),
                    workspace: ws.cwd.clone(),
                    worktree: None,
                    spec: json!({
                        "tool": tool,
                        "model": a["model"].as_str().filter(|m| !m.is_empty()).map(str::to_string).or(if default_tool { c.task.model.clone() } else { None }),
                        "effort": if default_tool { c.task.effort.clone() } else { None },
                        "worktree": a["worktree"].as_bool().unwrap_or(c.task.worktree),
                        "computerUse": computer_use,
                        "note": truncate(&s("note"), 400),
                        "request": s("request"),
                    }),
                    created_at: now_ms(),
                    ..Default::default()
                };
                // The requester's acknowledgement is queued with the task, so a crash can't lose it
                let ack_text = s("ack");
                let ack = events.first().filter(|_| !ack_text.trim().is_empty()).and_then(|e| self.ack_row(&t, e, ack_text.trim()));
                if let Some(o) = &ack {
                    t.spec["ack"] = json!(o.text);
                }
                let spawned: Vec<EventRow> = events
                    .into_iter()
                    .map(|mut e| {
                        e.disposition = "spawned".into();
                        e.task_id = Some(t.id.clone());
                        e
                    })
                    .collect();
                self.store.spawn(&t, &spawned, ack.as_ref())?;
                self.changed();
                if let Some(o) = ack {
                    self.attempt(o).await;
                }
                let state = self.try_start(&t.id).await?;
                Ok(json!({"taskId": t.id, "state": state}))
            }
            "attach" => {
                let mut e = self.bot_event(&c.id, &s("event_id"))?;
                let mut t = self.store.task(&s("task_id"))?;
                if t.bot_id != c.id {
                    bail!("任务不存在");
                }
                let pending_brief = t.spec["pendingBrief"].as_bool().unwrap_or(false);
                if !t.manual && !pending_brief && (t.active() || t.state == "ready") && t.session_id.is_some() {
                    self.check_task_desktop_ready(&t).await?;
                }
                e.disposition = "attached".into();
                e.task_id = Some(t.id.clone());
                self.store.update_event(&e)?;
                if !t.threads.contains(&e.thread) {
                    t.threads.push(e.thread.clone());
                }
                t.event_ids.push(e.id.clone());
                self.store.save_task(&t)?;
                if t.manual {
                    return Ok(json!("Recorded. The owner has taken over this task, so nothing was sent to it."));
                }
                if pending_brief {
                    // Included in the initial brief once the desktop is unlocked; try waking it now if already unlocked
                    if self.send_to_task(&mut t, String::new()).await.is_err() {
                        return Ok(json!("Recorded on the task; it will receive the event once the host's desktop is unlocked."));
                    }
                    return Ok(json!("ok"));
                }
                if t.active() || t.state == "ready" {
                    if t.session_id.is_some() {
                        self.send_to_task(&mut t, follow_up(&e)).await?;
                    }
                }
                Ok(json!("ok"))
            }
            "ignore" => {
                let ids: Vec<String> = serde_json::from_value(a["event_ids"].clone()).unwrap_or_default();
                for id in ids {
                    let mut e = self.bot_event(&c.id, &id)?;
                    e.disposition = "ignored".into();
                    e.note = truncate(&s("reason"), 300);
                    self.store.update_event(&e)?;
                }
                Ok(json!("ok"))
            }
            "send_task" => {
                let mut t = self.bot_task(&c.id, &s("task_id"))?;
                if t.manual {
                    bail!("The owner has taken over this task; send nothing until they hand it back.");
                }
                if !(t.active() || t.state == "ready") || t.session_id.is_none() {
                    bail!("The task has ended.");
                }
                self.send_to_task(&mut t, format!("<bot-message>\n{}\n</bot-message>", s("text"))).await?;
                Ok(json!("ok"))
            }
            "task_status" => Ok(task_view(&self.bot_task(&c.id, &s("task_id"))?)),
            "list_tasks" => {
                let active_only = a["active_only"].as_bool().unwrap_or(true);
                let list: Vec<Value> = self
                    .store
                    .tasks(Some(&c.id), 50)?
                    .iter()
                    .filter(|t| !active_only || t.active() || t.state == "ready")
                    .map(task_view)
                    .collect();
                Ok(json!(list))
            }
            "stop_task" => {
                self.stop_task(&s("task_id"), Some(&c.id)).await?;
                Ok(json!("stopped"))
            }
            "close_task" => {
                let mut t = self.bot_task(&c.id, &s("task_id"))?;
                let result = s("result");
                if !["done", "failed"].contains(&result.as_str()) {
                    bail!("result must be done or failed");
                }
                if !matches!(t.state.as_str(), "working" | "needsYou" | "ready") {
                    bail!("The task is {}; only a working, blocked or ready task can be closed.", t.state);
                }
                if t.manual {
                    bail!("The owner has taken over this task; leave it to them.");
                }
                if self.task_busy.lock().unwrap().get(&t.id).copied().unwrap_or(false) {
                    bail!("The task is still running; wait for its report, or stop it with stop_task.");
                }
                t.state = result.clone();
                t.ended_at = Some(now_ms());
                t.last_report = Some(json!({"state": result, "summary": s("summary"), "by": "bot", "at": now_ms()}));
                self.store.save_task(&t)?;
                self.store.resolve_attention(None, Some(&t.id), now_ms())?;
                self.release_task_desktop(&t).await;
                self.start_queued_for_host(&c.id, &t.host).await;
                self.changed();
                Ok(json!(format!("Closed as {result}.")))
            }
            "reply" => {
                let mut e = self.bot_event(&c.id, &s("event_id"))?;
                if c.accept != Accept::Auto {
                    e.note = format!("would reply: {}", truncate(&s("text"), 300));
                    self.store.update_event(&e)?;
                    return Ok(json!("Observe mode: the reply was recorded, not sent."));
                }
                if c.approval.reply && self.engine_turn(&c.id) {
                    e.note = format!("reply drafted for approval: {}", truncate(&s("text"), 300));
                    self.store.update_event(&e)?;
                    return self.draft(
                        c,
                        ApprovalRow {
                            kind: "reply".into(),
                            instance_id: e.instance_id.clone(),
                            event_id: Some(e.id.clone()),
                            thread: e.thread.clone(),
                            reply_to: Some(e.key.clone()),
                            text: s("text"),
                            ..Default::default()
                        },
                    );
                }
                let o = self
                    .send_out(OutboxRow {
                        key: format!("reply:{}:{:x}", e.id, now_ms()),
                        instance_id: e.instance_id.clone(),
                        kind: "reply".into(),
                        bot_id: Some(c.id.clone()),
                        thread: e.thread.clone(),
                        reply_to: Some(e.key.clone()),
                        text: s("text"),
                        ..Default::default()
                    })
                    .await;
                Ok(json!(match o.state.as_str() {
                    "sent" => "sent".to_string(),
                    "gone" => "The conversation no longer accepts replies.".to_string(),
                    _ => format!("Not sent yet ({}); it will be retried automatically.", o.error.unwrap_or_default()),
                }))
            }
            "notify_human" => {
                let urgency = a["urgency"].as_str().filter(|u| ["low", "normal", "high"].contains(u)).unwrap_or("normal");
                let task = a["task_id"].as_str().filter(|t| !t.is_empty());
                self.attention(&c.id, task, &s("text"), urgency);
                Ok(json!("The owner was notified."))
            }
            "read_event" => Ok(serde_json::to_value(self.bot_event(&c.id, &s("event_id"))?)?),
            "read_thread" => self.read_thread(&c.id, &s("event_id"), a["cursor"].as_str()).await,
            "plugin_tools" => {
                let mut out = vec![];
                for id in self.tool_instances(c) {
                    if let Some(inst) = self.instance(&id) {
                        let tools = blocking(move || inst.tools()).await?;
                        out.push(json!({"instance": id, "tools": tools}));
                    }
                }
                Ok(json!(out))
            }
            "plugin_call" => {
                let id = s("instance");
                if !self.tool_instances(c).contains(&id) {
                    bail!("This Bot has no plugin instance {id} with tools.");
                }
                let inst = self.instance(&id).ok_or_else(|| anyhow!("The plugin instance is not running."))?;
                let name = s("name");
                // Observe and off modes don't act on the outside world: only read-only tools run. In auto mode a
                // changing call from a turn the owner didn't start waits for approval when the Bot is set up so
                let drafted = c.accept == Accept::Auto && c.approval.write && self.engine_turn(&c.id);
                if c.accept != Accept::Auto || drafted {
                    let i2 = inst.clone();
                    let tools = blocking(move || i2.tools()).await?;
                    let read_only = tools.iter().any(|t| t["name"] == name.as_str() && t["readOnly"] == true);
                    if !read_only && drafted {
                        let what = a["summary"].as_str().map(str::trim).filter(|x| !x.is_empty()).map(str::to_string);
                        return self.draft(
                            c,
                            ApprovalRow {
                                kind: "tool".into(),
                                instance_id: id.clone(),
                                text: what.unwrap_or_else(|| format!("{id}: {name}")),
                                tool: Some(name.clone()),
                                input: a["input"].clone(),
                                ..Default::default()
                            },
                        );
                    }
                    if !read_only {
                        return Ok(json!(format!("Not run (observe mode): {name} changes things outside; only read-only tools run in this mode.")));
                    }
                }
                let input = a["input"].clone();
                Ok(json!(blocking(move || inst.call_tool(&name, &input)).await??))
            }
            "routine_done" => {
                let mut r = self.store.run(&s("run_id"))?;
                if r.bot_id != c.id {
                    bail!("No such routine run.");
                }
                r.state = a["result"].as_str().filter(|x| ["done", "nothing", "failed"].contains(x)).unwrap_or("done").into();
                r.summary = truncate(a["summary"].as_str().unwrap_or("").trim(), 1000);
                r.ended_at = Some(now_ms());
                self.store.save_run(&r)?;
                self.changed();
                Ok(json!("Recorded."))
            }
            "get_config" => Ok(json!({"config": self.store.bot(&c.id)?.config})),
            "request_secret" => {
                if self.engine_turn(&c.id) {
                    bail!("Only when the owner asks for it in this session directly: this turn was started by events or reports. Tell the owner instead.");
                }
                let iid = s("instance_id");
                if !bot_instances(c).contains(&iid) {
                    bail!("{iid} is not one of your plugin instances (triggers, notify or allow).");
                }
                let inst = self.store.instances()?.into_iter().find(|i| i.id == iid).ok_or_else(|| anyhow!("No instance {iid}"))?;
                let m = self.manifest(&inst.plugin_id).ok_or_else(|| anyhow!("Plugin {} is not installed", inst.plugin_id))?;
                let secret = secret_keys(&m.config);
                if secret.is_empty() {
                    bail!("This plugin has no secret settings.");
                }
                let mut fields: Vec<String> = a["fields"].as_array().into_iter().flatten().filter_map(|v| v.as_str()).map(str::to_string).collect();
                if fields.is_empty() {
                    fields = secret.clone();
                }
                if let Some(bad) = fields.iter().find(|f| !secret.contains(f)) {
                    bail!("{bad} is not a secret setting; secret ones: {}", secret.join(", "));
                }
                let req = SecretRequest { id: short_id("sr"), instance_id: iid.clone(), fields, reason: truncate(&s("reason"), 300) };
                {
                    let mut bots = self.bots.lock().unwrap();
                    let rt = bots.entry(c.id.clone()).or_default();
                    rt.secret_requests.retain(|r| r.instance_id != iid);
                    rt.secret_requests.push(req);
                }
                if let Ok(h) = self.hub() {
                    h.emit_event(json!({"type": "bots.secretsNeeded", "botId": c.id, "name": c.name, "pending": [{"id": iid, "name": inst.name}]}));
                }
                self.changed();
                Ok(json!("The owner now has a card in this session to enter it; the value goes straight into the plugin configuration and you never see it. You get a message once it is saved. Do not ask for it in the chat."))
            }
            "update_config" => {
                // Only on the owner's word: a turn started by events or reports could carry someone else's text
                if self.engine_turn(&c.id) {
                    bail!(
                        "Only when the owner asks for it in this session directly: this turn was started by events or reports. \
                         Nothing changed; tell the owner what you would change instead."
                    );
                }
                let old = self.store.bot(&c.id)?.config;
                let changes = &a["changes"];
                if !changes.is_object() {
                    bail!("`changes` must be an object with the fields to change.");
                }
                let mut unknown = vec![];
                tools::unknown_keys(&tools::draft(), changes, "", &mut unknown);
                if !unknown.is_empty() {
                    bail!("Unknown fields: {}. Nothing changed; use only the fields get_config returns.", unknown.join(", "));
                }
                let mut v = serde_json::to_value(&old)?;
                merge_patch(&mut v, changes);
                v["id"] = json!(old.id);
                v["promptRevision"] = json!(old.prompt_revision);
                let (nc, instances, errors, warnings) = self.check_draft(&json!({"draft": v, "instances": a["instances"]})).await?;
                let Some(nc) = nc.filter(|_| errors.is_empty()) else {
                    bail!("Nothing changed; fix these and try again:\n- {}", errors.join("\n- "));
                };
                if nc == old && instances.is_empty() {
                    return Ok(json!("Nothing changed."));
                }
                let summary = truncate(&s("reason"), 300);
                let items = sensitive_changes(&old, &nc);
                if !items.is_empty() {
                    let id = short_id("change");
                    self.bots.lock().unwrap().entry(c.id.clone()).or_default().pending =
                        Some(PendingChange { id, config: nc, instances, summary: summary.clone(), items: items.clone() });
                    // The pending card shows in the Bot's session bar through bots.list
                    self.changed();
                    if let Ok(h) = self.hub() {
                        h.emit_event(json!({"type": "bots.confirm", "botId": c.id, "name": c.name, "summary": summary}));
                    }
                    return Ok(json!(format!(
                        "Waiting for the owner: changing {} needs their confirmation in the app. Nothing has changed yet; \
                         tell them in one sentence what to confirm. Their answer arrives as a message.",
                        items.join(", ")
                    )));
                }
                let (_, pending) = self.apply_config(nc, &instances, "bot").await?;
                let mut out = String::from("Saved; instruction changes apply from your next turn and the owner can restore earlier prompt revisions.");
                if !warnings.is_empty() {
                    out += &format!("\nWarnings:\n- {}", warnings.join("\n- "));
                }
                out += &secrets_note(&pending);
                Ok(json!(out))
            }
            "propose_plugin" => {
                // A task built it; the Bot checked it and submits it from the task's host
                let t = self.bot_task(&c.id, &s("task_id"))?;
                let path = s("path");
                let path = if path.starts_with(['/', '~']) || path.get(1..2) == Some(":") {
                    path
                } else {
                    format!("{}/{}", t.cwd.trim_end_matches(['/', '\\']), path)
                };
                let staged = self.package_from_host(&t.host, &path).await?;
                if builtin::RESERVED.contains(&staged.manifest.id.as_str()) {
                    bail!("Plugin id {} is reserved; have the task choose another", staged.manifest.id);
                }
                let v = self.stage(staged)?;
                if let (Ok(h), Some(sr)) = (self.hub(), bot.session.as_ref()) {
                    h.emit_event(json!({"type": "bots.pluginProposal", "token": v["token"], "id": v["manifest"]["id"], "name": v["manifest"]["name"], "host": sr.host, "sessionId": sr.session_id}));
                }
                let list = |k: &str| {
                    v["manifest"]["permissions"][k].as_array().map(|x| x.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>().join(", ")).filter(|x| !x.is_empty()).unwrap_or_else(|| "none".into())
                };
                Ok(json!(format!(
                    "Submitted {} {} for review (network: {}; commands: {}). It is installed only when the owner confirms in the app.",
                    v["manifest"]["id"].as_str().unwrap_or(""),
                    v["manifest"]["version"].as_str().unwrap_or(""),
                    list("net"),
                    list("exec"),
                )))
            }
            t if tools::SETUP_TOOLS.contains(&t) => self.setup_tool(t, &a).await,
            "notes_read" => Ok(json!(self.store.bot(&c.id)?.notes)),
            "notes_write" => {
                self.store.set_notes(&c.id, &truncate(&s("text"), 8000))?;
                Ok(json!("saved"))
            }
            _ => bail!("Unknown tool: {tool}"),
        }
    }

    async fn read_thread(&self, bot_id: &str, event_id: &str, cursor: Option<&str>) -> Result<Value> {
        let e = self.bot_event(bot_id, event_id)?;
        let inst = self.instance(&e.instance_id).ok_or_else(|| anyhow!("This event's source is not running."))?;
        let cursor = cursor.map(str::to_string);
        let page = blocking(move || inst.read_context(&e.thread, cursor.as_deref())).await??;
        Ok(serde_json::to_value(page)?)
    }

    fn instance_plugin(&self, id: &str) -> Option<String> {
        if id == APP_INSTANCE {
            return Some(builtin::APP_NOTIFY.into());
        }
        self.store.instances().ok()?.into_iter().find(|i| i.id == id).map(|i| i.plugin_id)
    }

    /// Instances a Bot may use tools of: the ones it picked, whose plugin has the tools capability
    fn tool_instances(&self, c: &BotConfig) -> Vec<String> {
        c.triggers
            .iter()
            .chain(c.notify.iter())
            .filter(|id| self.instance_plugin(id).and_then(|p| self.manifest(&p)).is_some_and(|m| m.has("tools")))
            .cloned()
            .collect::<HashSet<_>>()
            .into_iter()
            .collect()
    }

    // ---------- outbox ----------

    /// Queue a message for a plugin and try it right away; returns the row after that attempt. A key that is already
    /// queued or sent is not sent again
    async fn send_out(&self, mut o: OutboxRow) -> OutboxRow {
        let now = now_ms();
        o.state = "pending".into();
        o.created_at = now;
        o.next_at = now;
        if !self.store.outbox_add(&o).unwrap_or(false) {
            return self.store.outbox_get(&o.key).unwrap_or(o);
        }
        self.attempt(o).await
    }

    async fn attempt(&self, mut o: OutboxRow) -> OutboxRow {
        let r = match self.instance(&o.instance_id) {
            None => Err(anyhow!("这份插件配置没有在运行")),
            Some(inst) => {
                let m = o.clone();
                blocking(move || {
                    if m.kind == "notify" {
                        inst.notify(&m.text, &m.key)
                    } else {
                        inst.reply(&m.thread, m.reply_to.as_deref(), &m.key, &m.text)
                    }
                })
                .await
                .and_then(|r| r)
            }
        };
        o.attempts += 1;
        match r {
            Ok(SendResult::Sent) => {
                o.state = "sent".into();
                o.error = None;
            }
            Ok(SendResult::Gone) => o.state = "gone".into(),
            Err(e) => {
                o.error = Some(truncate(&format!("{e:#}"), 500));
                if o.attempts >= OUTBOX_ATTEMPTS {
                    o.state = "failed".into();
                } else {
                    o.next_at = now_ms() + (30_000i64 << (o.attempts - 1)).min(3_600_000);
                }
            }
        }
        let _ = self.store.outbox_save(&o);
        // A reply that never made it is worth a look; failed notifications are not (they are how people get told)
        if o.state == "failed" && o.kind == "reply" {
            if let Some(b) = &o.bot_id {
                self.attention(b, None, &format!("回复没有发出去（{}）：{}", o.instance_id, truncate(&o.text, 200)), "low");
            }
        }
        self.changed();
        o
    }

    /// A task's acknowledgement as a reply in `e`'s conversation; None when its plugin can't reply
    fn ack_row(&self, t: &TaskRow, e: &EventRow, text: &str) -> Option<OutboxRow> {
        let can_reply = self.instance_plugin(&e.instance_id).and_then(|p| self.manifest(&p)).is_some_and(|m| m.has("reply"));
        let now = now_ms();
        can_reply.then(|| OutboxRow {
            key: format!("ack:{}", t.id),
            instance_id: e.instance_id.clone(),
            kind: "reply".into(),
            bot_id: Some(t.bot_id.clone()),
            thread: e.thread.clone(),
            reply_to: Some(e.key.clone()),
            text: truncate(text, 500),
            state: "pending".into(),
            created_at: now,
            next_at: now,
            ..Default::default()
        })
    }

    /// A ready or blocked (`needsYou`) task that gets more to do goes back to work; its timeout counts from now.
    fn reopen(&self, t: &mut TaskRow) -> Result<()> {
        if !matches!(t.state.as_str(), "ready" | "needsYou") || t.session_id.is_none() {
            return Ok(());
        }
        t.state = "working".into();
        t.started_at = Some(now_ms());
        if t.last_report.as_ref().is_some_and(|r| r["state"] == "blocked") {
            t.last_report = None;
        }
        self.store.save_task(t)?;
        self.store.resolve_attention(None, Some(&t.id), now_ms())?;
        self.changed();
        Ok(())
    }

    /// Another computer-use task currently holding `host`'s desktop (`working` or `needsYou`).
    fn desktop_holder(&self, host: &str, except_task: Option<&str>) -> Result<Option<TaskRow>> {
        Ok(self
            .store
            .active_tasks()?
            .into_iter()
            .find(|x| except_task != Some(x.id.as_str()) && x.host == host && x.state != "queued" && task_uses_computer(x)))
    }

    /// Make sure `host`'s desktop is unlocked, available and free before sending a turn to a computer-use task.
    async fn check_task_desktop_ready(&self, t: &TaskRow) -> Result<()> {
        if !task_uses_computer(t) {
            return Ok(());
        }
        if let Some(other) = self.desktop_holder(&t.host, Some(&t.id))? {
            bail!("主机上另一个桌面任务「{}」正在运行，请稍后再试", other.title);
        }
        let sid = t.session_id.as_deref().ok_or_else(|| anyhow!("任务会话不存在"))?;
        let cu = self.rpc(&t.host, "computerUse.status", json!({"sessionId": sid})).await?;
        if cu["supported"] == false {
            bail!("操作电脑仅支持 Windows x64 和 macOS");
        }
        if cu["status"] == "driverMissing" {
            bail!("远端电脑缺少桌面驱动");
        }
        if cu["status"] == "desktopUnavailable" {
            bail!("桌面不可用，请先在远端电脑上登录并解锁桌面");
        }
        if cu["enabled"] != true {
            let enabled = self.rpc(&t.host, "session.computerUse", json!({"sessionId": sid, "action": "enable"})).await?;
            if enabled["status"] == "permissionsRequired" {
                bail!("请先在远端 Mac 的系统设置中为 Cua Driver 开启辅助功能和屏幕录制权限");
            }
        } else if cu["status"] == "permissionsRequired" {
            bail!("请先在远端 Mac 的系统设置中为 Cua Driver 开启辅助功能和屏幕录制权限");
        }
        Ok(())
    }

    /// Send a message to a working, blocked or ready task, enabling desktop access (and sending a held initial brief) first when needed.
    async fn send_to_task(&self, t: &mut TaskRow, text: String) -> Result<()> {
        let sid = t.session_id.clone().filter(|_| t.active() || t.state == "ready").ok_or_else(|| anyhow!("任务已结束"))?;
        self.check_task_desktop_ready(t).await?;
        let pending_brief = t.spec["pendingBrief"].as_bool().unwrap_or(false);
        self.reopen(t)?;
        if pending_brief {
            t.spec["pendingBrief"] = json!(false);
            self.store.save_task(t)?;
            let events: Vec<EventRow> = t.event_ids.iter().filter_map(|id| self.store.event(id).ok()).collect();
            let initial = brief(t, &events, &|id| self.instance_label(id));
            self.rpc(&t.host, "session.send", json!({"sessionId": sid, "text": initial, "queue": true})).await?;
        }
        if !text.is_empty() {
            self.rpc(&t.host, "session.send", json!({"sessionId": sid, "text": text, "queue": true, "origin": ENGINE_ORIGIN}))
                .await?;
        }
        Ok(())
    }

    async fn release_task_desktop(&self, t: &TaskRow) {
        if task_uses_computer(t) && matches!(t.state.as_str(), "done" | "failed" | "stopped") {
            if let Some(sid) = &t.session_id {
                let _ = self.rpc(&t.host, "session.computerUse", json!({"sessionId": sid, "action": "stop"})).await;
            }
        }
    }

    fn bot_task(&self, bot_id: &str, id: &str) -> Result<TaskRow> {
        let t = self.store.task(id)?;
        if t.bot_id != bot_id {
            bail!("任务不存在: {id}");
        }
        Ok(t)
    }

    pub async fn task_tool(&self, task_id: &str, tool: &str, a: Value) -> Result<Value> {
        let mut t = self.store.task(task_id)?;
        let s = |k: &str| a[k].as_str().unwrap_or("").to_string();
        match tool {
            "report" => {
                let state = s("state");
                if !["progress", "ready", "blocked", "failed", "dropped"].contains(&state.as_str()) {
                    bail!("state must be progress, ready, blocked, failed or dropped");
                }
                if t.hold {
                    return Ok(json!("This task was stopped by the owner; nothing more is needed."));
                }
                t.last_report = Some(json!({"state": state, "summary": s("summary"), "question": a["question"], "links": a["links"], "at": now_ms()}));
                match state.as_str() {
                    "ready" => t.state = "ready".into(),
                    "blocked" => t.state = "needsYou".into(),
                    "failed" => t.state = "failed".into(),
                    "dropped" => t.state = "done".into(),
                    _ => {}
                }
                if matches!(state.as_str(), "failed" | "dropped") {
                    t.ended_at = Some(now_ms());
                }
                self.store.save_task(&t)?;
                if state == "blocked" {
                    let q = a["question"].as_str().filter(|q| !q.is_empty()).map(str::to_string).unwrap_or(s("summary"));
                    self.attention(&t.bot_id, Some(&t.id), &format!("{}: {q}", t.title), "normal");
                } else if state == "failed" {
                    self.attention(&t.bot_id, Some(&t.id), &format!("{}: {}", t.title, s("summary")), "normal");
                }
                let links = a["links"].as_array().map(|l| l.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>().join(" ")).unwrap_or_default();
                let mut card = format!("<report task=\"{}\" title=\"{}\" state=\"{state}\">\n{}", t.id, attr(&t.title), clip(&s("summary")));
                if let Some(q) = a["question"].as_str().filter(|q| !q.is_empty()) {
                    card.push_str(&format!("\nQuestion: {q}"));
                }
                if !links.is_empty() {
                    card.push_str(&format!("\nLinks: {links}"));
                }
                card.push_str("\n</report>");
                self.push(&t.bot_id, card, state != "progress");
                if !t.active() {
                    self.release_task_desktop(&t).await;
                    self.start_queued_for_host(&t.bot_id, &t.host).await;
                }
                Ok(json!("Reported to the Bot."))
            }
            "ack" => {
                if let Some(sent) = t.spec["ack"].as_str().filter(|a| !a.is_empty()) {
                    return Ok(json!(format!("Already acknowledged when the task was created (\"{sent}\"); nothing sent.")));
                }
                let text = s("text");
                t.spec["ack"] = json!(text);
                self.store.save_task(&t)?;
                let bot = self.store.bot(&t.bot_id)?;
                // The acknowledgement goes back to the source conversation, when there is one that takes replies
                let event_id = a["event_id"].as_str().map(str::to_string).or(t.event_ids.first().cloned());
                if let (Accept::Auto, Some(eid)) = (bot.config.accept, event_id) {
                    if let Some(o) = self.store.event(&eid).ok().and_then(|e| self.ack_row(&t, &e, &text)) {
                        self.send_out(o).await;
                    }
                }
                Ok(json!("ok"))
            }
            "handback" => {
                let reason = s("reason");
                if let Some(eid) = a["event_id"].as_str() {
                    if let Ok(mut e) = self.store.event(eid) {
                        e.disposition = "queued".into();
                        e.task_id = None;
                        let _ = self.store.update_event(&e);
                    }
                }
                self.push(
                    &t.bot_id,
                    format!("<handback task=\"{}\" event=\"{}\">{reason}</handback>", t.id, a["event_id"].as_str().unwrap_or("")),
                    true,
                );
                Ok(json!("Handed back to the Bot."))
            }
            "read_event" => {
                let e = self.store.event(&s("event_id"))?;
                if e.bot_id != t.bot_id {
                    bail!("事件不存在");
                }
                Ok(serde_json::to_value(e)?)
            }
            "read_thread" => self.read_thread(&t.bot_id, &s("event_id"), a["cursor"].as_str()).await,
            "plugin_kit" => {
                let dir = a["dir"].as_str().filter(|d| !d.is_empty()).map(str::to_string).unwrap_or_else(|| format!("{}/yi-plugin-kit", t.cwd.trim_end_matches(['/', '\\'])));
                let dir = dir.trim_end_matches(['/', '\\']).to_string();
                let mut written = vec![];
                for (rel, content) in tools::plugin_kit() {
                    let path = format!("{dir}/{rel}");
                    if let Some((parent, _)) = path.rsplit_once('/') {
                        // "already exists" is fine
                        let _ = self.rpc(&t.host, "fs.mkdir", json!({"path": parent})).await;
                    }
                    self.rpc(&t.host, "fs.write", json!({"path": path, "content": content})).await?;
                    written.push(rel);
                }
                Ok(json!(format!(
                    "Wrote {dir}: {}. Read GUIDE.md first. Add the plugin as a new workspace member next to example/, build it with \
                     `cargo build --release --target wasm32-wasip2 -p <crate>` (once: `rustup target add wasm32-wasip2`), copy \
                     target/wasm32-wasip2/release/<crate_name>.wasm into a folder with its plugin.json (named as `main`) and \
                     prompt.md, then report ready with that folder's path: the Bot checks it and submits it to the owner.",
                    written.join(", ")
                )))
            }
            _ => bail!("Unknown tool: {tool}"),
        }
    }

    // ---------- task lifecycle ----------

    /// Start a queued task if there is room (and the host's desktop is free when it uses computer use); returns its state
    async fn try_start(&self, task_id: &str) -> Result<String> {
        let t = self.store.task(task_id)?;
        if t.state != "queued" {
            return Ok(t.state);
        }
        let c = self.store.bot(&t.bot_id)?.config;
        let running = self
            .store
            .active_tasks()?
            .iter()
            .filter(|x| x.bot_id == t.bot_id && x.state != "queued")
            .count() as u32;
        if running >= c.task.max_concurrent {
            return Ok("queued".into());
        }
        if task_uses_computer(&t) && self.desktop_holder(&t.host, Some(&t.id))?.is_some() {
            return Ok("queued".into());
        }
        match self.start_task(&c, t.clone()).await {
            Ok(state) => Ok(state),
            Err(e) => {
                let mut t = self.store.task(task_id)?;
                t.state = "failed".into();
                t.ended_at = Some(now_ms());
                t.last_report = Some(json!({"state": "failed", "summary": format!("Could not start: {e:#}"), "at": now_ms()}));
                self.store.save_task(&t)?;
                Err(anyhow!("Could not start the task: {e:#}"))
            }
        }
    }

    async fn start_task(&self, c: &BotConfig, mut t: TaskRow) -> Result<String> {
        let mut cwd = t.workspace.clone();
        if t.spec["worktree"].as_bool().unwrap_or(true) {
            let name = t.id.replace('_', "-");
            match self.rpc(&t.host, "git.worktreeAdd", json!({"cwd": t.workspace, "name": name, "branch": format!("yi/{name}")})).await {
                Ok(w) => {
                    cwd = w["path"].as_str().unwrap_or(&cwd).to_string();
                    t.worktree = Some(w);
                }
                Err(e) if format!("{e:#}").contains("不是 git 仓库") => {}
                Err(e) => return Err(e),
            }
        }
        let want_cu = task_uses_computer(&t);
        if want_cu {
            let cu = self.rpc(&t.host, "computerUse.status", json!({})).await?;
            if cu["supported"] == false {
                bail!("Host {} does not support computer use (Windows x64 and macOS only)", t.host);
            }
            if cu["status"] == "driverMissing" {
                bail!("Desktop driver is missing on host {}", t.host);
            }
            if cu["status"] == "desktopUnavailable" {
                return self.park_desktop_unavailable(c, t, cwd).await;
            }
        }
        let events: Vec<EventRow> = t.event_ids.iter().filter_map(|id| self.store.event(id).ok()).collect();
        let brief = brief(&t, &events, &|id| self.instance_label(id));
        let create = self
            .rpc(
                &t.host,
                "session.create",
                json!({
                    "tool": t.spec["tool"],
                    "cwd": cwd,
                    "title": t.title,
                    "model": t.spec["model"],
                    "effort": t.spec["effort"],
                    "permissionMode": c.task.permission_mode,
                    "appendSystemPrompt": prompt::task_prompt(c),
                    "clientMcp": [tools::client_mcp(TASK_SERVER, &self.identity.client_id)],
                    "bot": self.link(c, "task", Some(&t.id)),
                    "computerUse": want_cu,
                    "prompt": brief,
                }),
            )
            .await;
        let info = match create {
            Ok(info) => info,
            Err(e) if want_cu && format!("{e:#}").contains("桌面不可用") => return self.park_desktop_unavailable(c, t, cwd).await,
            Err(e) => return Err(e),
        };
        t.session_id = info["id"].as_str().map(str::to_string);
        t.cwd = cwd;
        t.state = "working".into();
        t.started_at = Some(now_ms());
        self.store.save_task(&t)?;
        self.changed();
        Ok("working".into())
    }

    /// The host's desktop is locked or not signed in: create an idle task session without sending the prompt yet and
    /// wait in `needsYou` until the owner unlocks it and continues.
    async fn park_desktop_unavailable(&self, c: &BotConfig, mut t: TaskRow, cwd: String) -> Result<String> {
        let info = self
            .rpc(
                &t.host,
                "session.create",
                json!({
                    "tool": t.spec["tool"],
                    "cwd": cwd,
                    "title": t.title,
                    "model": t.spec["model"],
                    "effort": t.spec["effort"],
                    "permissionMode": c.task.permission_mode,
                    "appendSystemPrompt": prompt::task_prompt(c),
                    "clientMcp": [tools::client_mcp(TASK_SERVER, &self.identity.client_id)],
                    "bot": self.link(c, "task", Some(&t.id)),
                }),
            )
            .await?;
        t.session_id = info["id"].as_str().map(str::to_string);
        t.cwd = cwd;
        t.state = "needsYou".into();
        t.started_at = Some(now_ms());
        t.spec["pendingBrief"] = json!(true);
        let summary = "The desktop on the host is locked or unavailable.";
        let question = "Unlock or sign in to the desktop on the host, then continue this task.";
        t.last_report = Some(json!({"state": "blocked", "summary": summary, "question": question, "at": now_ms()}));
        self.store.save_task(&t)?;
        self.attention(&t.bot_id, Some(&t.id), &format!("{}: 远端电脑当前锁屏或桌面不可用，请解锁后点继续", t.title), "normal");
        self.push(
            &t.bot_id,
            format!("<report task=\"{}\" title=\"{}\" state=\"blocked\">\n{summary}\nQuestion: {question}\n</report>", t.id, attr(&t.title)),
            true,
        );
        self.changed();
        Ok("needsYou".into())
    }

    async fn start_queued(&self, bot_id: &str) {
        for t in self.store.active_tasks().unwrap_or_default() {
            if t.bot_id == bot_id && t.state == "queued" {
                let _ = self.try_start(&t.id).await;
            }
        }
    }

    async fn start_queued_for_host(&self, bot_id: &str, host: &str) {
        for t in self.store.active_tasks().unwrap_or_default() {
            if t.state == "queued" && (t.bot_id == bot_id || (t.host == host && self.store.bot(&t.bot_id).is_ok_and(|b| b.config.enabled))) {
                let _ = self.try_start(&t.id).await;
            }
        }
    }

    pub async fn stop_task(&self, id: &str, bot_id: Option<&str>) -> Result<()> {
        let mut t = self.store.task(id)?;
        if bot_id.is_some_and(|b| b != t.bot_id) {
            bail!("任务不存在");
        }
        if let (true, Some(sid)) = (t.state == "working" || t.state == "needsYou", &t.session_id) {
            let _ = self.rpc(&t.host, "session.interrupt", json!({"sessionId": sid})).await;
        }
        t.state = "stopped".into();
        t.hold = true;
        t.ended_at = Some(now_ms());
        self.store.save_task(&t)?;
        self.store.resolve_attention(None, Some(id), now_ms())?;
        self.release_task_desktop(&t).await;
        self.start_queued_for_host(&t.bot_id, &t.host).await;
        self.changed();
        Ok(())
    }

    async fn tick(&self) {
        let now = now_ms();
        for mut t in self.store.active_tasks().unwrap_or_default() {
            let Ok(b) = self.store.bot(&t.bot_id) else { continue };
            let limit = b.config.task.timeout_minutes as i64 * 60_000;
            if t.state == "working" && limit > 0 && t.started_at.is_some_and(|s| now - s > limit) {
                if let Some(sid) = &t.session_id {
                    let _ = self.rpc(&t.host, "session.interrupt", json!({"sessionId": sid})).await;
                }
                t.state = "failed".into();
                t.ended_at = Some(now);
                t.last_report = Some(json!({"state": "failed", "summary": "Timed out", "at": now}));
                let _ = self.store.save_task(&t);
                self.release_task_desktop(&t).await;
                self.attention(&t.bot_id, Some(&t.id), &format!("{}: timed out", t.title), "normal");
                self.push(&t.bot_id, format!("<report task=\"{}\" title=\"{}\" state=\"failed\">Timed out after {} minutes.</report>", t.id, attr(&t.title), b.config.task.timeout_minutes), true);
            }
        }
        for mut ap in self.store.approvals(None, Some("pending"), 500).unwrap_or_default() {
            if now - ap.created_at > APPROVAL_HOURS * 3_600_000 {
                ap.state = "expired".into();
                ap.decided_at = Some(now);
                let _ = self.store.save_approval(&ap);
                let what = if ap.kind == "reply" { "reply" } else { "action" };
                self.push(
                    &ap.bot_id,
                    format!(
                        "<approval id=\"{}\" state=\"expired\" kind=\"{}\" text=\"{}\">Your drafted {what} ({}) was not approved within {APPROVAL_HOURS} hours and was dropped.</approval>",
                        ap.id,
                        ap.kind,
                        attr(&truncate(&ap.text, 120)),
                        truncate(&ap.text, 200)
                    ),
                    false,
                );
                self.changed();
            }
        }
        for mut r in self.store.open_runs().unwrap_or_default() {
            if now - r.started_at > RUN_HOURS * 3_600_000 {
                r.state = "failed".into();
                r.summary = "No result".into();
                r.ended_at = Some(now);
                let _ = self.store.save_run(&r);
                self.changed();
            }
        }
        for b in self.store.bots().unwrap_or_default() {
            if b.config.enabled {
                self.schedule_routines(&b.config);
                self.release_held(&b.config);
                self.start_queued(&b.config.id).await;
                self.deliver(&b.config.id).await;
            }
        }
        for o in self.store.outbox_due(now).unwrap_or_default() {
            self.attempt(o).await;
        }
        self.tidy_tasks().await;
    }

    /// Ended tasks: the worktree goes as soon as the task ends, unless something in it is uncommitted (its branch
    /// stays either way); after `keepDays` the session goes too, and a worktree that is clean by then.
    async fn tidy_tasks(&self) {
        let now = now_ms();
        let mut changed = false;
        for mut t in self.store.untidy_tasks().unwrap_or_default() {
            let Ok(b) = self.store.bot(&t.bot_id) else { continue };
            if self.task_busy.lock().unwrap().get(&t.id).copied().unwrap_or(false) {
                continue;
            }
            let keep = b.config.task.keep_days as i64 * 86_400_000;
            let due = keep > 0 && t.ended_at.is_some_and(|e| now - e > keep);
            let fresh = t.worktree.as_ref().is_some_and(|w| w["dirty"] != true && w["error"].is_null());
            if fresh || (due && t.worktree.is_some()) {
                self.drop_worktree(&mut t).await;
                changed = true;
            }
            if due {
                self.clean_task(&mut t).await;
                changed = true;
            }
        }
        if changed {
            self.changed();
        }
    }

    /// Removes an ended task's worktree when nothing in it is uncommitted; the branch stays. Marks it `dirty` (kept
    /// for the owner) or `error` otherwise. True when the task has no worktree left.
    async fn drop_worktree(&self, t: &mut TaskRow) -> bool {
        let Some(path) = t.worktree.as_ref().and_then(|w| w["path"].as_str()).map(str::to_string) else { return true };
        let mark = |t: &mut TaskRow, k: &str, v: Value| {
            if let Some(w) = t.worktree.as_mut() {
                w[k] = v;
            }
        };
        if let Ok(c) = self.rpc(&t.host, "git.changes", json!({"cwd": path, "diff": false})).await {
            if c["files"].as_array().is_some_and(|f| !f.is_empty()) {
                mark(t, "dirty", json!(true));
                let _ = self.store.save_task(t);
                return false;
            }
        }
        // The task's tool process runs inside the worktree; on Windows its directory can't be deleted while it lives
        if let Some(sid) = &t.session_id {
            let _ = self.rpc(&t.host, "session.kill", json!({"sessionId": sid})).await;
        }
        let args = json!({"path": path, "force": false});
        let mut r = self.rpc(&t.host, "git.worktreeRemove", args.clone()).await;
        if r.is_err() {
            // Child processes may take a moment to let go of the directory
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            r = self.rpc(&t.host, "git.worktreeRemove", args).await;
        }
        let gone = match r {
            Ok(_) => true,
            // Removed by hand already
            Err(e) if e.to_string().contains("worktree 不存在") => true,
            Err(e) => {
                mark(t, "error", json!(e.to_string()));
                false
            }
        };
        if gone {
            t.worktree = None;
        }
        let _ = self.store.save_task(t);
        gone
    }

    /// Deletes an ended task's session; the task row stays with its title, result and events
    async fn clean_task(&self, t: &mut TaskRow) {
        if let Some(sid) = t.session_id.take() {
            let _ = self.rpc(&t.host, "session.delete", json!({"sessionId": sid})).await;
        }
        t.cleaned_at = Some(now_ms());
        let _ = self.store.save_task(t);
    }

    /// Events held outside active hours, delivered once they start (or the hours are removed)
    fn release_held(&self, c: &BotConfig) {
        if !in_active_hours(c) {
            return;
        }
        let mut held = self.store.events(&c.id, None, 200, Some("held")).unwrap_or_default();
        if held.is_empty() {
            return;
        }
        held.sort_by_key(|e| e.at);
        for mut e in held {
            e.disposition = "queued".into();
            e.note.clear();
            if self.store.update_event(&e).is_err() {
                continue;
            }
            let text = card(&e, self.explicit(c, &e), &self.instance_label(&e.instance_id));
            self.push_item(&c.id, Inbox { text, events: vec![e.id.clone()], wake: true, owner: false, run: None });
        }
        self.changed();
    }

    // ---------- frontend commands ----------

    pub async fn command(&self, cmd: &str, a: Value) -> Result<Value> {
        let s = |k: &str| a[k].as_str().map(str::to_string).ok_or_else(|| anyhow!("缺少参数 {k}"));
        let r = match cmd {
            "bots.list" => self.list()?,
            // The hosts of this app, for editing Bots from a phone or yi-web (their own host ids differ)
            "bots.hosts" => self.hub()?.call("hosts.list", json!({})).await?,
            "bots.get" => {
                let b = self.store.bot(&s("id")?)?;
                json!({"bot": b, "prompts": self.store.prompts(&b.config.id)?, "preview": self.bot_prompt(&b.config)})
            }
            "bots.defaults" => json!({
                "shared": prompt::SHARED, "bot": prompt::BOT, "task": prompt::TASK, "dispatch": prompt::DISPATCH,
                "work": prompt::WORK, "communication": prompt::COMMUNICATION_DEFAULTS,
            }),
            "bots.preview" => {
                let mut c: BotConfig = serde_json::from_value(a["bot"].clone())?;
                c.normalize();
                json!({"bot": self.bot_prompt(&c), "task": prompt::task_prompt(&c)})
            }
            "bots.save" => {
                let c: BotConfig = serde_json::from_value(a["bot"].clone()).map_err(|e| anyhow!("Bot 配置无效: {e}"))?;
                let origin = a["origin"].as_str().unwrap_or("form").to_string();
                serde_json::to_value(self.save(c, &origin).await?)?
            }
            "bots.setEnabled" => {
                let mut c = self.store.bot(&s("id")?)?.config;
                c.enabled = a["enabled"].as_bool().unwrap_or(true);
                serde_json::to_value(self.save(c, "form").await?)?
            }
            "bots.delete" => {
                let id = s("id")?;
                let b = self.store.bot(&id)?;
                if a["sessions"].as_bool().unwrap_or(false) {
                    if let Some(sr) = &b.session {
                        let _ = self.rpc(&sr.host, "session.delete", json!({"sessionId": sr.session_id})).await;
                    }
                    for t in self.store.tasks(Some(&id), 10_000)? {
                        if let Some(sid) = &t.session_id {
                            let _ = self.rpc(&t.host, "session.delete", json!({"sessionId": sid})).await;
                        }
                    }
                } else if let Some(sr) = &b.session {
                    // Keep the session as an ordinary one
                    let _ = self.rpc(&sr.host, "session.setClientMcp", json!({"sessionId": sr.session_id, "clientMcp": []})).await;
                    let _ = self.rpc(&sr.host, "session.setBot", json!({"sessionId": sr.session_id, "bot": null})).await;
                }
                self.store.delete_bot(&id)?;
                self.bots.lock().unwrap().remove(&id);
                Value::Null
            }
            "bots.restart" => {
                let id = s("id")?;
                serde_json::to_value(self.ensure_session(&id).await?)?
            }
            "bots.promptRestore" => {
                let id = s("id")?;
                let rev = a["revision"].as_u64().ok_or_else(|| anyhow!("缺少参数 revision"))? as u32;
                let r = self.store.prompts(&id)?.into_iter().find(|p| p.revision == rev).ok_or_else(|| anyhow!("没有第 {rev} 版提示词"))?;
                let mut c = self.store.bot(&id)?.config;
                c.prompt = r.sections;
                serde_json::to_value(self.save(c, "restore").await?)?
            }
            "bots.create" => {
                // A new Bot from the new-session page: where and with what it runs, and its role in the owner's words.
                // Everything else starts at the defaults; the Bot offers the setup in its first reply
                let host = s("host")?;
                let tool = s("tool")?;
                let role = a["role"].as_str().map(str::trim).filter(|r| !r.is_empty()).ok_or_else(|| anyhow!("先说说这个 Bot 负责什么"))?.to_string();
                let cwd = a["cwd"].as_str().map(str::trim).filter(|c| !c.is_empty()).ok_or_else(|| anyhow!("请选择目录"))?.to_string();
                let opt = |k: &str| a[k].as_str().filter(|v| !v.is_empty()).map(str::to_string);
                let c: BotConfig = serde_json::from_value(json!({
                    "name": default_name(&role),
                    "enabled": true,
                    "workspaces": [{"host": host, "cwd": cwd}],
                    "bot": {"host": host, "tool": tool, "model": opt("model"), "effort": opt("effort"), "permissionMode": opt("permissionMode")},
                    "task": {"tool": tool, "model": opt("model"), "effort": opt("effort")},
                    "accept": "observe",
                    "approval": {"reply": true, "write": true},
                    "prompt": {"role": role},
                }))?;
                let saved = self.save_with(c, "create", false).await?;
                let sr = self.ensure_session(&saved.id).await?;
                self.push_item(&saved.id, Inbox { text: role, events: vec![], wake: true, owner: true, run: None });
                json!({"botId": saved.id, "host": sr.host, "sessionId": sr.session_id})
            }
            "bots.confirmChange" => {
                let bot_id = s("botId")?;
                let id = s("id")?;
                let p = {
                    let mut bots = self.bots.lock().unwrap();
                    let rt = bots.entry(bot_id.clone()).or_default();
                    if rt.pending.as_ref().map(|p| p.id.as_str()) != Some(id.as_str()) {
                        bail!("这个修改已经处理过了");
                    }
                    rt.pending.take().unwrap()
                };
                if a["approve"].as_bool().unwrap_or(false) {
                    let (_, pending) = self.apply_config(p.config, &p.instances, "bot").await?;
                    self.push(&bot_id, format!("<owner event=\"change-approved\" summary=\"{}\">The owner approved your configuration change ({}); it is saved.{}</owner>", attr(&p.summary), p.summary, secrets_note(&pending)), true);
                } else {
                    self.push(&bot_id, format!("<owner event=\"change-declined\" summary=\"{}\">The owner declined your configuration change ({}); nothing changed.</owner>", attr(&p.summary), p.summary), true);
                }
                Value::Null
            }
            "plugins.staged" => {
                let token = s("token")?;
                let staged = self.staged.lock().unwrap();
                let st = staged.get(&token).ok_or_else(|| anyhow!("安装已过期，请重新选择插件包"))?;
                self.describe_staged(&token, st)
            }
            "bots.runRoutine" => {
                let b = self.store.bot(&s("botId")?)?;
                if !b.config.enabled {
                    bail!("Bot 已暂停，先启用再运行");
                }
                let rid = s("routineId")?;
                let r = b.config.routines.iter().find(|r| r.id == rid).ok_or_else(|| anyhow!("找不到这个例行任务"))?;
                json!(self.start_routine(&b.config, r, true)?)
            }
            "bots.routineRuns" => {
                let limit = a["limit"].as_u64().unwrap_or(20).min(100) as usize;
                json!(self.store.runs(&s("botId")?, a["routineId"].as_str(), limit)?)
            }
            "bots.fillSecrets" => {
                let bot_id = s("botId")?;
                let iid = s("instanceId")?;
                let b = self.store.bot(&bot_id)?;
                let cards = self.secret_cards(&b.config);
                let card = cards.iter().find(|c| c["instanceId"] == iid.as_str()).ok_or_else(|| anyhow!("这份插件配置不需要填写密钥了"))?;
                let allowed: Vec<&str> = card["fields"].as_array().into_iter().flatten().filter_map(|f| f["key"].as_str()).collect();
                let mut inst = self.store.instances()?.into_iter().find(|i| i.id == iid).ok_or_else(|| anyhow!("插件配置不存在"))?;
                let m = self.manifest(&inst.plugin_id).ok_or_else(|| anyhow!("插件不存在: {}", inst.plugin_id))?;
                let mut filled = vec![];
                for (k, v) in a["values"].as_object().into_iter().flatten() {
                    let v = v.as_str().unwrap_or("").trim();
                    if v.is_empty() || v == plugins::MASK {
                        continue;
                    }
                    if !allowed.contains(&k.as_str()) {
                        bail!("不能在这里填写 {k}");
                    }
                    inst.config[k] = json!(v);
                    filled.push(k.clone());
                }
                if filled.is_empty() {
                    bail!("请填写密钥");
                }
                // Filled in from the card: the configuration is meant to run once it is complete
                let complete = plugins::validate_config(&m.config, &inst.config, true).is_ok();
                if complete {
                    inst.enabled = true;
                }
                let name = inst.name.clone();
                self.save_instance(inst).await?;
                self.bots.lock().unwrap().entry(bot_id.clone()).or_default().secret_requests.retain(|r| r.instance_id != iid);
                let state = if complete { "it is enabled now" } else { "it still misses other settings and stays disabled" };
                let text = format!("<owner event=\"secret-filled\" name=\"{}\">The owner entered {} for the plugin configuration \"{name}\" ({iid}); {state}. You never see the values.</owner>", attr(&name), filled.join(", "));
                self.push_item(&bot_id, Inbox { text, events: vec![], wake: true, owner: true, run: None });
                self.changed();
                json!({"enabled": complete})
            }
            "bots.dismissSecret" => {
                let bot_id = s("botId")?;
                let id = s("id")?;
                let gone = {
                    let mut bots = self.bots.lock().unwrap();
                    let rt = bots.entry(bot_id.clone()).or_default();
                    let n = rt.secret_requests.len();
                    rt.secret_requests.retain(|r| r.id != id);
                    n != rt.secret_requests.len()
                };
                if gone {
                    self.push(&bot_id, "<owner event=\"secret-dismissed\">The owner dismissed your request for a secret.</owner>".into(), false);
                    self.changed();
                }
                Value::Null
            }
            "bots.approve" => self.decide(&s("id")?, a["approve"].as_bool().unwrap_or(false), a["text"].as_str().map(str::to_string)).await?,
            "bots.approvals" => {
                let limit = a["limit"].as_u64().unwrap_or(50).min(200) as usize;
                json!(self.store.approvals(a["botId"].as_str(), a["state"].as_str(), limit)?)
            }
            "bots.events" => {
                let limit = a["limit"].as_u64().unwrap_or(100).min(500) as usize;
                json!(self.store.events(&s("id")?, a["before"].as_i64(), limit, a["disposition"].as_str())?)
            }
            "bots.stats" => {
                let id = s("id")?;
                let c = self.store.bot(&id)?.config;
                let mut v = stats(&self.store, &id, c.budget.tasks_per_day, a["days"].as_u64().unwrap_or(14).clamp(1, 90) as i64)?;
                v["tokensPerDay"] = json!(c.budget.tokens_per_day);
                v["tokensToday"] = json!(self.tokens_on(&id, &local_day(now_ms())));
                v
            }
            "bots.prompts" => json!(self.store.prompts(&s("id")?)?),
            "bots.saveNotes" => {
                self.store.set_notes(&s("id")?, &s("notes")?)?;
                Value::Null
            }
            "bots.resolve" => {
                self.store.resolve_attention(a["id"].as_str(), a["taskId"].as_str(), now_ms())?;
                if let Some(b) = a["botId"].as_str() {
                    self.store.resolve_bot_attention(b, now_ms())?;
                }
                Value::Null
            }
            "tasks.events" => {
                let t = self.store.task(&s("id")?)?;
                json!(t.event_ids.iter().filter_map(|id| self.store.event(id).ok()).collect::<Vec<_>>())
            }
            "tasks.list" => json!(self.store.tasks(a["botId"].as_str(), a["limit"].as_u64().unwrap_or(100) as usize)?),
            "tasks.stop" => {
                self.stop_task(&s("id")?, None).await?;
                Value::Null
            }
            "tasks.markDone" => {
                let mut t = self.store.task(&s("id")?)?;
                t.state = "done".into();
                t.ended_at.get_or_insert(now_ms());
                self.store.save_task(&t)?;
                self.store.resolve_attention(None, Some(&t.id), now_ms())?;
                self.release_task_desktop(&t).await;
                self.start_queued_for_host(&t.bot_id, &t.host).await;
                Value::Null
            }
            "tasks.continue" => {
                let mut t = self.store.task(&s("id")?)?;
                if t.state != "needsYou" {
                    bail!("只有等待你处理的任务才能继续");
                }
                let pending_brief = t.spec["pendingBrief"].as_bool().unwrap_or(false);
                t.manual = false;
                let msg = if pending_brief {
                    String::new()
                } else {
                    "<owner event=\"task-continued\">The owner handled what this task was waiting for and asked you to continue. If you are using desktop tools, read the current window state again before acting.</owner>".to_string()
                };
                self.send_to_task(&mut t, msg).await?;
                self.push(
                    &t.bot_id,
                    format!("<owner event=\"task-continued\" summary=\"{}\">The owner handled what task \"{}\" ({}) was waiting for and resumed it.</owner>", attr(&t.title), t.title, t.id),
                    false,
                );
                Value::Null
            }
            "tasks.manual" | "tasks.handBack" => {
                let mut t = self.store.task(&s("id")?)?;
                t.manual = cmd == "tasks.manual";
                self.store.save_task(&t)?;
                if cmd == "tasks.handBack" {
                    self.push(&t.bot_id, format!("<handback task=\"{}\">The owner handed this task back to you.</handback>", t.id), true);
                }
                Value::Null
            }
            "tasks.retry" => {
                let mut t = self.store.task(&s("id")?)?;
                if t.active() {
                    bail!("任务还在进行中");
                }
                t.state = "queued".into();
                t.hold = false;
                t.ended_at = None;
                t.cleaned_at = None;
                t.session_id = None;
                t.worktree = None;
                t.created_at = now_ms();
                self.store.save_task(&t)?;
                json!(self.try_start(&t.id).await?)
            }
            "tasks.cleanup" => {
                let mut t = self.store.task(&s("id")?)?;
                if !matches!(t.state.as_str(), "done" | "failed" | "stopped") {
                    bail!("只能清理已结束的任务");
                }
                let kept = !self.drop_worktree(&mut t).await;
                self.clean_task(&mut t).await;
                self.changed();
                json!({"worktreeKept": kept})
            }
            "tasks.removeWorktree" => {
                let mut t = self.store.task(&s("id")?)?;
                let path = t.worktree.as_ref().and_then(|w| w["path"].as_str()).map(str::to_string).ok_or_else(|| anyhow!("这个任务没有 worktree"))?;
                if t.active() {
                    bail!("任务还在进行中，先停止它");
                }
                // The task's tool process runs inside the worktree; on Windows its directory can't be deleted while it lives
                if let Some(sid) = &t.session_id {
                    let _ = self.rpc(&t.host, "session.kill", json!({"sessionId": sid})).await;
                }
                let args = json!({"path": path, "force": a["force"].as_bool().unwrap_or(false)});
                if self.rpc(&t.host, "git.worktreeRemove", args.clone()).await.is_err() {
                    // Child processes may take a moment to let go of the directory
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    self.rpc(&t.host, "git.worktreeRemove", args).await?;
                }
                t.worktree = None;
                self.store.save_task(&t)?;
                Value::Null
            }
            "plugins.list" => {
                let health = self.health.lock().unwrap().clone();
                json!({"plugins": self.manifests(), "health": health, "disabled": self.disabled_plugins(), "wasm": self.wasm.is_some()})
            }
            "plugins.inspect" => self.inspect(&a).await?,
            "plugins.install" => {
                let token = s("token")?;
                let staged = self.staged.lock().unwrap().remove(&token).ok_or_else(|| anyhow!("安装已过期，请重新选择插件包"))?;
                json!(self.install(staged).await?)
            }
            "plugins.uninstall" => {
                let id = s("id")?;
                let wasm = self.wasm.clone().ok_or_else(|| anyhow!("插件运行时没有启动"))?;
                if wasm.installed(&id).is_none() {
                    bail!("只能卸载安装的插件");
                }
                if wasm.is_bundled(&id) {
                    bail!("这是 App 自带的插件，不能卸载；不用时可以停用");
                }
                let instances: Vec<Instance> = self.store.instances()?.into_iter().filter(|i| i.plugin_id == id).collect();
                for i in &instances {
                    self.stop_instance(&i.id);
                }
                wasm.unload(&id);
                let _ = std::fs::remove_dir_all(wasm.dir().join(&id));
                if !a["keepData"].as_bool().unwrap_or(false) {
                    for i in &instances {
                        self.store.purge_instance(&i.id)?;
                        self.prune_instance(&i.id)?;
                    }
                }
                self.refresh_prompts().await;
                Value::Null
            }
            "plugins.setEnabled" => {
                let id = s("id")?;
                let enabled = a["enabled"].as_bool().unwrap_or(true);
                let mut off = self.disabled_plugins();
                off.retain(|x| *x != id);
                if !enabled {
                    off.push(id.clone());
                }
                self.store.kv_set(APP_KV, "disabledPlugins", Some(&serde_json::to_string(&off)?))?;
                for i in self.store.instances()?.into_iter().filter(|i| i.plugin_id == id) {
                    if enabled && i.enabled {
                        self.start_instance(&i);
                    } else {
                        self.stop_instance(&i.id);
                    }
                }
                self.refresh_prompts().await;
                Value::Null
            }
            "plugins.tools" | "plugins.callTool" => {
                let inst = self.instance(&s("instanceId")?).ok_or_else(|| anyhow!("这份插件配置没有在运行"))?;
                if cmd == "plugins.tools" {
                    json!(blocking(move || inst.tools()).await?)
                } else {
                    // The owner trying a tool by hand: runs whatever the Bot's mode is
                    let (name, input) = (s("name")?, a["input"].clone());
                    json!(blocking(move || inst.call_tool(&name, &input)).await??)
                }
            }
            "plugins.outbox" => json!(self.store.outbox(a["instanceId"].as_str(), a["limit"].as_u64().unwrap_or(50).min(500) as usize)?),
            "plugins.instances" => self.instances_view()?,
            "plugins.saveInstance" => {
                let i: Instance = serde_json::from_value(a["instance"].clone()).map_err(|e| anyhow!("插件配置无效: {e}"))?;
                json!(self.save_instance(i).await?)
            }
            "plugins.deleteInstance" => {
                let id = s("id")?;
                self.stop_instance(&id);
                self.store.delete_instance(&id)?;
                self.prune_instance(&id)?;
                self.refresh_prompts().await;
                Value::Null
            }
            _ => bail!("未知命令: {cmd}"),
        };
        // Reads must not announce a change: the frontend refetches `bots.list` on `bots.changed`
        const READS: &[&str] = &[
            "bots.list", "bots.hosts", "bots.get", "bots.defaults", "bots.preview", "bots.events", "bots.stats", "bots.prompts", "bots.routineRuns", "bots.approvals", "plugins.staged", "tasks.list", "tasks.events", "plugins.list",
            "plugins.instances", "plugins.inspect", "plugins.outbox", "plugins.tools", "plugins.callTool",
        ];
        if !READS.contains(&cmd) {
            self.changed();
        }
        Ok(r)
    }

    fn list(&self) -> Result<Value> {
        let pending: HashMap<String, Value> = self
            .bots
            .lock()
            .unwrap()
            .iter()
            .filter_map(|(id, r)| r.pending.as_ref().map(|p| (id.clone(), json!({"id": p.id, "summary": p.summary, "items": p.items}))))
            .collect();
        let attention = self.store.attention(None)?;
        let approvals = self.store.approvals(None, Some("pending"), 500)?;
        let today = local_day(now_ms());
        let mut out = vec![];
        for b in self.store.bots()? {
            let id = b.config.id.clone();
            let (state, error, waiting, context) = {
                let bots = self.bots.lock().unwrap();
                let rt = bots.get(&id);
                let state = match rt {
                    _ if !b.config.enabled => "disabled",
                    Some(r) if r.starting => "starting",
                    Some(r) if r.error.is_some() => "error",
                    Some(r) if r.compacting => "compacting",
                    Some(r) => match r.status.as_deref() {
                        Some("running" | "starting" | "waiting-permission") => "running",
                        Some("error") => "error",
                        _ => "idle",
                    },
                    None => "idle",
                };
                (
                    state,
                    rt.and_then(|r| r.error.clone()),
                    rt.map(|r| r.inbox.iter().filter(|i| i.wake).count()).unwrap_or(0),
                    rt.and_then(|r| r.context),
                )
            };
            let mut tasks = self.store.tasks(Some(&id), 200)?;
            // Active ones first, then the latest 20 that ended
            tasks.sort_by_key(|t| (!t.active(), -t.created_at));
            let ended = tasks.iter().filter(|t| !t.active()).count();
            if ended > 20 {
                let keep = tasks.len() - (ended - 20);
                tasks.truncate(keep);
            }
            // Latest run per routine
            let mut last_runs = serde_json::Map::new();
            for r in self.store.runs(&id, None, 200)? {
                last_runs.entry(r.routine_id.clone()).or_insert_with(|| json!(r));
            }
            out.push(json!({
                "config": b.config,
                "session": b.session,
                "state": state,
                "error": error,
                "waiting": waiting,
                "context": context,
                "attention": attention.iter().filter(|a| a.bot_id == id).collect::<Vec<_>>(),
                "pendingChange": pending.get(&id),
                "approvals": approvals.iter().filter(|a| a.bot_id == id).collect::<Vec<_>>(),
                "secrets": self.secret_cards(&b.config),
                "lastRuns": last_runs,
                "tokensToday": self.tokens_on(&id, &today),
                "tasks": tasks,
            }));
        }
        Ok(json!({"bots": out, "clientId": self.identity.client_id}))
    }

    /// Save a plugin configuration and start or stop it; returns its id. A disabled one may still miss required
    /// settings (one a Bot created, waiting for the owner's secrets)
    async fn save_instance(&self, mut i: Instance) -> Result<String> {
        let m = self.manifest(&i.plugin_id).ok_or_else(|| anyhow!("插件不存在: {}", i.plugin_id))?;
        if m.singleton {
            bail!("这个插件只能有一份配置");
        }
        if i.id.is_empty() {
            i.id = short_id(&i.plugin_id.replace('-', ""));
        }
        if i.id == APP_INSTANCE {
            bail!("配置 id 已被占用");
        }
        let old = self.store.instances()?.into_iter().find(|x| x.id == i.id).map(|x| x.config).unwrap_or(json!({}));
        plugins::keep_secrets(&m.config, &mut i.config, &old)?;
        plugins::validate_config(&m.config, &i.config, i.enabled)?;
        if i.plugin_id == builtin::CRON {
            plugins::cron::Cron::parse(i.config["schedule"].as_str().unwrap_or(""))?;
        }
        i.name = i.name.trim().to_string();
        if i.name.is_empty() {
            i.name = m.name.zh.clone();
        }
        self.store.save_instance(&i, now_ms())?;
        if i.enabled {
            self.start_instance(&i);
        } else {
            self.stop_instance(&i.id);
        }
        self.refresh_prompts().await;
        Ok(i.id)
    }

    /// Save a configuration; returns it. New prompt text becomes a new revision
    async fn save(&self, c: BotConfig, origin: &str) -> Result<BotConfig> {
        self.save_with(c, origin, true).await
    }

    /// `start`: start the session of an enabled Bot in the background
    async fn save_with(&self, mut c: BotConfig, origin: &str, start: bool) -> Result<BotConfig> {
        c.normalize();
        c.validate()?;
        let old = if c.id.is_empty() {
            c.id = short_id("bot");
            None
        } else {
            Some(self.store.bot(&c.id)?)
        };
        let prompt_changed = old.as_ref().map_or(true, |o| o.config.prompt != c.prompt);
        if prompt_changed {
            let base = old.as_ref().map(|o| o.config.prompt_revision).unwrap_or(0);
            c.prompt_revision = self.store.add_prompt(&c.id, base, &c.prompt, origin, now_ms())?;
        }
        let now = now_ms();
        self.store.save_bot(&c, now)?;
        // Moving the Bot to another host starts a new session there
        if old.as_ref().is_some_and(|o| o.config.bot.host != c.bot.host) {
            self.store.set_bot_session(&c.id, None)?;
        } else if let Some(sr) = old.as_ref().filter(|o| o.config.name != c.name).and_then(|o| o.session.clone()) {
            // A Bot names itself in its first reply: keep its session title in step
            let _ = self.rpc(&sr.host, "session.rename", json!({"sessionId": sr.session_id, "title": format!("Bot · {}", c.name)})).await;
        }
        if c.enabled && start {
            let me = self.arc();
            let id = c.id.clone();
            tokio::spawn(async move {
                let _ = me.ensure_session(&id).await;
            });
        }
        Ok(c)
    }

    /// Drop an instance from every Bot that picked it, so no config keeps a dangling id
    fn prune_instance(&self, id: &str) -> Result<()> {
        for b in self.store.bots()? {
            let mut c = b.config;
            let before = (c.triggers.len(), c.notify.len(), c.allow.len());
            c.triggers.retain(|x| x != id);
            c.notify.retain(|x| x != id);
            c.allow.retain(|x| x.instance != id);
            if before != (c.triggers.len(), c.notify.len(), c.allow.len()) {
                self.store.save_bot(&c, now_ms())?;
            }
        }
        Ok(())
    }

    /// Read a package (`path` on this machine, `url`, or base64 `data`), check it, and keep it until confirmed
    async fn inspect(&self, a: &Value) -> Result<Value> {
        let wasm = self.wasm.clone().ok_or_else(|| anyhow!("插件运行时没有启动"))?;
        let path = a["path"].as_str().map(str::to_string);
        let url = a["url"].as_str().map(str::to_string);
        let data = a["data"].as_str().map(str::to_string);
        let staged = blocking(move || -> Result<package::Staged> {
            let s = if let Some(p) = path {
                package::from_path(Path::new(&p))?
            } else if let Some(u) = url {
                package::from_zip(&package::download(&u)?)?
            } else if let Some(d) = data {
                use base64::Engine as _;
                let bytes = base64::engine::general_purpose::STANDARD.decode(d.trim()).map_err(|_| anyhow!("插件包数据不对"))?;
                package::from_zip(&bytes)?
            } else {
                bail!("缺少参数 path / url / data");
            };
            wasm.check(s.main()?)?;
            Ok(s)
        })
        .await??;
        self.stage(staged)
    }

    /// Keep a checked package until the owner confirms it; what the review shows
    fn stage(&self, staged: package::Staged) -> Result<Value> {
        if self.wasm.as_ref().is_some_and(|w| w.is_bundled(&staged.manifest.id)) {
            bail!("「{}」是 App 自带的插件，随 App 一起更新，不能另外安装", staged.manifest.id);
        }
        let token = short_id("pkg");
        let v = self.describe_staged(&token, &staged);
        self.staged.lock().unwrap().insert(token, staged);
        Ok(v)
    }

    fn describe_staged(&self, token: &str, staged: &package::Staged) -> Value {
        let installed = self.wasm.as_ref().and_then(|w| w.installed(&staged.manifest.id));
        let added = package::added(&installed.as_ref().map(|m| m.permissions.clone()).unwrap_or_default(), &staged.manifest.permissions);
        json!({"token": token, "manifest": staged.manifest, "installed": installed, "added": added})
    }

    /// Install a confirmed package; running instances of the plugin restart on the new version
    async fn install(&self, staged: package::Staged) -> Result<String> {
        let wasm = self.wasm.clone().ok_or_else(|| anyhow!("插件运行时没有启动"))?;
        let id = staged.manifest.id.clone();
        if wasm.is_bundled(&id) {
            bail!("「{id}」是 App 自带的插件，随 App 一起更新，不能另外安装");
        }
        let instances: Vec<Instance> = self.store.instances()?.into_iter().filter(|i| i.plugin_id == id).collect();
        for i in &instances {
            self.stop_instance(&i.id);
        }
        let dir = {
            let w = wasm.clone();
            blocking(move || package::install(&staged, w.dir())).await??
        };
        wasm.load(&dir)?;
        for i in instances.iter().filter(|i| i.enabled) {
            self.start_instance(i);
        }
        self.refresh_prompts().await;
        Ok(id)
    }

    /// Plugin instances changed: the Bots' generated prompt layer mentions them
    async fn refresh_prompts(&self) {
        for b in self.store.bots().unwrap_or_default() {
            if let (true, Some(s)) = (b.config.enabled, &b.session) {
                let _ = self
                    .rpc(&s.host, "session.setSystemPrompt", json!({"sessionId": s.session_id, "prompt": self.bot_prompt(&b.config)}))
                    .await;
            }
        }
    }
}

fn attr(s: &str) -> String {
    s.replace('"', "'").replace(['\n', '\r'], " ")
}

/// An event as the Bot sees it
fn card(e: &EventRow, explicit: bool, source: &str) -> String {
    let sender = if e.sender_name.is_empty() { e.sender.clone() } else { format!("{} ({})", e.sender_name, e.sender) };
    let mut s = format!(
        "<event id=\"{}\" source=\"{}\" thread=\"{}\" sender=\"{}\" kind=\"{}\" at=\"{}\">\n{}",
        e.id,
        attr(source),
        attr(&e.thread),
        attr(&sender),
        if explicit { "explicit" } else { "ambient" },
        chrono::DateTime::from_timestamp_millis(e.at).map(|t| t.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string()).unwrap_or_default(),
        truncate(&e.text, 2000),
    );
    if let Some(u) = &e.url {
        s.push_str(&format!("\n{u}"));
    }
    s.push_str("\n</event>");
    s
}

fn follow_up(e: &EventRow) -> String {
    format!("Follow-up in your conversation (handle it, or hand it back with handback if it is outside this task):\n{}", card(e, true, &e.instance_id))
}

/// The first message of a task session
fn brief(t: &TaskRow, events: &[EventRow], label: &dyn Fn(&str) -> String) -> String {
    let mut s = format!("# Task: {}\n\nTask id: {}\n", t.title, t.id);
    if let Some(n) = t.spec["note"].as_str().filter(|n| !n.is_empty()) {
        s.push_str(&format!("Routing note from the Bot: {n}\n"));
    }
    if let Some(a) = t.spec["ack"].as_str().filter(|a| !a.is_empty()) {
        s.push_str(&format!("The requester was already told: \"{a}\". Do not ack again.\n"));
    }
    if let Some(r) = t.spec["request"].as_str().filter(|r| !r.is_empty()) {
        s.push_str(&format!("\n## Request from the owner\n\n{r}\n"));
    }
    if !events.is_empty() {
        s.push_str("\n## Events\n\n");
        for e in events {
            s.push_str(&card(e, true, &label(&e.instance_id)));
            s.push_str("\n\n");
        }
    }
    if let Some(w) = &t.worktree {
        s.push_str(&format!(
            "\nYou are in a git worktree of {} on branch {}. Commit your work there unless the delivery requirements in your instructions say otherwise.\n",
            t.workspace,
            w["branch"].as_str().unwrap_or("")
        ));
    }
    s.push_str("\nStart with ack, then do the work, and finish with report.");
    s
}

fn task_uses_computer(t: &TaskRow) -> bool {
    t.spec["computerUse"].as_bool().unwrap_or(false)
}

fn host_supports_computer_use(os: Option<&str>, arch: Option<&str>) -> Option<bool> {
    let (os, arch) = (os?, arch?);
    Some((os == "windows" && arch == "x86_64") || (os == "macos" && matches!(arch, "x86_64" | "aarch64")))
}

fn task_view(t: &TaskRow) -> Value {
    json!({
        "taskId": t.id, "title": t.title, "state": t.state, "host": t.host, "sessionId": t.session_id,
        "cwd": t.cwd, "computerUse": task_uses_computer(t), "lastReport": t.last_report, "manual": t.manual, "createdAt": t.created_at,
    })
}

// ---------- configuration changes a Bot asks for ----------

impl Engine {
    /// Plugin instances as the UI and the Bots see them: secrets masked, the app notification first
    fn instances_view(&self) -> Result<Value> {
        let manifests = self.manifests();
        let app = Instance { id: APP_INSTANCE.into(), plugin_id: builtin::APP_NOTIFY.into(), name: "App".into(), config: json!({}), enabled: true };
        let list: Vec<Value> = std::iter::once(app)
            .chain(self.store.instances()?)
            .map(|mut i| {
                if let Some(m) = manifests.iter().find(|m| m.id == i.plugin_id) {
                    i.config = plugins::redact(&m.config, &i.config);
                }
                serde_json::to_value(i).unwrap_or_default()
            })
            .collect();
        Ok(json!(list))
    }

    async fn host_entries(&self) -> Result<Vec<Value>> {
        let r = self.hub()?.call("hosts.list", json!({})).await?;
        Ok(r["hosts"].as_array().cloned().unwrap_or_default())
    }

    /// Check a configuration a Bot wants: the save rules, plus the hosts, plugins and instances it names. Returns the
    /// normalized configuration, the new instances with secrets taken out, errors and warnings
    async fn check_draft(&self, a: &Value) -> Result<(Option<BotConfig>, Vec<Value>, Vec<String>, Vec<String>)> {
        let (mut errors, mut warnings) = (vec![], vec![]);
        let manifests = self.manifests();
        let mut new_refs: HashMap<String, String> = HashMap::new();
        let mut instances = vec![];
        for i in a["instances"].as_array().into_iter().flatten() {
            let r = i["ref"].as_str().unwrap_or("").to_string();
            let pid = i["pluginId"].as_str().unwrap_or("").to_string();
            if !r.starts_with("new:") {
                errors.push(format!("Instance ref must start with \"new:\": {r:?}"));
                continue;
            }
            let Some(m) = manifests.iter().find(|m| m.id == pid) else {
                errors.push(format!("{r}: no plugin {pid:?} is installed (list_plugins)"));
                continue;
            };
            if m.singleton {
                errors.push(format!("{r}: plugin {pid} has a single built-in instance ({APP_INSTANCE}); use that id"));
                continue;
            }
            let mut config = if i["config"].is_object() { i["config"].clone() } else { json!({}) };
            let props = m.config["properties"].as_object().cloned().unwrap_or_default();
            let secret = |k: &str| props.get(k).is_some_and(|p| p["x-secret"] == true);
            if let Some(obj) = config.as_object_mut() {
                let given: Vec<String> = obj.keys().filter(|k| secret(k)).cloned().collect();
                for k in given {
                    obj.remove(&k);
                    warnings.push(format!("{r}: secret field {k} was dropped; the owner fills it in"));
                }
                for k in obj.keys() {
                    if !props.contains_key(k) {
                        warnings.push(format!("{r}: {pid} has no setting {k}"));
                    }
                }
            }
            for req in m.config["required"].as_array().into_iter().flatten().filter_map(|v| v.as_str()) {
                if !secret(req) && config.get(req).map_or(true, |v| v.is_null() || v.as_str() == Some("")) {
                    errors.push(format!("{r}: missing required setting {req}"));
                }
            }
            if pid == builtin::CRON {
                if let Err(e) = plugins::cron::Cron::parse(config["schedule"].as_str().unwrap_or("")) {
                    errors.push(format!("{r}: {e:#}"));
                }
            }
            new_refs.insert(r.clone(), pid.clone());
            instances.push(json!({"ref": r, "pluginId": pid, "name": i["name"].as_str().unwrap_or(""), "config": config}));
        }

        // Serde ignores unknown keys: name them, or a misplaced field passes silently
        let mut unknown = vec![];
        tools::unknown_keys(&tools::draft(), &a["draft"], "", &mut unknown);
        if !unknown.is_empty() {
            errors.push(format!("Unknown fields: {}. Use only the fields get_config returns.", unknown.join(", ")));
        }
        let mut c: BotConfig = match serde_json::from_value(a["draft"].clone()) {
            Ok(c) => c,
            Err(e) => {
                errors.push(format!("The draft is not a Bot configuration: {e}. Compare with get_config."));
                return Ok((None, instances, errors, warnings));
            }
        };
        c.normalize();
        if let Err(e) = c.validate() {
            errors.push(format!("{e:#}"));
        }
        if !c.id.is_empty() && self.store.bot(&c.id).is_err() {
            errors.push(format!("No Bot has id {}; leave id empty for a new Bot", c.id));
        }
        let hosts = self.host_entries().await.unwrap_or_default();
        let host_ids: Vec<&str> = hosts.iter().filter_map(|h| h["config"]["id"].as_str()).collect();
        for h in std::iter::once(&c.bot.host).chain(c.workspaces.iter().map(|w| &w.host)) {
            if !h.is_empty() && !host_ids.contains(&h.as_str()) {
                errors.push(format!("Unknown host {h} (list_hosts)"));
            }
        }
        // Each instance id names an existing instance or a new one, with the capability its use needs
        let existing = self.store.instances()?;
        let plugin_of = |id: &str| -> Option<String> {
            if id == APP_INSTANCE {
                return Some(builtin::APP_NOTIFY.into());
            }
            new_refs.get(id).cloned().or_else(|| existing.iter().find(|i| i.id == id).map(|i| i.plugin_id.clone()))
        };
        let mut need = |id: &str, cap: &str, what: &str| match plugin_of(id) {
            None => errors.push(format!("{what}: no instance {id} (list_instances, or a \"new:\" ref in instances)")),
            Some(pid) if !manifests.iter().any(|m| m.id == pid && m.has(cap)) => errors.push(format!("{what}: {id} ({pid}) cannot {cap}")),
            _ => {}
        };
        for id in &c.triggers {
            need(id, "trigger", "triggers");
        }
        for id in &c.notify {
            need(id, "notify", "notify");
        }
        for al in &c.allow {
            need(&al.instance, "trigger", "allow");
        }
        if c.workspaces.is_empty() {
            warnings.push("No workspace: this Bot cannot start tasks".into());
        } else if c.task.computer_use {
            let support_of = |hid: &str| {
                hosts.iter().find(|h| h["config"]["id"].as_str() == Some(hid)).and_then(|h| {
                    host_supports_computer_use(h["status"]["hello"]["os"].as_str(), h["status"]["hello"]["arch"].as_str())
                })
            };
            let known: Vec<(&str, bool)> = c.workspaces.iter().filter_map(|w| support_of(&w.host).map(|ok| (w.host.as_str(), ok))).collect();
            if !known.is_empty() && known.iter().all(|(_, ok)| !*ok) {
                errors.push("None of the configured workspaces are on a host that supports computer use (Windows x86_64 or macOS only).".into());
            } else if known.iter().any(|(_, ok)| !*ok) {
                let unsupported: Vec<&str> = known.iter().filter(|(_, ok)| !*ok).map(|(h, _)| *h).collect();
                warnings.push(format!("Workspace host(s) {} do not support computer use; only spawn computer_use tasks in workspaces on Windows x64 or macOS.", unsupported.join(", ")));
            }
        }
        if c.accept == Accept::Auto {
            warnings.push("accept is auto: tasks start without review; new Bots usually start in observe".into());
        }
        Ok((Some(c), instances, errors, warnings))
    }

    /// Read a plugin package from a host: a .yiplugin file, or a folder with plugin.json, the component and extras
    async fn package_from_host(&self, host: &str, path: &str) -> Result<package::Staged> {
        use base64::Engine as _;
        let read = |p: String| async move {
            let r = self.rpc(host, "fs.read", json!({"path": p, "raw": true})).await?;
            base64::engine::general_purpose::STANDARD.decode(r["base64"].as_str().unwrap_or("")).map_err(|e| anyhow!("{e}"))
        };
        let is_dir = self.rpc(host, "fs.list", json!({"path": path})).await.is_ok();
        let wasm = self.wasm.clone().ok_or_else(|| anyhow!("插件运行时没有启动"))?;
        if !is_dir {
            let zip = read(path.to_string()).await?;
            return blocking(move || -> Result<package::Staged> {
                let s = package::from_zip(&zip)?;
                wasm.check(s.main()?)?;
                Ok(s)
            })
            .await?;
        }
        let join = |f: &str| format!("{}/{f}", path.trim_end_matches(['/', '\\']));
        let manifest = read(join("plugin.json")).await?;
        let main = serde_json::from_slice::<Value>(&manifest).ok().and_then(|m| m["main"].as_str().map(str::to_string)).unwrap_or_else(|| "plugin.wasm".into());
        let mut files = vec![("plugin.json".to_string(), manifest)];
        files.push((main.clone(), read(join(&main)).await.map_err(|e| anyhow!("读取 {main} 失败: {e:#}"))?));
        for extra in ["prompt.md", "README.md", "LICENSE"] {
            if let Ok(b) = read(join(extra)).await {
                files.push((extra.to_string(), b));
            }
        }
        blocking(move || -> Result<package::Staged> {
            let s = package::finish(files)?;
            wasm.check(s.main()?)?;
            Ok(s)
        })
        .await?
    }

    /// Read-only lookups a Bot uses to change its configuration
    async fn setup_tool(&self, tool: &str, a: &Value) -> Result<Value> {
        let s = |k: &str| a[k].as_str().map(str::to_string).ok_or_else(|| anyhow!("Missing argument {k}"));
        match tool {
            "list_hosts" => Ok(json!(self
                .host_entries()
                .await?
                .iter()
                .map(|h| {
                    let os = h["status"]["hello"]["os"].as_str();
                    let arch = h["status"]["hello"]["arch"].as_str();
                    json!({
                        "id": h["config"]["id"],
                        "name": h["config"]["name"],
                        "connected": h["status"]["status"] == "connected",
                        "home": h["status"]["hello"]["home"],
                        "os": h["status"]["hello"]["os"],
                        "arch": h["status"]["hello"]["arch"],
                        "computerUse": host_supports_computer_use(os, arch),
                    })
                })
                .collect::<Vec<_>>())),
            "list_tools" => {
                let id = s("host")?;
                let hosts = self.host_entries().await?;
                let h = hosts.iter().find(|h| h["config"]["id"] == id.as_str()).ok_or_else(|| anyhow!("No host {id}"))?;
                if h["status"]["status"] != "connected" {
                    bail!("Host {id} is not connected");
                }
                Ok(json!(h["status"]["hello"]["tools"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|t| AI_TOOLS.contains(&t["kind"].as_str().unwrap_or("")))
                    .map(|t| json!({"tool": t["kind"], "available": t["available"], "version": t["version"]}))
                    .collect::<Vec<_>>()))
            }
            "list_models" => self.rpc(&s("host")?, "tools.models", json!({"tool": s("tool")?})).await,
            "list_plugins" => {
                let off = self.disabled_plugins();
                Ok(json!(self
                    .manifests()
                    .into_iter()
                    .map(|m| json!({
                        "id": m.id,
                        "name": m.name.en,
                        "description": m.description.en,
                        "runtime": m.runtime,
                        "capabilities": m.capabilities,
                        "permissions": m.permissions,
                        "config": m.config,
                        "singleton": m.singleton,
                        "disabled": off.contains(&m.id),
                    }))
                    .collect::<Vec<_>>()))
            }
            "list_instances" => self.instances_view(),
            _ => bail!("Unknown tool: {tool}"),
        }
    }

    /// Secret cards for a Bot: the instances it uses that miss a required secret, and the ones it asked for with
    /// request_secret. Each lists its secret fields with labels from the plugin's schema
    fn secret_cards(&self, c: &BotConfig) -> Vec<Value> {
        let requests = self.bots.lock().unwrap().get(&c.id).map(|r| r.secret_requests.clone()).unwrap_or_default();
        let used = bot_instances(c);
        let mut out = vec![];
        for inst in self.store.instances().unwrap_or_default() {
            if !used.contains(&inst.id) {
                continue;
            }
            let Some(m) = self.manifest(&inst.plugin_id) else { continue };
            let empty = |k: &str| inst.config.get(k).map_or(true, |v| v.is_null() || v.as_str() == Some(""));
            let required: Vec<&str> = m.config["required"].as_array().into_iter().flatten().filter_map(|v| v.as_str()).collect();
            let req = requests.iter().find(|r| r.instance_id == inst.id);
            let keys: Vec<String> = secret_keys(&m.config)
                .into_iter()
                .filter(|k| req.is_some_and(|r| r.fields.contains(k)) || (required.contains(&k.as_str()) && empty(k)))
                .collect();
            if keys.is_empty() {
                continue;
            }
            let fields: Vec<Value> = keys
                .iter()
                .map(|k| {
                    let p = &m.config["properties"][k];
                    json!({
                        "key": k, "title": p["title"], "titleEn": p["x-en"]["title"], "description": p["description"],
                        "descriptionEn": p["x-en"]["description"], "missing": empty(k),
                    })
                })
                .collect();
            out.push(json!({
                "instanceId": inst.id, "name": inst.name, "pluginId": inst.plugin_id, "enabled": inst.enabled,
                "requestId": req.map(|r| r.id.clone()), "reason": req.map(|r| r.reason.clone()), "fields": fields,
            }));
        }
        out
    }

    /// Instances a Bot stopped using that never became usable (disabled and still missing a required setting, such as
    /// one it created and then replaced with a better one) and no other Bot uses: nobody would ever fill them in
    async fn drop_unfinished(&self, before: &[String]) {
        let used: HashSet<String> = self.store.bots().unwrap_or_default().iter().flat_map(|b| bot_instances(&b.config)).collect();
        for inst in self.store.instances().unwrap_or_default() {
            if !before.contains(&inst.id) || used.contains(&inst.id) || inst.enabled {
                continue;
            }
            let Some(m) = self.manifest(&inst.plugin_id) else { continue };
            if plugins::validate_config(&m.config, &inst.config, true).is_ok() {
                continue;
            }
            self.stop_instance(&inst.id);
            let _ = self.store.delete_instance(&inst.id);
            let _ = self.prune_instance(&inst.id);
        }
    }

    /// Save a checked configuration: its new plugin instances first (one still missing a secret is created disabled,
    /// for the owner to fill in), then the Bot with the "new:" refs swapped for their ids. Returns the saved
    /// configuration and the instances waiting for secrets
    async fn apply_config(&self, mut c: BotConfig, instances: &[Value], origin: &str) -> Result<(BotConfig, Vec<Value>)> {
        let mut ids: HashMap<String, String> = HashMap::new();
        let mut pending = vec![];
        for i in instances {
            let pid = i["pluginId"].as_str().unwrap_or("").to_string();
            let m = self.manifest(&pid).ok_or_else(|| anyhow!("插件不存在: {pid}"))?;
            let config = i["config"].clone();
            let missing: Vec<String> = m.config["required"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_str())
                .filter(|k| config.get(*k).map_or(true, |v| v.is_null() || v.as_str() == Some("")))
                .map(str::to_string)
                .collect();
            let name = i["name"].as_str().unwrap_or("").to_string();
            let inst = Instance { id: String::new(), plugin_id: pid, name, config, enabled: missing.is_empty() };
            let id = self.save_instance(inst).await?;
            if !missing.is_empty() {
                let name = self.store.instances()?.into_iter().find(|x| x.id == id).map(|x| x.name).unwrap_or_default();
                pending.push(json!({"id": id, "name": name, "missing": missing}));
            }
            ids.insert(i["ref"].as_str().unwrap_or("").to_string(), id);
        }
        let map = |x: &mut String| {
            if let Some(id) = ids.get(x.as_str()) {
                *x = id.clone();
            }
        };
        c.triggers.iter_mut().for_each(map);
        c.notify.iter_mut().for_each(map);
        c.allow.iter_mut().for_each(|a| map(&mut a.instance));
        let before = self.store.bot(&c.id).map(|b| bot_instances(&b.config)).unwrap_or_default();
        let saved = self.save(c, origin).await?;
        self.drop_unfinished(&before).await;
        self.changed();
        if !pending.is_empty() {
            if let Ok(h) = self.hub() {
                h.emit_event(json!({"type": "bots.secretsNeeded", "botId": saved.id, "name": saved.name, "pending": pending}));
            }
        }
        Ok((saved, pending))
    }
}

/// Whether the items waiting in an inbox are all the owner's own words
fn items_are_owner(items: &[Inbox]) -> bool {
    !items.is_empty() && items.iter().filter(|i| i.wake).all(|i| i.owner)
}

/// A name for a new Bot until it picks one: the start of its role
fn default_name(role: &str) -> String {
    let first = role.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("Bot");
    let name: String = first.chars().take(16).collect();
    let name = name.trim_end_matches(|c: char| c.is_ascii_punctuation() || "，。；：！？、".contains(c)).to_string();
    if first.chars().count() > 16 { format!("{name}…") } else { name }
}

/// JSON merge patch (RFC 7386, except that null sets null rather than removing): objects merge, everything else replaces
fn merge_patch(target: &mut Value, patch: &Value) {
    match (target.as_object_mut(), patch.as_object()) {
        (Some(t), Some(p)) => {
            for (k, v) in p {
                match t.get_mut(k) {
                    Some(old) if old.is_object() && v.is_object() => merge_patch(old, v),
                    _ => {
                        t.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        _ => *target = patch.clone(),
    }
}

/// What in a configuration change needs the owner's confirmation: acting on its own, who may ask, permission modes
fn sensitive_changes(old: &BotConfig, new: &BotConfig) -> Vec<String> {
    let mut items = vec![];
    if new.accept != old.accept && new.accept == Accept::Auto {
        items.push("accept".to_string());
    }
    if new.allow != old.allow {
        items.push("allow".to_string());
    }
    if new.bot.permission_mode != old.bot.permission_mode || new.task.permission_mode != old.task.permission_mode {
        items.push("permission".to_string());
    }
    if (old.approval.reply && !new.approval.reply) || (old.approval.write && !new.approval.write) {
        items.push("approval".to_string());
    }
    if !old.task.computer_use && new.task.computer_use {
        items.push("computerUse".to_string());
    }
    items
}

/// Whether the Bot takes plugin events now
fn in_active_hours(c: &BotConfig) -> bool {
    c.active_hours.as_ref().map_or(true, |h| h.contains(chrono::Local::now().naive_local()))
}

/// Tokens a session used that count toward the daily reminder: input, output and cache writes (cache reads are
/// cheap and would swamp the number)
fn session_tokens(usage: &Value) -> u64 {
    ["inputTokens", "outputTokens", "cacheWriteTokens"].iter().map(|k| usage[*k].as_u64().unwrap_or(0)).sum()
}

fn fmt_tokens(n: u64) -> String {
    match n {
        n if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1e6),
        n if n >= 1_000 => format!("{:.0}k", n as f64 / 1e3),
        n => n.to_string(),
    }
}

/// Plugin instances a Bot uses: its triggers, notify targets and allow list
fn bot_instances(c: &BotConfig) -> Vec<String> {
    let mut v: Vec<String> = c.triggers.iter().chain(c.notify.iter()).cloned().chain(c.allow.iter().map(|a| a.instance.clone())).collect();
    v.sort();
    v.dedup();
    v
}

/// Settings marked x-secret in a plugin's configuration schema
fn secret_keys(schema: &Value) -> Vec<String> {
    schema["properties"].as_object().into_iter().flatten().filter(|(_, p)| p["x-secret"] == true).map(|(k, _)| k.clone()).collect()
}

/// For the Bot: new plugin configurations that wait for the owner's secrets
fn secrets_note(pending: &[Value]) -> String {
    if pending.is_empty() {
        return String::new();
    }
    let names: Vec<String> = pending
        .iter()
        .map(|p| {
            let missing = p["missing"].as_array().map(|m| m.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>().join(", ")).unwrap_or_default();
            format!("{} (needs {missing})", p["name"].as_str().unwrap_or(""))
        })
        .collect();
    format!(
        "\nThese new plugin configurations stay disabled until the owner enters their secrets in the card the app shows \
         in this session (you never see the values; you get a message when they are saved): {}. Tell the owner in one \
         sentence to fill in the card; never ask for the secret in the chat.",
        names.join("; ")
    )
}

/// Start of the local day `days_ago` days back, in ms
fn local_midnight(days_ago: i64) -> i64 {
    let day = chrono::Local::now().date_naive() - chrono::Duration::days(days_ago);
    let midnight = day.and_hms_opt(0, 0, 0).unwrap();
    chrono::TimeZone::from_local_datetime(&chrono::Local, &midnight).earliest().map(|d| d.timestamp_millis()).unwrap_or(0)
}

fn local_day(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).map(|t| t.with_timezone(&chrono::Local).format("%Y-%m-%d").to_string()).unwrap_or_default()
}

/// Per-day counts of events (by disposition) and tasks (by state) over the last `days` days, oldest first,
/// plus today's use of the task budget
fn stats(store: &Store, bot_id: &str, tasks_per_day: u32, days: i64) -> Result<Value> {
    let (events, tasks) = store.activity_since(bot_id, local_midnight(days - 1))?;
    let mut out: Vec<(String, serde_json::Map<String, Value>, serde_json::Map<String, Value>)> =
        (0..days).rev().map(|i| (local_day(local_midnight(i)), Default::default(), Default::default())).collect();
    let bump = |m: &mut serde_json::Map<String, Value>, k: &str| {
        let n = m.get(k).and_then(Value::as_u64).unwrap_or(0);
        m.insert(k.to_string(), json!(n + 1));
    };
    let mut task_states = serde_json::Map::new();
    let mut disps = serde_json::Map::new();
    for (at, d) in &events {
        if let Some(day) = out.iter_mut().find(|x| x.0 == local_day(*at)) {
            bump(&mut day.1, d);
        }
        bump(&mut disps, d);
    }
    for (at, st) in &tasks {
        if let Some(day) = out.iter_mut().find(|x| x.0 == local_day(*at)) {
            bump(&mut day.2, st);
        }
        bump(&mut task_states, st);
    }
    let today = local_midnight(0);
    Ok(json!({
        "tasksPerDay": tasks_per_day,
        "tasksToday": tasks.iter().filter(|(at, _)| *at >= today).count(),
        "events": disps,
        "tasks": task_states,
        "days": out.into_iter().map(|(day, e, t)| json!({"day": day, "events": e, "tasks": t})).collect::<Vec<_>>(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_last_message_of_a_turn_is_its_trailing_text() {
        let ev = |kind: &str, text: &str| json!({"kind": kind, "text": text});
        let events = [
            ev("user_message", "fix it"),
            ev("assistant_text", "Looking."),
            ev("tool_call", ""),
            ev("tool_result", ""),
            ev("assistant_text", "Fixed the parser."),
            ev("usage", ""),
            ev("assistant_text", "Should I also update the docs?"),
            ev("turn_end", ""),
        ];
        assert_eq!(final_text(&events), "Fixed the parser.\n\nShould I also update the docs?");
        // A turn that ended right after a tool call said nothing at the end
        assert_eq!(final_text(&events[..4]), "");
        // Text of the turn before does not count
        let next = [ev("assistant_text", "old"), ev("turn_end", ""), ev("turn_end", "")];
        assert_eq!(final_text(&next), "");
        assert_eq!(clip("short"), "short");
        let long = "字".repeat(REPORT_CHARS + 10);
        let c = clip(&long);
        assert!(c.starts_with(&"字".repeat(REPORT_CHARS)));
        assert!(c.ends_with(&format!("[{} characters in all; task_status gives the full text]", REPORT_CHARS + 10)));
    }

    #[test]
    fn cards_escape_attributes_and_cut_long_text() {
        let e = EventRow {
            id: "ev_1".into(),
            instance_id: "i".into(),
            thread: "t\"1".into(),
            sender: "u1".into(),
            sender_name: "Ann".into(),
            text: "x".repeat(3000),
            at: 0,
            ..Default::default()
        };
        let c = card(&e, false, "i (cron)");
        assert!(c.contains("thread=\"t'1\""));
        assert!(c.contains("sender=\"Ann (u1)\""));
        assert!(c.contains("kind=\"ambient\""));
        assert!(c.chars().count() < 2300);
    }

    #[test]
    fn brief_quotes_the_request_and_the_worktree() {
        let t = TaskRow {
            id: "task_1".into(),
            title: "Fix".into(),
            workspace: "/repo".into(),
            worktree: Some(json!({"branch": "yi/task-1"})),
            spec: json!({"note": "see #3", "request": "please fix the typo"}),
            ..Default::default()
        };
        let b = brief(&t, &[], &|i| i.to_string());
        assert!(b.contains("please fix the typo") && b.contains("yi/task-1") && b.contains("see #3"));
    }

    #[test]
    fn config_changes_merge_and_sensitive_ones_wait() {
        let old: BotConfig = serde_json::from_value(json!({
            "name": "x", "bot": {"host": "local", "tool": "claude"},
            "workspaces": [{"host": "local", "cwd": "/a"}], "prompt": {"role": "r", "delivery": "d"},
        }))
        .unwrap();
        let mut v = serde_json::to_value(&old).unwrap();
        merge_patch(&mut v, &json!({"name": "y", "prompt": {"role": "r2"}, "workspaces": [{"host": "local", "cwd": "/b"}]}));
        let new: BotConfig = serde_json::from_value(v).unwrap();
        assert_eq!((new.name.as_str(), new.prompt.role.as_str(), new.prompt.delivery.as_str()), ("y", "r2", "d"));
        assert_eq!(new.workspaces.len(), 1);
        assert!(sensitive_changes(&old, &new).is_empty());
        let mut auto = new.clone();
        auto.accept = Accept::Auto;
        auto.task.permission_mode = "default".into();
        assert_eq!(sensitive_changes(&old, &auto), ["accept", "permission"]);
        // Going back to observe is never held up
        assert!(sensitive_changes(&auto, &new).contains(&"permission".to_string()));
        assert!(!sensitive_changes(&auto, &new).contains(&"accept".to_string()));
    }

    #[test]
    fn a_new_bot_is_named_after_its_role_and_its_first_turn_is_the_owners() {
        assert_eq!(default_name("每天早上汇总昨天的提交记录并发给我"), "每天早上汇总昨天的提交记录并发给…");
        assert_eq!(default_name("修 bug。\n其余不管"), "修 bug");
        assert_eq!(default_name("Triage issues"), "Triage issues");
        assert_eq!(default_name("\n  "), "Bot");
        let item = |wake, owner| Inbox { text: String::new(), events: vec![], wake, owner, run: None };
        assert!(items_are_owner(&[item(true, true), item(false, false)]));
        assert!(!items_are_owner(&[item(true, true), item(true, false)]));
        assert!(!items_are_owner(&[]));
    }

    #[test]
    fn turning_approval_off_needs_confirmation_but_turning_it_on_does_not() {
        let mut old: BotConfig = serde_json::from_value(json!({
            "name": "x", "bot": {"host": "local", "tool": "claude"}, "approval": {"reply": true, "write": true}
        }))
        .unwrap();
        let mut new = old.clone();
        new.approval.write = false;
        assert_eq!(sensitive_changes(&old, &new), ["approval"]);
        old.approval.write = false;
        new.approval.write = true;
        assert!(sensitive_changes(&old, &new).is_empty());
    }

    #[test]
    fn enabling_computer_use_needs_confirmation_and_host_support_matches_platforms() {
        let old: BotConfig = serde_json::from_value(json!({
            "name": "x", "bot": {"host": "local", "tool": "claude"}
        }))
        .unwrap();
        let mut new = old.clone();
        new.task.computer_use = true;
        assert_eq!(sensitive_changes(&old, &new), ["computerUse"]);
        // Turning it off is never held up
        assert!(sensitive_changes(&new, &old).is_empty());
        assert_eq!(host_supports_computer_use(Some("windows"), Some("x86_64")), Some(true));
        assert_eq!(host_supports_computer_use(Some("windows"), Some("aarch64")), Some(false));
        assert_eq!(host_supports_computer_use(Some("macos"), Some("aarch64")), Some(true));
        assert_eq!(host_supports_computer_use(Some("macos"), Some("x86_64")), Some(true));
        assert_eq!(host_supports_computer_use(Some("linux"), Some("x86_64")), Some(false));
        assert_eq!(host_supports_computer_use(None, None), None);
    }

    #[tokio::test]
    async fn host_desktop_mutex_blocks_concurrent_computer_use_tasks_only() {
        let dir = std::env::temp_dir().join(format!("yi-bots-cu-{}", short_id("t")));
        let _ = std::fs::create_dir_all(&dir);
        let store = Store::open(&dir.join("bots.db")).unwrap();
        let engine = Engine::for_tests(store);
        let mk = |id: &str, host: &str, state: &str, cu: bool| TaskRow {
            id: id.into(),
            bot_id: "b1".into(),
            host: host.into(),
            session_id: Some(format!("s-{id}")),
            state: state.into(),
            title: id.into(),
            spec: json!({"computerUse": cu}),
            created_at: 1,
            ..Default::default()
        };
        engine.store.save_task(&mk("t1", "win", "needsYou", true)).unwrap();
        engine.store.save_task(&mk("t2", "win", "working", false)).unwrap();
        // t1 holds `win`'s desktop even while waiting for a person in needsYou
        assert_eq!(engine.desktop_holder("win", Some("t3")).unwrap().unwrap().id, "t1");
        assert!(engine.desktop_holder("win", Some("t1")).unwrap().is_none());
        assert!(engine.desktop_holder("mac", Some("t3")).unwrap().is_none());
        // reopen clears blocked state on needsYou tasks too
        let mut t1 = engine.store.task("t1").unwrap();
        t1.last_report = Some(json!({"state": "blocked", "question": "unlock"}));
        engine.store.save_task(&t1).unwrap();
        engine.reopen(&mut t1).unwrap();
        assert_eq!(t1.state, "working");
        assert!(t1.last_report.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn token_counts_skip_cache_reads() {
        let u = json!({"inputTokens": 1000, "outputTokens": 500, "cacheReadTokens": 90000, "cacheWriteTokens": 200});
        assert_eq!(session_tokens(&u), 1700);
        assert_eq!(fmt_tokens(1_700), "2k");
        assert_eq!(fmt_tokens(2_500_000), "2.5M");
        assert_eq!(fmt_tokens(12), "12");
    }
}
