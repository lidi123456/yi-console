//! Hub：会话管理器。持有所有会话的运行时状态，负责事件编号、落库和广播。

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

/// 终端回放缓冲上限
const TERM_BUF_MAX: usize = 2 * 1024 * 1024;

/// 单张图片上限
pub const IMAGE_MAX: usize = 5 * 1024 * 1024;

/// 客户端发来的图片（base64）
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

/// 会话 id 都是我们生成的 UUID：只含十六进制数字和 `-`。拼进路径前用它挡住 `..`、绝对路径和空串
fn safe_sid(sid: &str) -> bool {
    !sid.is_empty() && sid.len() <= 64 && sid.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

/// 输入框里粘贴 / 拖进来的普通文件：存到 `uploads/<会话>/files/`，重名就加 (1) (2)，删会话时一起删
pub fn attach_file(sid: &str, name: &str, data: &str) -> Result<serde_json::Value> {
    let clean: String = std::path::Path::new(name)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default()
        .chars()
        // 空格也换掉：@引用里的路径带空格要加引号，有的工具认不出
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

/// 分块上传的后续块：接到 `attach_file` 刚建的文件末尾。只认这个会话上传目录里已有的文件，总大小不超过 50MB
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

/// 发往客户端连接的消息。`session_id` 有值时只发给订阅了该会话的连接。
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
    /// 每次启动 adapter 加一；旧 adapter 的迟到事件按代号丢弃
    generation: Mutex<u64>,
    /// 工具这次启动时报告的可选模型
    models: Mutex<Vec<ModelOption>>,
    /// 这一轮最后一段回复，回合结束的通知里带上开头一段
    last_text: Mutex<String>,
    /// 工具报告的斜杠命令
    commands: Mutex<Vec<SlashCommand>>,
    /// 排队等当前回合结束再发的消息
    queue: Mutex<VecDeque<Queued>>,
    /// 用户点了中断，这一轮结束后不自动发排队的消息
    interrupted: Mutex<bool>,
    /// 当前进程启动时的审批模式
    launched_mode: Mutex<String>,
    /// 审批模式改了但要重启进程才能生效（Claude 切到全部放行），等这一轮结束再重启
    restart_pending: Mutex<bool>,
}

struct Queued {
    id: String,
    text: String,
    images: Vec<IncomingImage>,
    /// 用户点了"立即发送"
    now: bool,
}

/// 各工具实际支持的审批模式（前端 `permModes` 手动同步）。
/// auto 是工具自己的自动审批：Claude 的 `--permission-mode auto`，Codex 的 auto_review
pub fn permission_modes(tool: ToolKind, mode: SessionMode) -> &'static [&'static str] {
    use SessionMode::*;
    use ToolKind::*;
    match (tool, mode) {
        (Claude, _) | (Codex, Structured) => &["default", "acceptEdits", "auto", "plan", "bypass"],
        (Codex, Terminal) => &["default", "auto", "plan", "bypass"],
        (Cursor | Kiro, Structured) => &["default", "acceptEdits", "plan", "bypass"],
        (Cursor, Terminal) => &["default", "plan", "bypass"],
        (Kiro, Terminal) => &["default", "bypass"],
        // Qoder 没有规划模式
        (Qoder, _) => &["default", "acceptEdits", "bypass"],
        (Codebuddy, _) => &["default", "acceptEdits", "plan", "bypass"],
        (Shell, _) => &[],
    }
}

/// 终端会话等页面报上实际尺寸再启动（最多等 3 秒）：按默认尺寸启动后马上改尺寸，
/// cursor-agent、Claude Code 这类全屏程序重画时会按旧宽度擦屏，留下一堆残影
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
    /// 各工具最近一次报告的模型列表（落盘，新建会话时用）
    tool_models: Mutex<HashMap<String, Vec<ModelOption>>>,
    /// 各工具最近一次报告的斜杠命令（只在内存里）
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
    /// 接着某个已有的工具会话（历史导入）
    #[serde(default)]
    pub resume_id: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    /// default / acceptEdits / bypass / plan
    #[serde(default)]
    pub permission_mode: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    /// shell 工具或终端模式下自定义命令行
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub cols: Option<u16>,
    #[serde(default)]
    pub rows: Option<u16>,
    /// 底部终端面板：所属对话会话
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

    /// 守护进程重启后，之前的工具进程都已不在，统一标记为 exited
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

        // 接续历史会话：能离线读取的（claude/codex）先把历史导入事件日志，
        // 读不了的（ACP 工具）让工具在 session/load 时回放
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
        // 终端里的 Claude：第一次启动就指定会话 id，之后重启接着这个会话。
        // 开了没说话就退出时 Claude 不会留下记录，这时还用同一个 id 新开
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

    /// 发送用户消息；进程已退出时自动 resume。
    /// `queue` 为 true 且这一轮还没结束时：Claude、Codex 直接交给工具，工具在下一次工具调用后读到
    /// （没赶上的由工具在回合结束后接着处理）；其他工具先排队，回合结束后按顺序发出
    pub fn send(self: &Arc<Self>, id: &str, text: &str, images: Vec<IncomingImage>, queue: bool) -> Result<()> {
        let rt = self.get(id)?;
        let info = rt.info.lock().unwrap().clone();
        if info.mode == SessionMode::Terminal && !images.is_empty() {
            bail!("终端模式不支持发送图片");
        }
        let busy = matches!(info.status, SessionStatus::Running | SessionStatus::WaitingPermission);
        // 回合结束要重启进程（换审批模式）时，工具没读到的消息会随进程丢掉，这时也排队
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
        // 先校验、落盘图片，失败时不拉起进程
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

    /// 读用户消息里的图片
    pub fn image(&self, sid: &str, image_id: &str) -> Result<Value> {
        use base64::Engine;
        self.get(sid)?;
        // id 由我们生成（hex + 扩展名），拒绝其他字符防止路径穿越
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

    /// 切换模型：记到会话上（重启后也生效），进程在跑就通知工具
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

    /// 切换思考等级：记到会话上，进程在跑就通知工具（Claude 立即生效，Codex 下一回合）
    /// 压缩上下文：各工具都认 /compact（Codex 由适配器转成 thread/compact/start），当普通消息发
    pub fn compact(self: &Arc<Self>, id: &str) -> Result<()> {
        let info = self.get(id)?.info.lock().unwrap().clone();
        if info.mode == SessionMode::Terminal || info.tool == ToolKind::Shell {
            bail!("终端模式请直接在终端里输入 /compact");
        }
        self.send(id, "/compact", vec![], true)
    }

    /// /clear：结束工具进程、删掉聊天记录、用量清零，忘掉工具会话 id，下次发消息时新开一个工具会话。
    /// Yi Console 会话（id、标题、目录、模型、审批模式、底部终端）不变；回复进行中会直接中断
    pub async fn clear(self: &Arc<Self>, id: &str) -> Result<()> {
        let rt = self.get(id)?;
        {
            let i = rt.info.lock().unwrap();
            if i.mode == SessionMode::Terminal || i.tool == ToolKind::Shell {
                bail!("终端模式请直接在终端里输入 /clear");
            }
        }
        // 先作废代号，旧进程迟到的事件、退出状态都不再记进会话
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
        // 对话里的图片、附件只被删掉的记录引用，一起删
        if safe_sid(id) {
            let _ = std::fs::remove_dir_all(uploads_dir(id));
        }
        self.broadcast_updated(&info);
        // seq 接着往后编，客户端按这条丢掉之前的事件；重连补拉时也会收到它
        self.emit(id, 0, EventBody::Cleared {});
        Ok(())
    }

    /// 停止一个后台任务（目前只有 Claude 支持）
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

    /// 会话可选模型：这次启动时工具报告的，没有就用该工具上次的
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

    /// 重新拉起已退出的会话（不发消息），用于查看 ACP 工具的历史或重开终端
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

    /// 从工具历史接过来的 ACP 会话（Cursor、Kiro…）第一次 session/load 没成功（比如会话被另一个 Kiro 占着），
    /// 历史没回放进来，事件里却已经有了出错记录，last_seq 不是 0：这时还得让工具回放
    fn history_missing(&self, info: &SessionInfo) -> bool {
        info.mode == SessionMode::Structured
            && !matches!(info.tool, ToolKind::Claude | ToolKind::Codex | ToolKind::Shell)
            && info.external_id.is_some()
            && self.store.count_user_messages(&info.id).is_ok_and(|n| n == 0)
    }

    /// 在对话模式和终端模式之间切换，接着同一个工具会话。
    /// 切到终端：记下工具会话里已有几条用户消息，用工具的交互界面 resume；
    /// 切回对话：只把终端里新聊的导入对话（Claude / Codex 读工具日志，Cursor / Kiro 让工具回放时跳过前面的）
    pub async fn set_mode(self: &Arc<Self>, id: &str, mode: SessionMode) -> Result<()> {
        let rt = self.get(id)?;
        let info = rt.info.lock().unwrap().clone();
        if info.tool == ToolKind::Shell || info.parent_id.is_some() {
            bail!("普通终端没有对话模式");
        }
        if info.mode == mode {
            return Ok(());
        }
        // 终端里的 Codex / Cursor / Kiro 还没记下工具会话 id 的，先从工具历史里找
        self.guess_terminal_session(id).await;
        // 结束当前进程。先作废代号，旧进程退出时的状态、退出码不再记进会话
        let old = rt.cmd.lock().unwrap().take();
        *rt.generation.lock().unwrap() += 1;
        if let Some(tx) = &old {
            let _ = tx.send(AdapterCmd::Kill);
        }
        if info.mode == SessionMode::Terminal {
            // 用户主动切走，tmux 里的程序一起结束，等它退出了再接续
            if !crate::tmux::kill_and_wait(&crate::tmux::session_name(id)).await {
                // 没用 tmux：适配器收到 Kill 直接杀进程，留一点收尾时间
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
        } else if let Some(tx) = old {
            // 等对话模式的工具进程退出，免得两个进程同时写同一个工具会话
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
            // 没说过话的会话工具那边可能还没有记录，resume 会失败；Claude 由 spec_for 用 --session-id 处理
            if users == 0 && !offline {
                info.external_id = None;
            }
        } else {
            skip = info.synced_users.take().unwrap_or(0) as usize;
            if crate::history::pins_session_id(tool)
                && info.external_id.as_deref().is_some_and(|e| !crate::history::tool_session_exists(tool, e))
            {
                // 终端里开了 Claude 但没说话，没有可接的会话
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
        // 置成已退出，launch 报 Starting 时一定会广播新的会话信息
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
        // Cursor / Kiro 读不了离线历史，让工具 session/load 回放，跳过对话里已经有的
        let replay = mode == SessionMode::Structured && !offline && info.external_id.is_some();
        let mut spec = self.spec_for(&info, replay, 120, 32)?;
        spec.replay_skip = skip;
        // 终端模式由 start_when_sized 等前端报上尺寸再起进程
        self.launch(&rt, spec)
    }

    /// 把排队列表同步到会话信息上，客户端据此显示
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

    /// 排队消息：remove 撤回（返回内容，客户端放回输入框）；sendNow 中断当前回合并马上发它
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

    /// 回合结束：需要的话先重启进程（换审批模式），再发排队的下一条
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

    /// 结束当前进程，按会话现在的设置重新拉起（接着同一个工具会话）
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

    /// 切换审批模式：记到会话上（恢复时沿用），进程在跑就让工具马上换
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
        // Claude 只有启动时带了跳过审批的参数，才能切到全部放行，所以要重启进程
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

    /// 全局搜索：消息正文里包含 q 的，带上会话标题和命中处前后一段
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

    /// 斜杠命令：这次启动时工具报告的，没有就用该工具上次的
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

    /// 守护进程重启后，tmux 里还活着的终端会话直接接上
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

    /// 对话模式下正开着工具进程的会话（终端模式、shell 不算：里面可能有用户自己开的程序）
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

    /// 能不能结束这个进程而不丢东西：没在回复、没有排队的消息、没有后台任务。
    /// 闲着的要有工具会话 id（下次才接得回去）；出错的结束了也没损失；
    /// `waiting` 时等审批的也算（审批作废，下次发消息重新来）
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

    /// 结束闲置太久的工具进程（每个约占 100~250MB 内存），会话记录和工具会话 id 都在，
    /// 下次发消息时 send 会自动接着原来的会话拉起。等审批太久的、出错后没退出的也一并结束
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

    /// 要给 `id` 拉起对话模式的工具进程前，保证同时开着的不超过上限：
    /// 超出时按最久没动静的顺序结束空闲进程。都在忙时只能先超出，等它们闲下来再回收
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
        // id 来自客户端，后面要拼进路径做 remove_dir_all：只处理确实存在的会话
        self.get(id)?;
        // 先删底部终端面板里属于它的终端
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
        // 进程没在跑（比如守护进程刚重启还没接上）时，tmux 里的会话也要清掉
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

    /// 记下工具自己的会话 id（终端模式重启时据此恢复）
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

    /// 终端模式的 Codex / Cursor / Kiro 重启前：没记下工具会话 id 的，从工具的历史会话里找
    /// 同一目录、在这个会话创建之后用过、还没被别的会话占用的最新一个，找到就记下来，重启时接着它
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

    /// 最近的 limit 条（before 为空）或 before 之前的 limit 条，打开长会话时不用一次传完
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

    /// 编号、更新会话状态、落库、广播。gen=0 表示由 hub 自己发出（不校验代号）
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
        // 应用内通知：由客户端按自己的偏好决定弹不弹
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

    /// adapter 退出：清掉挂起的审批，标记 exited
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
        // 进程没了，后台任务也跟着没了
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
        // 失败也允许再次发送消息触发重启，所以 Error 之后统一落到 Exited 语义上由 send 处理
        *rt.cmd.lock().unwrap() = None;
    }
}

fn count_users(evs: &[EventBody]) -> usize {
    evs.iter().filter(|e| matches!(e, EventBody::UserMessage { .. })).count()
}

/// 跳过前 skip 条用户消息（连同它们的回复），返回之后的事件
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

/// 命中处前后各取 ctx 个字符，压成一行
fn around(text: &str, q: &str, ctx: usize) -> String {
    let one: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let lower = one.to_lowercase();
    let chars: Vec<char> = one.chars().collect();
    // to_lowercase 可能改变字节长度，按字符位置换算
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

/// 通知正文：去掉 Markdown 记号、压成一行，截到 max 个字符
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

/// adapter 往 hub 报告事件用的句柄
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

    /// 工具报告的斜杠命令
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

    /// 工具自己换了审批模式（比如 Claude 规划完退出 plan）。
    /// 认不出的模式也照实记下，界面显示原名，免得和工具实际状态对不上
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

    /// 工具报告的可选模型
    pub fn models(&self, models: Vec<ModelOption>) {
        let Some(rt) = self.hub.alive(&self.sid, self.gen) else {
            return;
        };
        let tool = rt.info.lock().unwrap().tool;
        self.hub.remember_models(tool, &models);
        *rt.models.lock().unwrap() = models;
    }

    /// 上下文占用百分比
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

    /// 往 hub 里塞一个"正开着进程"的会话，返回能看到它收到什么命令的接收端
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
        // 对话模式开着 4 个，再开 1 个要降到 2 个：两个闲着的都得停，忙的、等审批的、终端不动
        hub.make_room("new");
        assert!(killed(&mut old) && killed(&mut newer));
        assert!(!killed(&mut busy) && !killed(&mut asking) && !killed(&mut term));

        // 只超 1 个时只停最久没用的那个
        let hub = Hub::new(
            crate::store::Store::open_memory().unwrap(),
            DaemonSettings { max_live_processes: Some(3), ..Default::default() },
        );
        let mut a = live(&hub, "a", Structured, Idle, 1);
        let mut b = live(&hub, "b", Structured, Idle, 2);
        let mut c = live(&hub, "c", Structured, Idle, 3);
        hub.make_room("new");
        assert!(killed(&mut a) && !killed(&mut b) && !killed(&mut c));
        // 给已开着的会话重启进程，不算新增
        let mut c2 = live(&hub, "c", Structured, Idle, 3);
        hub.make_room("c");
        assert!(!killed(&mut c2));

        // 0 表示不限
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
        // 分块上传：后续块接在末尾；目录外的文件、不存在的文件都不行
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
