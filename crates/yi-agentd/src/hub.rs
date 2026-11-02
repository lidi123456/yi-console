//! Hub: the session manager. Holds the runtime state of every session and handles event numbering, persistence and broadcasting.

use crate::adapters::{self, AdapterCmd, Prompt, PromptImage, StartSpec};
use crate::store::Store;
use crate::util::{now_ms, truncate};
use anyhow::{anyhow, bail, Result};
use yi_proto::*;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use tokio::sync::{broadcast, mpsc};

/// Terminal replay buffer limit
const TERM_BUF_MAX: usize = 2 * 1024 * 1024;

/// Per-image size limit
pub const IMAGE_MAX: usize = 5 * 1024 * 1024;

/// Image sent by the client (base64)
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IncomingImage {
    pub mime_type: String,
    pub data: String,
}

fn image_ext(mime: &str) -> Option<&'static str> {
    match mime {
        "image/png" => Some("png"),
        "image/jpeg" => Some("jpg"),
        "image/gif" => Some("gif"),
        "image/webp" => Some("webp"),
        _ => None,
    }
}

fn uploads_dir(sid: &str) -> PathBuf {
    crate::paths::base_dir().join("uploads").join(sid)
}

/// Session ids are UUIDs we generate: only hex digits and `-`. Used before building paths to block `..`, absolute paths and empty strings
fn safe_sid(sid: &str) -> bool {
    !sid.is_empty() && sid.len() <= 64 && sid.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

/// Regular files pasted / dropped into the input box: saved to `uploads/<session>/files/`, with (1) (2) appended on name clashes, deleted with the session
pub fn attach_file(sid: &str, name: &str, data: &str) -> Result<serde_json::Value> {
    let clean: String = std::path::Path::new(name)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default()
        .chars()
        // Replace spaces too: paths with spaces in @references need quoting, which some tools don't understand
        .map(|c| if c.is_control() || c.is_whitespace() || r#"<>:"/\|?*"#.contains(c) { '_' } else { c })
        .collect();
    let clean = if clean.trim().is_empty() || clean.trim_matches('.').is_empty() { "file".to_string() } else { clean };
    let dir = uploads_dir(sid).join("files");
    std::fs::create_dir_all(&dir)?;
    let (stem, ext) = match clean.rfind('.') {
        Some(i) if i > 0 => (clean[..i].to_string(), clean[i..].to_string()),
        _ => (clean.clone(), String::new()),
    };
    let mut p = dir.join(&clean);
    let mut n = 1;
    while p.exists() {
        p = dir.join(format!("{stem} ({n}){ext}"));
        n += 1;
    }
    crate::fsapi::upload(&p.display().to_string(), data, false)
}

/// Subsequent chunks of a chunked upload: appended to the file `attach_file` just created. Only files already in this session's upload directory are accepted, up to 50MB total
pub fn attach_append(sid: &str, path: &str, data: &str) -> Result<serde_json::Value> {
    use base64::Engine;
    use std::io::Write;
    let p = PathBuf::from(path);
    if p.parent() != Some(uploads_dir(sid).join("files").as_path()) {
        bail!("只能续传到这个会话的上传目录");
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data.as_bytes())
        .map_err(|_| anyhow!("文件数据无效"))?;
    let len = std::fs::metadata(&p).map_err(|_| anyhow!("上传的文件不存在（可能已取消）"))?.len();
    if len + bytes.len() as u64 > crate::fsapi::MAX_RAW {
        bail!("文件超过 50MB，太大了");
    }
    std::fs::OpenOptions::new().append(true).open(&p)?.write_all(&bytes)?;
    Ok(json!({ "path": path, "size": len + bytes.len() as u64 }))
}

fn models_path() -> PathBuf {
    crate::paths::base_dir().join("models.json")
}

/// Message to client connections. When `session_id` is set, it goes only to connections subscribed to that session.
#[derive(Debug, Clone)]
pub struct Outbound {
    pub session_id: Option<String>,
    pub method: &'static str,
    pub params: Value,
}

struct SessionRt {
    info: Mutex<SessionInfo>,
    cmd: Mutex<Option<mpsc::UnboundedSender<AdapterCmd>>>,
    term: Mutex<VecDeque<u8>>,
    pending: Mutex<HashSet<String>>,
    /// Incremented on every adapter start; late events from an old adapter are dropped by generation
    generation: Mutex<u64>,
    /// Models the tool reported on this start
    models: Mutex<Vec<ModelOption>>,
    /// Last reply segment of this turn; its beginning goes into the turn-finished notification
    last_text: Mutex<String>,
    /// Slash commands reported by the tool
    commands: Mutex<Vec<SlashCommand>>,
    /// Messages queued until the current turn ends
    queue: Mutex<VecDeque<Queued>>,
    /// The user clicked interrupt; don't auto-send queued messages after this turn
    interrupted: Mutex<bool>,
    /// Permission mode when the current process started
    launched_mode: Mutex<String>,
    /// Permission mode changed but needs a process restart to apply (Claude switching to allow-all); restart once this turn ends
    restart_pending: Mutex<bool>,
}

struct Queued {
    id: String,
    text: String,
    images: Vec<IncomingImage>,
    /// The user clicked "Send now"
    now: bool,
}

/// Permission modes each tool actually supports (manually kept in sync with the frontend's `permModes`).
/// auto is the tool's own automatic approval: Claude's `--permission-mode auto`, Codex's auto_review
pub fn permission_modes(tool: ToolKind, mode: SessionMode) -> &'static [&'static str] {
    use SessionMode::*;
    use ToolKind::*;
    match (tool, mode) {
        (Claude, _) | (Codex, Structured) => &["default", "acceptEdits", "auto", "plan", "bypass"],
        (Codex, Terminal) => &["default", "auto", "plan", "bypass"],
        (Cursor | Kiro, Structured) => &["default", "acceptEdits", "plan", "bypass"],
        (Cursor, Terminal) => &["default", "plan", "bypass"],
        (Kiro, Terminal) => &["default", "bypass"],
        // Qoder has no plan mode
        (Qoder, _) => &["default", "acceptEdits", "bypass"],
        (Codebuddy, _) => &["default", "acceptEdits", "plan", "bypass"],
        (Shell, _) => &[],
    }
}

/// Terminal sessions wait for the page to report its real size before starting (up to 3 s): starting at a default size and resizing right after
/// makes full-screen programs like cursor-agent and Claude Code redraw by clearing at the old width, leaving ghost artifacts
async fn start_when_sized(mut spec: StartSpec, em: Emitter, mut rx: mpsc::UnboundedReceiver<AdapterCmd>) {
    let mut early = vec![];
    let deadline = tokio::time::sleep(std::time::Duration::from_secs(3));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => break,
            cmd = rx.recv() => match cmd {
                Some(AdapterCmd::PtyResize { cols, rows }) => {
                    spec.cols = cols;
                    spec.rows = rows;
                    break;
                }
                Some(AdapterCmd::Kill) | None => return em.exited(None),
                Some(other) => early.push(other),
            },
        }
    }
    let (tx, rx2) = mpsc::unbounded_channel();
    for c in early {
        let _ = tx.send(c);
    }
    tokio::spawn(async move {
        while let Some(c) = rx.recv().await {
            if tx.send(c).is_err() {
                break;
            }
        }
    });
    adapters::start(spec, em, rx2);
}

pub struct Hub {
    pub store: Store,
    sessions: Mutex<HashMap<String, Arc<SessionRt>>>,
    pub bus: broadcast::Sender<Arc<Outbound>>,
    pub settings: RwLock<DaemonSettings>,
    pub shutdown: tokio::sync::Notify,
    /// Each tool's most recently reported model list (persisted, used for new sessions)
    tool_models: Mutex<HashMap<String, Vec<ModelOption>>>,
    /// Each tool's most recently reported slash commands (memory only)
    tool_commands: Mutex<HashMap<String, Vec<SlashCommand>>>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateParams {
    pub tool: String,
    pub cwd: String,
    #[serde(default)]
    pub mode: Option<SessionMode>,
    #[serde(default)]
    pub title: Option<String>,
    /// Continue an existing tool session (history import)
    #[serde(default)]
    pub resume_id: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    /// default / acceptEdits / bypass / plan
    #[serde(default)]
    pub permission_mode: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    /// Custom command line for the shell tool or terminal mode
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub cols: Option<u16>,
    #[serde(default)]
    pub rows: Option<u16>,
    /// Bottom terminal panel: the chat session it belongs to
    #[serde(default)]
    pub parent_id: Option<String>,
}

impl Hub {
    pub fn new(store: Store, settings: DaemonSettings) -> Arc<Hub> {
        let (bus, _) = broadcast::channel(4096);
        let hub = Arc::new(Hub {
            store,
            sessions: Mutex::new(HashMap::new()),
            bus,
            settings: RwLock::new(settings),
            shutdown: tokio::sync::Notify::new(),
            tool_models: Mutex::new(
                std::fs::read(models_path())
                    .ok()
                    .and_then(|b| serde_json::from_slice(&b).ok())
                    .unwrap_or_default(),
            ),
            tool_commands: Mutex::new(HashMap::new()),
        });
        hub.restore();
        hub
    }

    /// After a daemon restart, the old tool processes are all gone, so mark everything exited
    fn restore(&self) {
        let Ok(list) = self.store.load_sessions() else {
            return;
        };
        let mut map = self.sessions.lock().unwrap();
        for mut info in list {
            info.last_seq = self.store.max_seq(&info.id).unwrap_or(info.last_seq);
            if info.status != SessionStatus::Exited || !info.queued.is_empty() {
                info.status = SessionStatus::Exited;
                info.pending_permissions = 0;
                info.queued.clear();
                let _ = self.store.save_session(&info);
            }
            map.insert(info.id.clone(), Arc::new(new_rt(info)));
        }
    }

    fn get(&self, id: &str) -> Result<Arc<SessionRt>> {
        self.sessions
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow!("会话不存在: {id}"))
    }

    pub fn list(&self) -> Vec<SessionInfo> {
        let mut v: Vec<SessionInfo> = self
            .sessions
            .lock()
            .unwrap()
            .values()
            .map(|s| s.info.lock().unwrap().clone())
            .collect();
        v.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        v
    }

    pub fn info(&self, id: &str) -> Result<SessionInfo> {
        Ok(self.get(id)?.info.lock().unwrap().clone())
    }

    pub fn create(self: &Arc<Self>, p: CreateParams) -> Result<SessionInfo> {
        let tool = ToolKind::parse(&p.tool).ok_or_else(|| anyhow!("未知工具: {}", p.tool))?;
        let mode = if tool == ToolKind::Shell {
            SessionMode::Terminal
        } else {
            p.mode.unwrap_or(SessionMode::Structured)
        };
        let cwd = crate::paths::expand(&p.cwd);
        if !cwd.is_dir() {
            bail!("目录不存在: {}", cwd.display());
        }
        let now = now_ms();
        let info = SessionInfo {
            id: uuid::Uuid::new_v4().to_string(),
            tool,
            mode,
            cwd: cwd.display().to_string(),
            title: p.title.clone().unwrap_or_default(),
            status: SessionStatus::Starting,
            external_id: p.resume_id.clone(),
            created_at: now,
            updated_at: now,
            last_seq: 0,
            pending_permissions: 0,
            usage: Usage::default(),
            model: p.model.clone(),
            command: p.command.clone(),
            context_percent: None,
            permission_mode: p
                .permission_mode
                .clone()
                .filter(|m| permission_modes(tool, mode).contains(&m.as_str())),
            effort: p.effort.clone().filter(|e| !e.is_empty()),
            pinned: false,
            queued: vec![],
            background_tasks: vec![],
            parent_id: p.parent_id.clone(),
            synced_users: None,
        };
        self.store.save_session(&info)?;
        let rt = Arc::new(new_rt(info.clone()));
        self.sessions
            .lock()
            .unwrap()
            .insert(info.id.clone(), rt.clone());
        self.broadcast_updated(&info);

        // Continuing a history session: for tools readable offline (claude/codex), import the history into the event log first;
        // for those that aren't (ACP tools), let the tool replay it on session/load
        let mut replay = p.resume_id.is_some();
        if let Some(rid) = &p.resume_id {
            if mode == SessionMode::Structured && matches!(tool, ToolKind::Claude | ToolKind::Codex) {
                if let Ok(evs) = crate::history::read(tool, rid) {
                    for ev in evs {
                        self.emit(&info.id, 0, ev);
                    }
                    replay = false;
                }
            }
        }

        let spec = self.spec_for(&info, replay, p.cols.unwrap_or(120), p.rows.unwrap_or(32))?;
        self.launch(&rt, spec)?;
        if let Some(prompt) = p.prompt.filter(|s| !s.trim().is_empty()) {
            self.send(&info.id, &prompt, vec![], false)?;
        }
        Ok(self.info(&info.id)?)
    }

    fn spec_for(
        &self,
        info: &SessionInfo,
        replay: bool,
        cols: u16,
        rows: u16,
    ) -> Result<StartSpec> {
        let settings = self.settings.read().unwrap().clone();
        let program = crate::tools::resolve(info.tool, &settings)
            .ok_or_else(|| anyhow!("这台机器上没有找到 {}", info.tool.as_str()))?;
        // Claude in the terminal: pin the session id on first launch, and continue that session on later restarts.
        // If it was opened and exited without anything said, Claude leaves no record, so start a new one with the same id
        let mut resume = info.external_id.clone();
        let mut session_id = None;
        if info.mode == SessionMode::Terminal && crate::history::pins_session_id(info.tool) {
            match &info.external_id {
                Some(id) if crate::history::tool_session_exists(info.tool, id) => {}
                Some(id) => {
                    resume = None;
                    session_id = Some(id.clone());
                }
                None => {
                    let id = uuid::Uuid::new_v4().to_string();
                    self.set_external_id(&info.id, &id);
                    session_id = Some(id);
                }
            }
        }
        let mut extra_args = crate::tools::extra_args(info.tool, &settings);
        if info.mode == SessionMode::Terminal && info.tool == ToolKind::Codex && crate::tools::codex_no_daemon(&program) {
            extra_args.push("--no-daemon".into());
        }
        Ok(StartSpec {
            tool: info.tool,
            mode: info.mode,
            cwd: PathBuf::from(&info.cwd),
            resume,
            session_id,
            replay,
            replay_skip: 0,
            model: info.model.clone(),
            effort: info.effort.clone(),
            permission_mode: info.permission_mode.clone().unwrap_or_else(|| "default".into()),
            command: info.command.clone(),
            program,
            extra_args,
            cols,
            rows,
            tmux: (info.mode == SessionMode::Terminal || info.tool == ToolKind::Shell)
                .then_some(())
                .filter(|_| settings.terminal_tmux.unwrap_or(true) && crate::tmux::program().is_some())
                .map(|_| crate::tmux::session_name(&info.id)),
        })
    }

    fn launch(self: &Arc<Self>, rt: &Arc<SessionRt>, spec: StartSpec) -> Result<()> {
        let sid = rt.info.lock().unwrap().id.clone();
        if spec.mode == SessionMode::Structured && spec.tool != ToolKind::Shell {
            self.make_room(&sid);
        }
        let gen = {
            let mut g = rt.generation.lock().unwrap();
            *g += 1;
            *g
        };
        let (tx, rx) = mpsc::unbounded_channel();
        *rt.cmd.lock().unwrap() = Some(tx);
        *rt.launched_mode.lock().unwrap() = spec.permission_mode.clone();
        *rt.restart_pending.lock().unwrap() = false;
        let em = Emitter {
            hub: self.clone(),
            sid,
            gen,
        };
        em.status(SessionStatus::Starting, None);
        if spec.mode == SessionMode::Terminal || spec.tool == ToolKind::Shell {
            tokio::spawn(start_when_sized(spec, em, rx));
        } else {
            adapters::start(spec, em, rx);
        }
        Ok(())
    }

    fn running_tx(&self, rt: &SessionRt) -> Option<mpsc::UnboundedSender<AdapterCmd>> {
        let g = rt.cmd.lock().unwrap();
        g.as_ref().filter(|t| !t.is_closed()).cloned()
    }

    /// Send a user message; resumes automatically if the process has exited.
    /// With `queue` true and the turn still running: Claude and Codex get it directly and read it after their next tool call
    /// (anything that misses that is handled by the tool after the turn ends); other tools queue it and send in order after the turn
    pub fn send(self: &Arc<Self>, id: &str, text: &str, images: Vec<IncomingImage>, queue: bool) -> Result<()> {
        let rt = self.get(id)?;
        let info = rt.info.lock().unwrap().clone();
        if info.mode == SessionMode::Terminal && !images.is_empty() {
            bail!("终端模式不支持发送图片");
        }
        let busy = matches!(info.status, SessionStatus::Running | SessionStatus::WaitingPermission);
        // If the process must restart at turn end (permission mode change), messages the tool hasn't read would be lost with it, so queue in that case too
        let steer = matches!(info.tool, ToolKind::Claude | ToolKind::Codex)
            && rt.queue.lock().unwrap().is_empty()
            && !*rt.restart_pending.lock().unwrap();
        if queue && busy && !steer && info.mode == SessionMode::Structured && self.running_tx(&rt).is_some() {
            rt.queue.lock().unwrap().push_back(Queued {
                id: uuid::Uuid::new_v4().simple().to_string()[..12].to_string(),
                text: text.to_string(),
                images,
                now: false,
            });
            self.sync_queue(&rt);
            return Ok(());
        }
        // Validate and save images first, so a failure doesn't start the process
        let (refs, prompt_images) = self.save_images(id, images)?;
        let tx = match self.running_tx(&rt) {
            Some(tx) => tx,
            None => {
                let spec = self.spec_for(&info, false, 120, 32)?;
                self.launch(&rt, spec)?;
                self.running_tx(&rt).ok_or_else(|| anyhow!("启动失败"))?
            }
        };
        if info.mode == SessionMode::Terminal {
            let mut data = text.as_bytes().to_vec();
            data.push(b'\r');
            tx.send(AdapterCmd::PtyInput(data))
                .map_err(|_| anyhow!("会话已结束"))?;
            return Ok(());
        }
        self.emit(
            id,
            0,
            EventBody::UserMessage {
                text: text.to_string(),
                images: refs,
            },
        );
        tx.send(AdapterCmd::Send(Prompt {
            text: text.to_string(),
            images: prompt_images,
        }))
        .map_err(|_| anyhow!("会话已结束"))?;
        Ok(())
    }

    fn save_images(&self, sid: &str, images: Vec<IncomingImage>) -> Result<(Vec<ImageRef>, Vec<PromptImage>)> {
        use base64::Engine;
        let mut refs = vec![];
        let mut out = vec![];
        if images.is_empty() {
            return Ok((refs, out));
        }
        let dir = uploads_dir(sid);
        std::fs::create_dir_all(&dir)?;
        for img in images {
            let ext = image_ext(&img.mime_type)
                .ok_or_else(|| anyhow!("不支持的图片格式 {}，请用 PNG / JPEG / GIF / WebP", img.mime_type))?;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(img.data.as_bytes())
                .map_err(|_| anyhow!("图片数据无效"))?;
            if bytes.len() > IMAGE_MAX {
                bail!("图片超过 5MB（{:.1}MB），请压缩后再发", bytes.len() as f64 / 1048576.0);
            }
            let iid = uuid::Uuid::new_v4().simple().to_string();
            let path = dir.join(format!("{iid}.{ext}"));
            std::fs::write(&path, &bytes)?;
            refs.push(ImageRef {
                id: format!("{iid}.{ext}"),
                mime_type: img.mime_type.clone(),
            });
            out.push(PromptImage {
                mime_type: img.mime_type,
                path,
                data: img.data,
            });
        }
        Ok((refs, out))
    }

    /// Read an image from a user message
    pub fn image(&self, sid: &str, image_id: &str) -> Result<Value> {
        use base64::Engine;
        self.get(sid)?;
        // We generate ids (hex + extension); reject any other characters to prevent path traversal
        let ok = !image_id.is_empty()
            && image_id.chars().all(|c| c.is_ascii_alphanumeric() || c == '.')
            && !image_id.starts_with('.');
        if !ok {
            bail!("图片 id 无效");
        }
        let ext = image_id.rsplit('.').next().unwrap_or("");
        let mime = match ext {
            "png" => "image/png",
            "jpg" => "image/jpeg",
            "gif" => "image/gif",
            "webp" => "image/webp",
            _ => bail!("图片 id 无效"),
        };
        let bytes = std::fs::read(uploads_dir(sid).join(image_id)).map_err(|_| anyhow!("图片不存在"))?;
        Ok(json!({
            "mimeType": mime,
            "data": base64::engine::general_purpose::STANDARD.encode(bytes),
        }))
    }

    /// Switch model: recorded on the session (survives restarts), and the tool is told if the process is running
    pub fn set_model(&self, id: &str, model: &str) -> Result<()> {
        let rt = self.get(id)?;
        let info = {
            let mut i = rt.info.lock().unwrap();
            if i.mode == SessionMode::Terminal {
                bail!("终端模式不支持切换模型");
            }
            i.model = Some(model.to_string()).filter(|m| !m.is_empty());
            i.clone()
        };
        self.store.save_session(&info)?;
        self.broadcast_updated(&info);
        if let Some(tx) = self.running_tx(&rt) {
            let _ = tx.send(AdapterCmd::SetModel(model.to_string()));
        }
        Ok(())
    }

    /// Switch thinking level: recorded on the session, and the tool is told if running (immediate for Claude, next turn for Codex)
    /// Compact context: every tool understands /compact (the adapter turns it into thread/compact/start for Codex), sent as a normal message
    pub fn compact(self: &Arc<Self>, id: &str) -> Result<()> {
        let info = self.get(id)?.info.lock().unwrap().clone();
        if info.mode == SessionMode::Terminal || info.tool == ToolKind::Shell {
            bail!("终端模式请直接在终端里输入 /compact");
        }
        self.send(id, "/compact", vec![], true)
    }

    /// /clear: end the tool process, delete the chat history, reset usage and forget the tool session id; the next message starts a new tool session.
    /// The Yi Console session (id, title, directory, model, permission mode, bottom terminal) stays; a reply in progress is interrupted
    pub async fn clear(self: &Arc<Self>, id: &str) -> Result<()> {
        let rt = self.get(id)?;
        {
            let i = rt.info.lock().unwrap();
            if i.mode == SessionMode::Terminal || i.tool == ToolKind::Shell {
                bail!("终端模式请直接在终端里输入 /clear");
            }
        }
        // Invalidate the generation first, so late events and exit status from the old process are no longer recorded on the session
        let old = rt.cmd.lock().unwrap().take();
        *rt.generation.lock().unwrap() += 1;
        if let Some(tx) = old {
            let _ = tx.send(AdapterCmd::Kill);
            for _ in 0..40 {
                if tx.is_closed() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        }
        let info = {
            let mut i = rt.info.lock().unwrap();
            i.external_id = None;
            i.synced_users = None;
            i.usage = Default::default();
            i.context_percent = None;
            i.status = SessionStatus::Exited;
            i.pending_permissions = 0;
            i.queued.clear();
            i.background_tasks.clear();
            i.clone()
        };
        rt.pending.lock().unwrap().clear();
        rt.queue.lock().unwrap().clear();
        rt.term.lock().unwrap().clear();
        rt.last_text.lock().unwrap().clear();
        *rt.interrupted.lock().unwrap() = false;
        *rt.restart_pending.lock().unwrap() = false;
        self.store.delete_events(id)?;
        self.store.save_session(&info)?;
        // Images and attachments in the chat are only referenced by the deleted records, so delete them too
        if safe_sid(id) {
            let _ = std::fs::remove_dir_all(uploads_dir(id));
        }
        self.broadcast_updated(&info);
        // seq keeps counting up; clients drop earlier events based on this one, and also receive it in reconnect catch-ups
        self.emit(id, 0, EventBody::Cleared {});
        Ok(())
    }

    /// Stop a background task (only Claude supports this for now)
    pub fn stop_task(&self, id: &str, task_id: &str) -> Result<()> {
        let rt = self.get(id)?;
        if !rt.info.lock().unwrap().background_tasks.iter().any(|t| t.id == task_id) {
            bail!("后台任务已经结束");
        }
        let tx = self.running_tx(&rt).ok_or_else(|| anyhow!("会话已结束"))?;
        tx.send(AdapterCmd::StopTask(task_id.to_string())).map_err(|_| anyhow!("会话已结束"))
    }

    pub fn set_effort(&self, id: &str, effort: &str) -> Result<()> {
        let rt = self.get(id)?;
        let info = {
            let mut i = rt.info.lock().unwrap();
            if i.mode == SessionMode::Terminal {
                bail!("终端模式不支持切换思考等级");
            }
            if !matches!(i.tool, ToolKind::Claude | ToolKind::Codex) {
                bail!("{} 不支持调整思考等级", i.tool.as_str());
            }
            i.effort = Some(effort.to_string()).filter(|m| !m.is_empty());
            i.clone()
        };
        self.store.save_session(&info)?;
        self.broadcast_updated(&info);
        if let Some(tx) = self.running_tx(&rt) {
            let _ = tx.send(AdapterCmd::SetEffort(effort.to_string()));
        }
        Ok(())
    }

    /// Models available to a session: those the tool reported on this start, otherwise the tool's last list
    pub fn session_models(&self, id: &str) -> Result<Value> {
        let rt = self.get(id)?;
        let info = rt.info.lock().unwrap().clone();
        let mut models = rt.models.lock().unwrap().clone();
        if models.is_empty() {
            models = self.cached_models(info.tool);
        }
        Ok(json!({ "current": info.model, "models": models }))
    }

    pub fn cached_models(&self, tool: ToolKind) -> Vec<ModelOption> {
        self.tool_models
            .lock()
            .unwrap()
            .get(tool.as_str())
            .cloned()
            .unwrap_or_default()
    }

    pub fn remember_models(&self, tool: ToolKind, models: &[ModelOption]) {
        if models.is_empty() {
            return;
        }
        let mut m = self.tool_models.lock().unwrap();
        if m.get(tool.as_str()).map(|v| v.as_slice()) == Some(models) {
            return;
        }
        m.insert(tool.as_str().to_string(), models.to_vec());
        if let Ok(b) = serde_json::to_vec(&*m) {
            let _ = std::fs::write(models_path(), b);
        }
    }

    /// Restart an exited session (without sending a message), to view an ACP tool's history or reopen a terminal
    pub fn resume(self: &Arc<Self>, id: &str, cols: u16, rows: u16) -> Result<()> {
        let rt = self.get(id)?;
        if self.running_tx(&rt).is_some() {
            return Ok(());
        }
        let info = rt.info.lock().unwrap().clone();
        let replay = info.last_seq == 0 || self.history_missing(&info);
        let spec = self.spec_for(&info, replay, cols, rows)?;
        self.launch(&rt, spec)
    }

    /// When an ACP session taken over from tool history (Cursor, Kiro…) failed its first session/load (e.g. the session was held by another Kiro),
    /// the history was never replayed but the events already contain an error record, so last_seq isn't 0: the tool still needs to replay
    fn history_missing(&self, info: &SessionInfo) -> bool {
        info.mode == SessionMode::Structured
            && !matches!(info.tool, ToolKind::Claude | ToolKind::Codex | ToolKind::Shell)
            && info.external_id.is_some()
            && self.store.count_user_messages(&info.id).is_ok_and(|n| n == 0)
    }

    /// Switch between chat mode and terminal mode, continuing the same tool session.
    /// To terminal: record how many user messages the tool session already has, and resume in the tool's interactive UI;
    /// back to chat: import only what was said in the terminal (Claude / Codex read the tool log; Cursor / Kiro replay and skip the earlier part)
    pub async fn set_mode(self: &Arc<Self>, id: &str, mode: SessionMode) -> Result<()> {
        let rt = self.get(id)?;
        let info = rt.info.lock().unwrap().clone();
        if info.tool == ToolKind::Shell || info.parent_id.is_some() {
            bail!("普通终端没有对话模式");
        }
        if info.mode == mode {
            return Ok(());
        }
        // Codex / Cursor / Kiro in the terminal that haven't recorded a tool session id yet: look it up in the tool history first
        self.guess_terminal_session(id).await;
        // End the current process. Invalidate the generation first so the old process's exit status and code aren't recorded on the session
        let old = rt.cmd.lock().unwrap().take();
        *rt.generation.lock().unwrap() += 1;
        if let Some(tx) = &old {
            let _ = tx.send(AdapterCmd::Kill);
        }
        if info.mode == SessionMode::Terminal {
            // Switched away by the user: the program in tmux ends too; continue once it has exited
            if !crate::tmux::kill_and_wait(&crate::tmux::session_name(id)).await {
                // No tmux: the adapter kills the process on Kill directly; leave a little time to wrap up
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
        } else if let Some(tx) = old {
            // Wait for the chat-mode tool process to exit, so two processes don't write the same tool session at once
            for _ in 0..40 {
                if tx.is_closed() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        }
        let hub = self.clone();
        let id = id.to_string();
        tokio::task::spawn_blocking(move || hub.finish_set_mode(&id, mode)).await?
    }

    fn finish_set_mode(self: &Arc<Self>, id: &str, mode: SessionMode) -> Result<()> {
        let rt = self.get(id)?;
        let mut info = rt.info.lock().unwrap().clone();
        let tool = info.tool;
        let offline = matches!(tool, ToolKind::Claude | ToolKind::Codex);
        let history = |ext: &Option<String>| ext.as_deref().and_then(|e| crate::history::read(tool, e).ok());
        let mut import = vec![];
        let mut skip = 0;
        if mode == SessionMode::Terminal {
            let users = if offline {
                history(&info.external_id).map(|evs| count_users(&evs)).unwrap_or(0)
            } else {
                self.store.count_user_messages(id)? as usize
            };
            info.synced_users = Some(users as u32);
            // A session where nothing was said may have no record on the tool side, and resume would fail; for Claude, spec_for handles it with --session-id
            if users == 0 && !offline {
                info.external_id = None;
            }
        } else {
            skip = info.synced_users.take().unwrap_or(0) as usize;
            if crate::history::pins_session_id(tool)
                && info.external_id.as_deref().is_some_and(|e| !crate::history::tool_session_exists(tool, e))
            {
                // Claude was opened in the terminal but nothing was said, so there's no session to continue
                info.external_id = None;
            }
            if offline {
                import = history(&info.external_id).map(|evs| after_users(evs, skip)).unwrap_or_default();
            }
        }
        if let (ToolKind::Cursor, Some(ext)) = (tool, &info.external_id) {
            if let Err(e) = crate::history::cursor_handoff(ext, &info.cwd, mode == SessionMode::Terminal) {
                crate::warn!("Cursor 会话 {ext} 换到{}失败: {e}", if mode == SessionMode::Terminal { "终端" } else { "对话" });
            }
        }
        info.mode = mode;
        info.permission_mode = info
            .permission_mode
            .filter(|m| permission_modes(tool, mode).contains(&m.as_str()));
        // Mark it exited, so launch is sure to broadcast the new session info when it reports Starting
        info.status = SessionStatus::Exited;
        info.pending_permissions = 0;
        info.queued.clear();
        info.background_tasks.clear();
        rt.pending.lock().unwrap().clear();
        rt.queue.lock().unwrap().clear();
        rt.term.lock().unwrap().clear();
        rt.commands.lock().unwrap().clear();
        *rt.interrupted.lock().unwrap() = false;
        *rt.info.lock().unwrap() = info.clone();
        self.store.save_session(&info)?;

        let label = if mode == SessionMode::Terminal { "终端模式" } else { "对话模式" };
        self.emit(id, 0, EventBody::System { text: format!("已切换到{label}") });
        for ev in import {
            self.emit(id, 0, ev);
        }
        // Cursor / Kiro can't read history offline; have the tool replay via session/load and skip what's already in the chat
        let replay = mode == SessionMode::Structured && !offline && info.external_id.is_some();
        let mut spec = self.spec_for(&info, replay, 120, 32)?;
        spec.replay_skip = skip;
        // In terminal mode, start_when_sized waits for the frontend to report its size before starting the process
        self.launch(&rt, spec)
    }

    /// Sync the queue onto the session info so clients can show it
    fn sync_queue(&self, rt: &SessionRt) {
        let list: Vec<QueuedMessage> = rt
            .queue
            .lock()
            .unwrap()
            .iter()
            .map(|q| QueuedMessage {
                id: q.id.clone(),
                text: q.text.clone(),
                images: q.images.len() as u32,
            })
            .collect();
        let info = {
            let mut i = rt.info.lock().unwrap();
            if i.queued == list {
                return;
            }
            i.queued = list;
            i.clone()
        };
        self.broadcast_updated(&info);
    }

    /// Queued messages: remove recalls one (returns the content; the client puts it back in the input box); sendNow interrupts the current turn and sends it immediately
    pub fn queue_action(self: &Arc<Self>, id: &str, qid: &str, action: &str) -> Result<Value> {
        let rt = self.get(id)?;
        let item = {
            let mut q = rt.queue.lock().unwrap();
            let pos = q.iter().position(|x| x.id == qid).ok_or_else(|| anyhow!("这条消息已经发出去了"))?;
            q.remove(pos).unwrap()
        };
        let text = item.text.clone();
        match action {
            "remove" => {}
            "sendNow" => {
                let status = rt.info.lock().unwrap().status;
                let busy = matches!(status, SessionStatus::Running | SessionStatus::WaitingPermission);
                match self.running_tx(&rt) {
                    Some(tx) if busy => {
                        rt.queue.lock().unwrap().push_front(Queued { now: true, ..item });
                        *rt.interrupted.lock().unwrap() = true;
                        let _ = tx.send(AdapterCmd::Interrupt);
                    }
                    _ => {
                        self.sync_queue(&rt);
                        self.send(id, &item.text, item.images, false)?;
                        return Ok(json!({ "text": text }));
                    }
                }
            }
            _ => bail!("未知操作: {action}"),
        }
        self.sync_queue(&rt);
        Ok(json!({ "text": text }))
    }

    /// Turn ended: restart the process first if needed (permission mode change), then send the next queued message
    fn after_turn(self: &Arc<Self>, sid: &str, gen: u64, stop_reason: Option<&str>) {
        let Some(rt) = self.alive(sid, gen) else {
            return;
        };
        let was_interrupted = std::mem::take(&mut *rt.interrupted.lock().unwrap())
            || matches!(stop_reason, Some("interrupted" | "cancelled"));
        if std::mem::take(&mut *rt.restart_pending.lock().unwrap()) {
            if let Err(e) = self.relaunch(&rt) {
                self.emit(sid, 0, EventBody::Error { message: format!("重启会话失败: {e}") });
            }
        }
        let next = {
            let mut q = rt.queue.lock().unwrap();
            match q.front() {
                Some(f) if f.now || !was_interrupted => q.pop_front(),
                _ => None,
            }
        };
        self.sync_queue(&rt);
        if let Some(item) = next {
            if let Err(e) = self.send(sid, &item.text, item.images, false) {
                self.emit(sid, 0, EventBody::Error { message: format!("发送排队消息失败: {e}") });
            }
        }
    }

    /// End the current process and start it again with the session's current settings (continuing the same tool session)
    fn relaunch(self: &Arc<Self>, rt: &Arc<SessionRt>) -> Result<()> {
        if let Some(tx) = self.running_tx(rt) {
            let _ = tx.send(AdapterCmd::Kill);
        }
        let info = rt.info.lock().unwrap().clone();
        let spec = self.spec_for(&info, false, 120, 32)?;
        self.launch(rt, spec)
    }

    pub fn interrupt(&self, id: &str) -> Result<()> {
        let rt = self.get(id)?;
        let tx = self.running_tx(&rt).ok_or_else(|| anyhow!("会话未在运行"))?;
        *rt.interrupted.lock().unwrap() = true;
        tx.send(AdapterCmd::Interrupt).map_err(|_| anyhow!("会话已结束"))
    }

    /// Switch permission mode: recorded on the session (kept on resume), and the tool switches right away if running
    pub fn set_permission_mode(self: &Arc<Self>, id: &str, mode: &str) -> Result<()> {
        let rt = self.get(id)?;
        let info = {
            let mut i = rt.info.lock().unwrap();
            if i.mode == SessionMode::Terminal {
                bail!("终端模式的审批模式在启动时就定了，要换请新开一个会话");
            }
            if !permission_modes(i.tool, i.mode).contains(&mode) {
                bail!("{} 不支持这个审批模式: {mode}", i.tool.as_str());
            }
            i.permission_mode = Some(mode.to_string());
            i.clone()
        };
        self.store.save_session(&info)?;
        self.broadcast_updated(&info);
        let Some(tx) = self.running_tx(&rt) else {
            return Ok(());
        };
        // Claude can only switch to allow-all if it was started with the skip-permissions flag, so the process must restart
        let launched = rt.launched_mode.lock().unwrap().clone();
        if info.tool == ToolKind::Claude && mode == "bypass" && launched != "bypass" {
            let busy = matches!(info.status, SessionStatus::Running | SessionStatus::WaitingPermission);
            if busy {
                *rt.restart_pending.lock().unwrap() = true;
            } else {
                self.relaunch(&rt)?;
            }
            return Ok(());
        }
        *rt.restart_pending.lock().unwrap() = false;
        let _ = tx.send(AdapterCmd::SetPermissionMode(mode.to_string()));
        Ok(())
    }

    /// Global search: messages whose text contains q, with the session title and some context around the match
    pub fn search(&self, q: &str, limit: usize) -> Result<Value> {
        let hits = self.store.search(q, limit)?;
        let sessions = self.sessions.lock().unwrap();
        let out: Vec<Value> = hits
            .into_iter()
            .filter_map(|(sid, seq, ts, body)| {
                let rt = sessions.get(&sid)?;
                let info = rt.info.lock().unwrap();
                let (kind, text) = match &body {
                    EventBody::UserMessage { text, .. } => ("user", text.as_str()),
                    EventBody::AssistantText { text, .. } => ("assistant", text.as_str()),
                    _ => return None,
                };
                Some(json!({
                    "sessionId": sid, "seq": seq, "ts": ts, "kind": kind,
                    "title": info.title, "tool": info.tool, "cwd": info.cwd,
                    "snippet": around(text, q, 60),
                }))
            })
            .collect();
        Ok(json!(out))
    }

    pub fn pin(&self, id: &str, pinned: bool) -> Result<()> {
        let rt = self.get(id)?;
        let info = {
            let mut i = rt.info.lock().unwrap();
            i.pinned = pinned;
            i.clone()
        };
        self.store.save_session(&info)?;
        self.broadcast_updated(&info);
        Ok(())
    }

    /// Slash commands: those the tool reported on this start, otherwise the tool's last list
    pub fn session_commands(&self, id: &str) -> Result<Vec<SlashCommand>> {
        let rt = self.get(id)?;
        let tool = rt.info.lock().unwrap().tool;
        let c = rt.commands.lock().unwrap().clone();
        if !c.is_empty() {
            return Ok(c);
        }
        Ok(self
            .tool_commands
            .lock()
            .unwrap()
            .get(tool.as_str())
            .cloned()
            .unwrap_or_default())
    }

    pub fn command(&self, id: &str, cmd: AdapterCmd) -> Result<()> {
        let rt = self.get(id)?;
        let tx = self.running_tx(&rt).ok_or_else(|| anyhow!("会话未在运行"))?;
        tx.send(cmd).map_err(|_| anyhow!("会话已结束"))
    }

    /// After a daemon restart, reattach terminal sessions still alive in tmux
    pub fn reattach_terminals(self: &Arc<Self>) {
        let ids: Vec<String> = self
            .sessions
            .lock()
            .unwrap()
            .values()
            .filter_map(|rt| {
                let i = rt.info.lock().unwrap();
                (i.mode == SessionMode::Terminal || i.tool == ToolKind::Shell).then(|| i.id.clone())
            })
            .collect();
        for id in ids {
            if crate::tmux::alive(&crate::tmux::session_name(&id)) {
                crate::info!("接上 tmux 里的终端会话 {id}");
                if let Err(e) = self.resume(&id, 120, 32) {
                    crate::warn!("接上终端会话 {id} 失败: {e}");
                }
            }
        }
    }

    /// Sessions with a tool process open in chat mode (terminal mode and shells don't count: they may contain programs the user started)
    fn live_structured(&self) -> Vec<(Arc<SessionRt>, SessionInfo)> {
        let list: Vec<Arc<SessionRt>> = self.sessions.lock().unwrap().values().cloned().collect();
        list.into_iter()
            .filter(|rt| self.running_tx(rt).is_some())
            .map(|rt| {
                let info = rt.info.lock().unwrap().clone();
                (rt, info)
            })
            .filter(|(_, i)| i.mode == SessionMode::Structured && i.tool != ToolKind::Shell)
            .collect()
    }

    /// Whether this process can be ended without losing anything: not replying, no queued messages, no background tasks.
    /// Idle ones need a tool session id (to reconnect later); errored ones lose nothing by ending;
    /// with `waiting`, those awaiting approval count too (the approval is voided and redone on the next message)
    fn releasable(rt: &SessionRt, info: &SessionInfo, waiting: bool) -> bool {
        if !rt.queue.lock().unwrap().is_empty() || !info.background_tasks.is_empty() {
            return false;
        }
        match info.status {
            SessionStatus::Idle => info.external_id.is_some() && rt.pending.lock().unwrap().is_empty(),
            SessionStatus::Error => true,
            SessionStatus::WaitingPermission => waiting,
            _ => false,
        }
    }

    /// End tool processes idle for too long (each takes about 100–250MB of memory); the session log and tool session id remain,
    /// and send restarts the original session automatically on the next message. Processes waiting too long for approval, or errored but not exited, are ended too
    pub fn release_idle(&self) {
        let minutes = self
            .settings
            .read()
            .unwrap()
            .idle_release_minutes
            .unwrap_or(yi_proto::DEFAULT_IDLE_RELEASE_MINUTES);
        if minutes == 0 {
            return;
        }
        let cutoff = crate::util::now_ms() - minutes as i64 * 60_000;
        for (rt, info) in self.live_structured() {
            if info.updated_at >= cutoff || !Self::releasable(&rt, &info, true) {
                continue;
            }
            if let Some(tx) = self.running_tx(&rt) {
                crate::info!("会话 {} 闲置超过 {minutes} 分钟，结束工具进程", info.id);
                let _ = tx.send(AdapterCmd::Kill);
            }
        }
    }

    /// Before starting a chat-mode tool process for `id`, make sure the number open at once stays within the limit:
    /// when over, end idle processes in order of least recent activity. If all are busy, go over for now and reclaim once they're idle
    fn make_room(&self, id: &str) {
        let max = self
            .settings
            .read()
            .unwrap()
            .max_live_processes
            .unwrap_or(yi_proto::DEFAULT_MAX_LIVE_PROCESSES) as usize;
        if max == 0 {
            return;
        }
        let live: Vec<_> = self.live_structured().into_iter().filter(|(_, i)| i.id != id).collect();
        let over = (live.len() + 1).saturating_sub(max);
        if over == 0 {
            return;
        }
        let mut idle: Vec<_> = live.into_iter().filter(|(rt, i)| Self::releasable(rt, i, false)).collect();
        idle.sort_by_key(|(_, i)| i.updated_at);
        for (rt, info) in idle.into_iter().take(over) {
            if let Some(tx) = self.running_tx(&rt) {
                crate::info!("工具进程已到上限 {max} 个，结束最久没用的会话 {} 的进程", info.id);
                let _ = tx.send(AdapterCmd::Kill);
            }
        }
    }

    pub fn kill(&self, id: &str) -> Result<()> {
        let rt = self.get(id)?;
        if let Some(tx) = self.running_tx(&rt) {
            let _ = tx.send(AdapterCmd::Kill);
        }
        Ok(())
    }

    pub fn delete(&self, id: &str) -> Result<()> {
        // The id comes from the client and is later used in a path for remove_dir_all: only handle sessions that really exist
        self.get(id)?;
        // First delete its terminals in the bottom terminal panel
        let kids: Vec<String> = self
            .sessions
            .lock()
            .unwrap()
            .values()
            .filter_map(|rt| {
                let i = rt.info.lock().unwrap();
                (i.parent_id.as_deref() == Some(id)).then(|| i.id.clone())
            })
            .collect();
        for k in kids {
            let _ = self.delete(&k);
        }
        let _ = self.kill(id);
        // When the process isn't running (e.g. the daemon just restarted and hasn't reattached), the tmux session must be cleaned up too
        crate::tmux::kill(&crate::tmux::session_name(id));
        self.sessions.lock().unwrap().remove(id);
        self.store.delete_session(id)?;
        if safe_sid(id) {
            let _ = std::fs::remove_dir_all(uploads_dir(id));
        }
        self.publish(
            None,
            methods::EV_SESSION_REMOVED,
            json!({ "sessionId": id }),
        );
        Ok(())
    }

    /// Record the tool's own session id (used to resume when terminal mode restarts)
    pub fn set_external_id(&self, id: &str, ext: &str) {
        let Ok(rt) = self.get(id) else { return };
        let info = {
            let mut i = rt.info.lock().unwrap();
            i.external_id = Some(ext.to_string());
            i.clone()
        };
        let _ = self.store.save_session(&info);
        self.broadcast_updated(&info);
    }

    /// Before restarting Codex / Cursor / Kiro in terminal mode: if no tool session id was recorded, look in the tool's history sessions
    /// for the newest one in the same directory, used after this session was created and not claimed by another session; record it and continue it on restart
    pub async fn guess_terminal_session(&self, id: &str) {
        let Ok(rt) = self.get(id) else { return };
        let info = rt.info.lock().unwrap().clone();
        if info.mode != SessionMode::Terminal
            || info.external_id.is_some()
            || !matches!(info.tool, ToolKind::Codex | ToolKind::Cursor | ToolKind::Kiro)
        {
            return;
        }
        let settings = self.settings.read().unwrap().clone();
        let list = crate::history::list(Some(info.tool), &settings).await;
        let taken: HashSet<String> = self
            .sessions
            .lock()
            .unwrap()
            .values()
            .filter_map(|r| r.info.lock().unwrap().external_id.clone())
            .collect();
        let norm = |p: &str| p.replace('\\', "/").trim_end_matches('/').to_lowercase();
        let cwd = norm(&info.cwd);
        let hit = list
            .into_iter()
            .filter(|h| norm(&h.cwd) == cwd && h.updated_at >= info.created_at - 10_000 && !taken.contains(&h.id))
            .max_by_key(|h| h.updated_at);
        if let Some(h) = hit {
            crate::info!("终端会话 {id} 接着 {} 会话 {}", info.tool.as_str(), h.id);
            self.set_external_id(id, &h.id);
        }
    }

    pub fn rename(&self, id: &str, title: &str) -> Result<()> {
        let rt = self.get(id)?;
        let info = {
            let mut i = rt.info.lock().unwrap();
            i.title = title.to_string();
            i.clone()
        };
        self.store.save_session(&info)?;
        self.broadcast_updated(&info);
        Ok(())
    }

    pub fn backlog(&self, id: &str, since: i64) -> Result<Vec<Event>> {
        self.get(id)?;
        self.store.events_since(id, since, 20_000)
    }

    /// The latest limit events (before empty) or the limit events before before, so long sessions don't have to be sent all at once on open
    pub fn backlog_tail(&self, id: &str, before: Option<i64>, limit: usize) -> Result<(Vec<Event>, bool)> {
        self.get(id)?;
        self.store.events_tail(id, before, limit.clamp(1, 20_000))
    }

    pub fn term_snapshot(&self, id: &str) -> Result<Vec<u8>> {
        let rt = self.get(id)?;
        let t = rt.term.lock().unwrap();
        let (a, b) = t.as_slices();
        Ok([a, b].concat())
    }

    pub fn publish(&self, session_id: Option<String>, method: &'static str, params: Value) {
        let _ = self.bus.send(Arc::new(Outbound {
            session_id,
            method,
            params,
        }));
    }

    fn broadcast_updated(&self, info: &SessionInfo) {
        self.publish(
            None,
            methods::EV_SESSION_UPDATED,
            serde_json::to_value(info).unwrap_or(Value::Null),
        );
    }

    fn alive(&self, sid: &str, gen: u64) -> Option<Arc<SessionRt>> {
        let rt = self.get(sid).ok()?;
        let cur = *rt.generation.lock().unwrap();
        (cur == gen || gen == 0).then_some(rt)
    }

    /// Number, update session state, persist, broadcast. gen=0 means emitted by the hub itself (generation not checked)
    pub fn emit(&self, sid: &str, gen: u64, mut body: EventBody) {
        let Some(rt) = self.alive(sid, gen) else {
            return;
        };
        if let EventBody::TurnEnd { model, .. } = &mut body {
            if model.is_none() {
                *model = rt.info.lock().unwrap().model.clone().filter(|m| !m.is_empty());
            }
        }
        let ts = now_ms();
        if body.is_ephemeral() {
            self.publish(
                Some(sid.to_string()),
                methods::EV_SESSION_EVENT,
                json!({ "sessionId": sid, "event": Event { seq: 0, ts, body } }),
            );
            return;
        }

        let mut notify: Option<(String, String)> = None;
        let (ev, info, changed) = {
            let mut info = rt.info.lock().unwrap();
            let mut changed = false;
            match &body {
                EventBody::Status { status, .. } => {
                    if info.status != *status {
                        info.status = *status;
                        changed = true;
                    }
                }
                EventBody::PermissionRequest {
                    request_id, title, questions, ..
                } => {
                    let mut p = rt.pending.lock().unwrap();
                    p.insert(request_id.clone());
                    info.pending_permissions = p.len() as u32;
                    info.status = SessionStatus::WaitingPermission;
                    changed = true;
                    notify = Some(if questions.is_empty() {
                        (format!("{} 等待审批", display_name(&info)), title.clone())
                    } else {
                        (format!("{} 在等你回答", display_name(&info)), questions[0].question.clone())
                    });
                }
                EventBody::PermissionResolved { request_id, .. } => {
                    let mut p = rt.pending.lock().unwrap();
                    p.remove(request_id);
                    info.pending_permissions = p.len() as u32;
                    if p.is_empty() && info.status == SessionStatus::WaitingPermission {
                        info.status = SessionStatus::Running;
                    }
                    changed = true;
                }
                EventBody::Usage { usage } => {
                    info.usage.add(usage);
                    let _ = self
                        .store
                        .log_usage(sid, info.tool.as_str(), ts, usage);
                    changed = true;
                }
                EventBody::UserMessage { text, .. } => {
                    if info.title.trim().is_empty() {
                        info.title = truncate(text.lines().next().unwrap_or(""), 80);
                    }
                    if info.status == SessionStatus::Idle {
                        info.status = SessionStatus::Running;
                    }
                    changed = true;
                }
                EventBody::AssistantText { text, .. } => {
                    if !text.trim().is_empty() {
                        *rt.last_text.lock().unwrap() = text.clone();
                    }
                }
                EventBody::TurnEnd { stop_reason, .. } => {
                    if !matches!(info.status, SessionStatus::Exited | SessionStatus::Error) {
                        info.status = SessionStatus::Idle;
                    }
                    changed = true;
                    let what = match stop_reason.as_deref() {
                        Some("error" | "failed") => "出错了",
                        Some("interrupted" | "cancelled") => "已中断",
                        _ => "回复完成",
                    };
                    let title = if info.title.trim().is_empty() {
                        format!("{} {what}", display_name(&info))
                    } else {
                        format!("{} {what}：{}", display_name(&info), snippet(&info.title, 60))
                    };
                    let reply = std::mem::take(&mut *rt.last_text.lock().unwrap());
                    notify = Some((title, snippet(&reply, 240)));
                }
                _ => {}
            }
            info.last_seq += 1;
            info.updated_at = ts;
            let ev = Event {
                seq: info.last_seq,
                ts,
                body,
            };
            (ev, info.clone(), changed)
        };

        if let Err(e) = self.store.append_event(sid, &ev) {
            crate::warn!("落库失败 {sid}: {e}");
        }
        if changed {
            let _ = self.store.save_session(&info);
        }
        let kind_turn_end = matches!(ev.body, EventBody::TurnEnd { .. });
        self.publish(
            Some(sid.to_string()),
            methods::EV_SESSION_EVENT,
            json!({ "sessionId": sid, "event": ev }),
        );
        if changed {
            self.broadcast_updated(&info);
        }
        // In-app notifications: the client decides whether to show them based on its own preferences
        if let Some((title, body)) = notify {
            self.publish(
                None,
                methods::EV_NOTIFY,
                json!({ "sessionId": sid, "title": title, "body": body,
                        "kind": if kind_turn_end { "turn_end" } else { "permission" } }),
            );
        }
    }

    fn set_info(&self, sid: &str, gen: u64, f: impl FnOnce(&mut SessionInfo) -> bool) {
        let Some(rt) = self.alive(sid, gen) else {
            return;
        };
        let info = {
            let mut i = rt.info.lock().unwrap();
            if !f(&mut i) {
                return;
            }
            i.clone()
        };
        let _ = self.store.save_session(&info);
        self.broadcast_updated(&info);
    }

    fn term_output(&self, sid: &str, gen: u64, data: &[u8]) {
        let Some(rt) = self.alive(sid, gen) else {
            return;
        };
        {
            let mut t = rt.term.lock().unwrap();
            t.extend(data);
            if t.len() > TERM_BUF_MAX {
                let drop = t.len() - TERM_BUF_MAX;
                t.drain(..drop);
            }
        }
        use base64::Engine;
        self.publish(
            Some(sid.to_string()),
            methods::EV_PTY_OUTPUT,
            json!({ "sessionId": sid,
                    "data": base64::engine::general_purpose::STANDARD.encode(data) }),
        );
    }

    /// Adapter exited: clear pending approvals and mark exited
    fn exited(&self, sid: &str, gen: u64, message: Option<String>) {
        let Some(rt) = self.alive(sid, gen) else {
            return;
        };
        let pending: Vec<String> = rt.pending.lock().unwrap().drain().collect();
        for request_id in pending {
            self.emit(
                sid,
                gen,
                EventBody::PermissionResolved {
                    request_id,
                    option_id: "cancelled".into(),
                    answers: vec![],
                },
            );
        }
        // The process is gone, and so are its background tasks
        self.set_info(sid, gen, |i| !std::mem::take(&mut i.background_tasks).is_empty());
        let failed = message.is_some();
        if let Some(m) = &message {
            self.emit(sid, gen, EventBody::Error { message: m.clone() });
        }
        self.emit(
            sid,
            gen,
            EventBody::Status {
                status: if failed {
                    SessionStatus::Error
                } else {
                    SessionStatus::Exited
                },
                message,
            },
        );
        // A failure still allows sending another message to trigger a restart, so after Error everything falls back to Exited semantics, handled by send
        *rt.cmd.lock().unwrap() = None;
    }
}

fn count_users(evs: &[EventBody]) -> usize {
    evs.iter().filter(|e| matches!(e, EventBody::UserMessage { .. })).count()
}

/// Skip the first skip user messages (with their replies) and return the events after them
fn after_users(evs: Vec<EventBody>, skip: usize) -> Vec<EventBody> {
    let mut seen = 0;
    evs.into_iter()
        .filter(|e| {
            if matches!(e, EventBody::UserMessage { .. }) {
                seen += 1;
            }
            seen > skip
        })
        .collect()
}

/// Take ctx characters on each side of the match and flatten into one line
fn around(text: &str, q: &str, ctx: usize) -> String {
    let one: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let lower = one.to_lowercase();
    let chars: Vec<char> = one.chars().collect();
    // to_lowercase may change byte lengths, so convert by character position
    let pos = lower
        .find(&q.to_lowercase())
        .map(|b| lower[..b].chars().count())
        .unwrap_or(0);
    let start = pos.saturating_sub(ctx);
    let end = (pos + q.chars().count() + ctx).min(chars.len());
    let mut s: String = chars[start..end].iter().collect();
    if start > 0 {
        s.insert(0, '…');
    }
    if end < chars.len() {
        s.push('…');
    }
    s
}

/// Notification body: strip Markdown markup, flatten into one line, truncate to max characters
fn snippet(text: &str, max: usize) -> String {
    let one: String = text
        .lines()
        .map(|l| l.trim().trim_start_matches(['#', '>', '-', '*']).trim())
        .filter(|l| !l.is_empty() && !l.starts_with("```"))
        .collect::<Vec<_>>()
        .join(" ")
        .replace("**", "")
        .replace('`', "");
    truncate(&one, max)
}

fn display_name(info: &SessionInfo) -> String {
    match info.tool {
        ToolKind::Claude => "Claude Code",
        ToolKind::Codex => "Codex",
        ToolKind::Cursor => "Cursor",
        ToolKind::Kiro => "Kiro",
        ToolKind::Qoder => "Qoder",
        ToolKind::Codebuddy => "CodeBuddy",
        ToolKind::Shell => "终端",
    }
    .to_string()
}

fn new_rt(info: SessionInfo) -> SessionRt {
    SessionRt {
        info: Mutex::new(info),
        cmd: Mutex::new(None),
        term: Mutex::new(VecDeque::new()),
        pending: Mutex::new(HashSet::new()),
        generation: Mutex::new(0),
        models: Mutex::new(vec![]),
        last_text: Mutex::new(String::new()),
        commands: Mutex::new(vec![]),
        queue: Mutex::new(VecDeque::new()),
        interrupted: Mutex::new(false),
        launched_mode: Mutex::new(String::new()),
        restart_pending: Mutex::new(false),
    }
}

/// Handle adapters use to report events to the hub
#[derive(Clone)]
pub struct Emitter {
    hub: Arc<Hub>,
    sid: String,
    gen: u64,
}

impl Emitter {
    pub fn event(&self, body: EventBody) {
        let end = match &body {
            EventBody::TurnEnd { stop_reason, .. } => Some(stop_reason.clone()),
            _ => None,
        };
        self.hub.emit(&self.sid, self.gen, body);
        if let Some(r) = end {
            self.hub.after_turn(&self.sid, self.gen, r.as_deref());
        }
    }

    /// Slash commands reported by the tool
    pub fn commands(&self, list: Vec<SlashCommand>) {
        let Some(rt) = self.hub.alive(&self.sid, self.gen) else {
            return;
        };
        let tool = rt.info.lock().unwrap().tool;
        if !list.is_empty() {
            self.hub
                .tool_commands
                .lock()
                .unwrap()
                .insert(tool.as_str().to_string(), list.clone());
        }
        *rt.commands.lock().unwrap() = list;
    }

    /// The tool changed its permission mode itself (e.g. Claude leaving plan after planning).
    /// Unrecognized modes are recorded as is and the UI shows the raw name, so it never disagrees with the tool's actual state
    pub fn permission_mode(&self, mode: &str) {
        let m = mode.to_string();
        if m.is_empty() {
            return;
        }
        self.hub.set_info(&self.sid, self.gen, |i| {
            if i.permission_mode.as_deref().unwrap_or("default") == m {
                return false;
            }
            i.permission_mode = Some(m);
            true
        });
    }

    pub fn status(&self, status: SessionStatus, message: Option<String>) {
        self.event(EventBody::Status { status, message });
    }

    pub fn external_id(&self, id: &str) {
        let id = id.to_string();
        self.hub.set_info(&self.sid, self.gen, |i| {
            if i.external_id.as_deref() == Some(id.as_str()) {
                return false;
            }
            i.external_id = Some(id);
            true
        });
    }

    pub fn model(&self, model: &str) {
        let m = model.to_string();
        self.hub.set_info(&self.sid, self.gen, |i| {
            if i.model.as_deref() == Some(m.as_str()) {
                return false;
            }
            i.model = Some(m);
            true
        });
    }

    /// Models the tool reported
    pub fn models(&self, models: Vec<ModelOption>) {
        let Some(rt) = self.hub.alive(&self.sid, self.gen) else {
            return;
        };
        let tool = rt.info.lock().unwrap().tool;
        self.hub.remember_models(tool, &models);
        *rt.models.lock().unwrap() = models;
    }

    /// Context usage percentage
    pub fn context(&self, percent: f64) {
        let p = (percent.clamp(0.0, 100.0) * 10.0).round() / 10.0;
        self.hub.set_info(&self.sid, self.gen, |i| {
            if i.context_percent == Some(p) {
                return false;
            }
            i.context_percent = Some(p);
            true
        });
    }

    pub fn background_tasks(&self, tasks: Vec<BackgroundTask>) {
        self.hub.set_info(&self.sid, self.gen, |i| {
            if i.background_tasks == tasks {
                return false;
            }
            i.background_tasks = tasks;
            true
        });
    }

    pub fn title_if_empty(&self, title: &str) {
        let t = truncate(title, 80);
        self.hub.set_info(&self.sid, self.gen, |i| {
            if !i.title.trim().is_empty() || t.trim().is_empty() {
                return false;
            }
            i.title = t;
            true
        });
    }

    pub fn pty_output(&self, data: &[u8]) {
        self.hub.term_output(&self.sid, self.gen, data);
    }

    pub fn exited(&self, message: Option<String>) {
        self.hub.exited(&self.sid, self.gen, message);
    }

    pub fn session_id(&self) -> &str {
        &self.sid
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(t: &str) -> EventBody {
        EventBody::UserMessage { text: t.into(), images: vec![] }
    }
    fn reply(t: &str) -> EventBody {
        EventBody::AssistantText { id: t.into(), text: t.into() }
    }

    #[test]
    fn after_users_skips_synced_turns() {
        let evs = vec![user("1"), reply("a"), user("2"), reply("b"), user("3"), reply("c")];
        assert_eq!(count_users(&evs), 3);
        assert_eq!(after_users(evs.clone(), 2), vec![user("3"), reply("c")]);
        assert_eq!(after_users(evs.clone(), 0), evs);
        assert!(after_users(evs, 3).is_empty());
    }

    #[test]
    fn session_id_path_guard() {
        assert!(safe_sid(&uuid::Uuid::new_v4().to_string()));
        for bad in ["", "..", "../../victim", "/home/me", "a/b", r"C:\x", "abc def"] {
            assert!(!safe_sid(bad), "{bad}");
        }
    }

    /// Insert a session with a "running process" into the hub; returns a receiver showing which commands it gets
    fn live(hub: &Hub, id: &str, mode: SessionMode, status: SessionStatus, updated_at: i64) -> mpsc::UnboundedReceiver<AdapterCmd> {
        let info = SessionInfo {
            id: id.into(),
            tool: ToolKind::Kiro,
            mode,
            cwd: "/tmp".into(),
            title: String::new(),
            status,
            external_id: Some(format!("ext-{id}")),
            created_at: 0,
            updated_at,
            last_seq: 0,
            pending_permissions: 0,
            usage: Default::default(),
            model: None,
            command: None,
            context_percent: None,
            permission_mode: None,
            effort: None,
            pinned: false,
            queued: vec![],
            background_tasks: vec![],
            parent_id: None,
            synced_users: None,
        };
        let rt = new_rt(info);
        let (tx, rx) = mpsc::unbounded_channel();
        *rt.cmd.lock().unwrap() = Some(tx);
        hub.sessions.lock().unwrap().insert(id.into(), Arc::new(rt));
        rx
    }

    fn killed(rx: &mut mpsc::UnboundedReceiver<AdapterCmd>) -> bool {
        matches!(rx.try_recv(), Ok(AdapterCmd::Kill))
    }

    #[test]
    fn make_room_stops_least_recently_used_idle() {
        use SessionMode::*;
        use SessionStatus::*;
        let settings = DaemonSettings { max_live_processes: Some(3), ..Default::default() };
        let hub = Hub::new(crate::store::Store::open_memory().unwrap(), settings);
        let mut old = live(&hub, "old", Structured, Idle, 1);
        let mut newer = live(&hub, "newer", Structured, Idle, 5);
        let mut busy = live(&hub, "busy", Structured, Running, 0);
        let mut asking = live(&hub, "asking", Structured, WaitingPermission, 0);
        let mut term = live(&hub, "term", Terminal, Idle, 0);
        // 4 open in chat mode, and opening 1 more must bring it down to 2: both idle ones must stop; busy, awaiting-approval and terminal ones are left alone
        hub.make_room("new");
        assert!(killed(&mut old) && killed(&mut newer));
        assert!(!killed(&mut busy) && !killed(&mut asking) && !killed(&mut term));

        // When only 1 over, stop only the least recently used one
        let hub = Hub::new(
            crate::store::Store::open_memory().unwrap(),
            DaemonSettings { max_live_processes: Some(3), ..Default::default() },
        );
        let mut a = live(&hub, "a", Structured, Idle, 1);
        let mut b = live(&hub, "b", Structured, Idle, 2);
        let mut c = live(&hub, "c", Structured, Idle, 3);
        hub.make_room("new");
        assert!(killed(&mut a) && !killed(&mut b) && !killed(&mut c));
        // Restarting the process of an already open session doesn't count as a new one
        let mut c2 = live(&hub, "c", Structured, Idle, 3);
        hub.make_room("c");
        assert!(!killed(&mut c2));

        // 0 means unlimited
        let hub = Hub::new(
            crate::store::Store::open_memory().unwrap(),
            DaemonSettings { max_live_processes: Some(0), ..Default::default() },
        );
        let mut rxs: Vec<_> = (0..10).map(|i| live(&hub, &format!("s{i}"), Structured, Idle, i)).collect();
        hub.make_room("new");
        assert!(rxs.iter_mut().all(|r| !killed(r)));
    }

    #[test]
    fn release_idle_covers_waiting_and_error() {
        use SessionMode::*;
        use SessionStatus::*;
        let hub = Hub::new(crate::store::Store::open_memory().unwrap(), Default::default());
        let long_ago = crate::util::now_ms() - 31 * 60_000;
        let mut idle = live(&hub, "idle", Structured, Idle, long_ago);
        let mut asking = live(&hub, "asking", Structured, WaitingPermission, long_ago);
        let mut err = live(&hub, "err", Structured, Error, long_ago);
        let mut busy = live(&hub, "busy", Structured, Running, long_ago);
        let mut term = live(&hub, "term", Terminal, Idle, long_ago);
        let mut recent = live(&hub, "recent", Structured, Idle, crate::util::now_ms());
        let mut queued = live(&hub, "queued", Structured, Idle, long_ago);
        hub.get("queued").unwrap().queue.lock().unwrap().push_back(Queued {
            id: "q".into(),
            text: "x".into(),
            images: vec![],
            now: false,
        });
        hub.release_idle();
        assert!(killed(&mut idle) && killed(&mut asking) && killed(&mut err));
        assert!(!killed(&mut busy) && !killed(&mut term) && !killed(&mut recent) && !killed(&mut queued));
    }

    #[test]
    fn delete_unknown_session_is_rejected() {
        let hub = Hub::new(crate::store::Store::open_memory().unwrap(), Default::default());
        assert!(hub.delete("../../victim").is_err());
        assert!(hub.delete("").is_err());
    }
}

#[cfg(test)]
mod attach_tests {
    #[test]
    fn attach_names() {
        let home = std::env::temp_dir().join(format!("ah-attach-{}", std::process::id()));
        std::env::set_var("YI_CONSOLE_HOME", &home);
        let a = super::attach_file("s1", "../my notes.txt", "aGk=").unwrap();
        let b = super::attach_file("s1", "my notes.txt", "aGk=").unwrap();
        let (a, b) = (a["path"].as_str().unwrap().to_string(), b["path"].as_str().unwrap().to_string());
        assert!(a.ends_with("my_notes.txt") && a.contains("files"), "{a}");
        assert!(b.ends_with("my_notes (1).txt"), "{b}");
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "hi");
        // Chunked upload: later chunks append to the end; files outside the directory or nonexistent files are refused
        super::attach_append("s1", &a, "IHRoZXJl").unwrap();
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "hi there");
        let outside = home.join("x.txt");
        std::fs::write(&outside, "x").unwrap();
        assert!(super::attach_append("s1", &outside.display().to_string(), "aGk=").is_err());
        assert!(super::attach_append("s2", &a, "aGk=").is_err());
        let gone = a.replace("my_notes.txt", "gone.txt");
        assert!(super::attach_append("s1", &gone, "aGk=").is_err());
        let _ = std::fs::remove_dir_all(&home);
    }
}
