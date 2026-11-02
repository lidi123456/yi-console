//! Yi Console 客户端与 yi-agentd 之间的协议。
//!
//! 传输层是按行分隔的 JSON（每行一个 JSON-RPC 2.0 消息），本机走 unix socket / 命名管道，
//! 远程走 SSH exec `yi-agentd bridge` 的 stdio。

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROTOCOL_VERSION: u32 = 1;
pub const DAEMON_VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "+", env!("YI_CONSOLE_BUILD"));

/// 构建时间（unix 秒），客户端据此判断守护进程是不是比自己旧
pub fn build_time() -> u64 {
    env!("YI_CONSOLE_BUILD_TIME").parse().unwrap_or(0)
}

// ---------- JSON-RPC 外壳 ----------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    pub method: String,
    pub params: Value,
}

/// 线上一行可能是三种之一。顺序有讲究：Request 要求 id + method，
/// 必须排在只要求 id 的 Response 前面。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Message {
    Request(Request),
    Response(Response),
    Notification(Notification),
}

// ---------- 领域模型 ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ToolKind {
    Claude,
    Codex,
    Cursor,
    Kiro,
    /// 阿里 Qoder CLI（qodercli），走 ACP
    Qoder,
    /// 腾讯 CodeBuddy Code（codebuddy），走 ACP
    Codebuddy,
    /// 任意命令，只能走终端模式
    Shell,
}

impl ToolKind {
    pub const ALL: [ToolKind; 7] = [
        ToolKind::Claude,
        ToolKind::Codex,
        ToolKind::Cursor,
        ToolKind::Kiro,
        ToolKind::Qoder,
        ToolKind::Codebuddy,
        ToolKind::Shell,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            ToolKind::Claude => "claude",
            ToolKind::Codex => "codex",
            ToolKind::Cursor => "cursor",
            ToolKind::Kiro => "kiro",
            ToolKind::Qoder => "qoder",
            ToolKind::Codebuddy => "codebuddy",
            ToolKind::Shell => "shell",
        }
    }

    pub fn parse(s: &str) -> Option<ToolKind> {
        ToolKind::ALL.into_iter().find(|t| t.as_str() == s)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SessionMode {
    /// 结构化：解析工具的机器接口，渲染成统一对话 UI
    Structured,
    /// 终端：PTY 直通，xterm.js 渲染
    Terminal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SessionStatus {
    Starting,
    Idle,
    Running,
    WaitingPermission,
    /// 工具进程已退出；发送新消息会自动 resume
    Exited,
    Error,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub cache_write_tokens: u64,
    #[serde(default)]
    pub cost_usd: f64,
    /// Kiro 等按 credit 计费的工具
    #[serde(default)]
    pub credits: f64,
}

impl Usage {
    pub fn add(&mut self, o: &Usage) {
        self.input_tokens += o.input_tokens;
        self.output_tokens += o.output_tokens;
        self.cache_read_tokens += o.cache_read_tokens;
        self.cache_write_tokens += o.cache_write_tokens;
        self.cost_usd += o.cost_usd;
        self.credits += o.credits;
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    pub id: String,
    pub tool: ToolKind,
    pub mode: SessionMode,
    pub cwd: String,
    pub title: String,
    pub status: SessionStatus,
    /// 工具自己的会话 id（claude session_id / codex thread id / ACP sessionId），用于 resume
    pub external_id: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub last_seq: i64,
    pub pending_permissions: u32,
    pub usage: Usage,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub command: Option<String>,
    /// 上下文窗口已用百分比（0~100），工具上报了才有
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_percent: Option<f64>,
    /// 审批模式：default / acceptEdits / plan / bypass，恢复会话时沿用
    #[serde(default)]
    pub permission_mode: Option<String>,
    /// 思考等级，不填用工具默认
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// 侧边栏置顶
    #[serde(default)]
    pub pinned: bool,
    /// 等当前回合结束再发的消息（只在内存里，守护进程重启后清空）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub queued: Vec<QueuedMessage>,
    /// 工具在后台跑着的任务（只在内存里，进程退出就清空）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub background_tasks: Vec<BackgroundTask>,    /// 会话页底部终端面板里开的终端：所属对话会话的 id。侧边栏不单独列出，删对话时一起删
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// 从对话模式切到终端模式时，工具会话里已有的用户消息数。切回对话模式时只导入这之后
    /// （也就是在终端里聊）的部分；没有值表示会话一开始就是终端模式，切回时全部导入
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synced_users: Option<u32>,
}

/// 后台任务：Claude 的后台 shell、后台子代理等
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundTask {
    pub id: String,
    /// 工具给的类型，如 local_bash、local_agent
    pub kind: String,
    pub description: String,
    /// 本机发现它的时间（毫秒）
    pub started_at: i64,
    /// 输出写在主机上的这个文件里，可以用 fs.read 看
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_file: Option<String>,
}

/// 排队中的消息
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct QueuedMessage {
    pub id: String,
    pub text: String,
    #[serde(default)]
    pub images: u32,
}

/// 工具提供的斜杠命令
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SlashCommand {
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// 参数提示
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub hint: String,
}

/// 工具可选的模型
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ModelOption {
    /// 传给工具的值
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// 别名实际对应的模型（Claude 的 opus / sonnet 等），用来和会话当前模型对上
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub resolved: String,
    /// 支持的思考等级（low / medium / high / xhigh / max …），空表示不支持调
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub efforts: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub default_effort: String,
}

/// 用户消息里的图片。文件存在 yi-agentd 数据目录，内容用 session.image 取
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ImageRef {
    pub id: String,
    pub mime_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FileDiff {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_text: Option<String>,
    /// 工具直接给了 unified diff 时用这个
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unified: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PermissionOption {
    pub id: String,
    pub label: String,
    /// allow_once / allow_always / reject_once / reject_always
    pub kind: String,
}

/// AI 向用户提的问题（Claude 的 AskUserQuestion、Codex 的 requestUserInput、Cursor 的 ask_question、
/// Kiro 正文里的编号选项），走审批同一条路：PermissionRequest 带 questions，回答时 answers 带回去
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct Question {
    pub id: String,
    /// 短标题（Claude / Codex 的 header）
    #[serde(default)]
    pub header: String,
    pub question: String,
    #[serde(default)]
    pub options: Vec<QuestionOption>,
    #[serde(default)]
    pub multi_select: bool,
    /// 允许不选选项、自己填一段文字
    #[serde(default)]
    pub allow_other: bool,
    /// 输入内容不回显（密码之类）
    #[serde(default)]
    pub secret: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct QuestionOption {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct QuestionAnswer {
    pub question_id: String,
    /// 选中的选项 id
    #[serde(default)]
    pub selected: Vec<String>,
    /// 自己填的内容
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
}

/// 提问类请求固定的两个选项 id
pub const QUESTION_ANSWER: &str = "answer";
pub const QUESTION_SKIP: &str = "skip";

/// 统一事件。`seq` 由 yi-agentd 分配；`delta` 类事件不落库、没有 seq。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum EventBody {
    UserMessage {
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<ImageRef>,
    },
    AssistantText {
        id: String,
        text: String,
    },
    /// 流式片段，只推送不落库；最终内容以 AssistantText 为准
    AssistantDelta {
        id: String,
        delta: String,
    },
    Thinking {
        id: String,
        text: String,
    },
    /// 思考过程的流式片段，同 AssistantDelta 只推送不落库
    ThinkingDelta {
        id: String,
        delta: String,
    },
    ToolCall {
        id: String,
        name: String,
        title: String,
        #[serde(default)]
        input: Value,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        diffs: Vec<FileDiff>,
    },
    ToolResult {
        id: String,
        output: String,
        #[serde(default)]
        is_error: bool,
    },
    PermissionRequest {
        request_id: String,
        tool_name: String,
        title: String,
        #[serde(default)]
        input: Value,
        options: Vec<PermissionOption>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        questions: Vec<Question>,
    },
    PermissionResolved {
        request_id: String,
        option_id: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        answers: Vec<QuestionAnswer>,
    },
    Plan {
        entries: Vec<PlanEntry>,
    },
    Status {
        status: SessionStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    TurnEnd {
        #[serde(default)]
        stop_reason: Option<String>,
        /// 这一轮用的模型；适配器不填时由 hub 按会话当前模型补上
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
    },
    Usage {
        usage: Usage,
    },
    Error {
        message: String,
    },
    System {
        text: String,
    },
    /// 上下文压缩：phase = start / done / failed；trigger = auto / manual（工具没说就空）
    Compact {
        phase: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        trigger: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pre_tokens: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        post_tokens: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
    },
    /// /clear：这条之前的事件都已删除，客户端收到后丢掉手里 seq 更小的事件
    Cleared {},
}

impl EventBody {
    pub fn is_ephemeral(&self) -> bool {
        matches!(self, EventBody::AssistantDelta { .. } | EventBody::ThinkingDelta { .. })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PlanEntry {
    pub content: String,
    /// pending / in_progress / completed
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    /// 0 表示临时事件（不落库）
    pub seq: i64,
    pub ts: i64,
    #[serde(flatten)]
    pub body: EventBody,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolInfo {
    pub kind: ToolKind,
    pub available: bool,
    pub path: Option<String>,
    pub version: Option<String>,
    /// 是否支持结构化模式
    pub structured: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HelloResult {
    pub protocol: u32,
    pub daemon_version: String,
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub home: String,
    pub tools: Vec<ToolInfo>,
    /// 旧版守护进程没有这个字段，按 0（最旧）算
    #[serde(default)]
    pub build_time: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntry {
    pub tool: ToolKind,
    pub id: String,
    pub cwd: String,
    pub title: String,
    pub updated_at: i64,
    #[serde(default)]
    pub message_count: Option<u64>,
    /// 是否能离线读取历史内容（claude/codex 读文件；ACP 工具要 resume 才能看）
    pub readable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DirEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified: i64,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolOverride {
    /// 覆盖可执行文件路径
    #[serde(default)]
    pub command: String,
    /// 追加参数
    #[serde(default)]
    pub extra_args: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonSettings {
    #[serde(default)]
    pub tools: std::collections::BTreeMap<String, ToolOverride>,
    #[serde(default)]
    pub status_line: StatusLineSettings,
    /// 提示词片段：输入框里打 / 能选到
    #[serde(default)]
    pub snippets: Vec<Snippet>,
    /// 会话闲置多少分钟后结束工具进程省内存，下次发消息自动接着聊；
    /// 不填按 30 分钟，0 表示不释放
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_release_minutes: Option<u32>,
    /// 对话模式的工具进程最多同时开几个，超出时结束最久没用的空闲进程（下次发消息自动接上）；
    /// 不填按 5，0 表示不限。终端模式不算在内
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_live_processes: Option<u32>,
    /// 终端会话放在 tmux 里（守护进程重启后不中断）；不填按开启，没装 tmux 时不生效
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_tmux: Option<bool>,
}

pub const DEFAULT_IDLE_RELEASE_MINUTES: u32 = 30;
pub const DEFAULT_MAX_LIVE_PROCESSES: u32 = 5;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct Snippet {
    pub name: String,
    pub text: String,
}

/// 会话输入框下面的状态栏
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct StatusLineSettings {
    /// 内置字段，按顺序显示（model / context / tokens / cost / lastTurn / dir / branch / status / quota …）
    #[serde(default = "default_status_items")]
    pub items: Vec<String>,
    /// 自定义格式，例如 `{model} · {branch}{dirty}`；非空时代替 items
    #[serde(default)]
    pub template: String,
    /// 自定义命令：stdin 收到 Claude Code statusLine 格式的 JSON，输出的第一行显示在状态栏上
    #[serde(default)]
    pub command: String,
    /// 没填 command 时，Claude 会话沿用 ~/.claude/settings.json 里的 statusLine
    #[serde(default = "yes")]
    pub use_claude: bool,
}

pub fn default_status_items() -> Vec<String> {
    ["model", "context", "branch", "tokens", "cost"].map(String::from).to_vec()
}

impl Default for StatusLineSettings {
    fn default() -> Self {
        StatusLineSettings {
            items: default_status_items(),
            template: String::new(),
            command: String::new(),
            use_claude: true,
        }
    }
}

// ---------- 方法名常量，两端共用，避免拼错 ----------

/// `yi-agentd bridge`（不带 `--local`，即 SSH 远程、经电脑转发到 SSH 主机）连上守护进程后先发这一行，
/// 守护进程据此把这条连接当成远程连接，只限本机的方法（fs.open、proxy.setHosts）不让调。
/// 故意不带 id：旧版守护进程会当成坏请求忽略，不会回一条客户端没发过的响应。
pub const BRIDGE_REMOTE_MARK: &str = r#"{"yiConsolePeer":"ssh"}"#;

pub mod methods {
    pub const HELLO: &str = "hello";
    pub const TOOLS_DETECT: &str = "tools.detect";
    pub const SESSION_LIST: &str = "session.list";
    pub const SESSION_CREATE: &str = "session.create";
    pub const SESSION_SUBSCRIBE: &str = "session.subscribe";
    pub const SESSION_UNSUBSCRIBE: &str = "session.unsubscribe";
    pub const SESSION_SEND: &str = "session.send";
    pub const SESSION_INTERRUPT: &str = "session.interrupt";
    pub const SESSION_PERMISSION: &str = "session.permission";
    pub const SESSION_KILL: &str = "session.kill";
    pub const SESSION_DELETE: &str = "session.delete";
    pub const SESSION_RENAME: &str = "session.rename";
    pub const PTY_INPUT: &str = "pty.input";
    pub const PTY_RESIZE: &str = "pty.resize";
    /// {sessionId, lines, cancel?} -> {inTmux, position, history}：翻 tmux 里终端会话的历史
    pub const PTY_SCROLL: &str = "pty.scroll";
    pub const HISTORY_LIST: &str = "history.list";
    pub const HISTORY_READ: &str = "history.read";
    pub const FS_LIST: &str = "fs.list";
    /// {path, tail?} -> {path, size, modified, truncated, binary, content}；带 image: true（≤10MB）或 raw: true（任意文件 ≤50MB）时返回 {path, size, modified, base64}
    pub const FS_READ: &str = "fs.read";
    /// {path, content, expectModified?, createOnly?} -> {path, size, modified}
    pub const FS_WRITE: &str = "fs.write";
    /// {path}：新建文件夹
    pub const FS_MKDIR: &str = "fs.mkdir";
    /// {from, to}
    pub const FS_RENAME: &str = "fs.rename";
    /// {path}：删除文件或空文件夹
    pub const FS_DELETE: &str = "fs.delete";
    /// Windows 盘符列表，其他系统返回 ["/"]
    pub const FS_ROOTS: &str = "fs.roots";
    /// {root, query, caseSensitive?, wholeWord?, limit?} -> {hits: [{path(相对), line, col, len, offset, text}], truncated, files}：在文件内容里搜
    pub const FS_GREP: &str = "fs.grep";
    /// {path, data(base64), overwrite?} -> {path, size, modified}：上传文件
    pub const FS_UPLOAD: &str = "fs.upload";
    /// {path, reveal?}：用系统默认程序打开 / 在文件管理器里显示，只接受本机连接
    pub const FS_OPEN: &str = "fs.open";
    pub const STATS_USAGE: &str = "stats.usage";
    pub const SETTINGS_GET: &str = "settings.get";
    pub const SETTINGS_SET: &str = "settings.set";
    pub const DAEMON_SHUTDOWN: &str = "daemon.shutdown";
    /// 会话可选模型 {sessionId} -> {current, models}
    pub const SESSION_MODELS: &str = "session.models";
    /// 切换模型 {sessionId, model}，下一轮生效
    pub const SESSION_SET_MODEL: &str = "session.setModel";
    pub const SESSION_SET_EFFORT: &str = "session.setEffort";
    /// {sessionId, mode}：切换审批模式，进程在跑时立即生效
    pub const SESSION_SET_PERMISSION_MODE: &str = "session.setPermissionMode";
    /// {sessionId} -> [SlashCommand]：工具报告的斜杠命令
    pub const SESSION_COMMANDS: &str = "session.commands";
    /// {sessionId, id, action: remove|sendNow}：管理排队中的消息
    pub const SESSION_QUEUE: &str = "session.queue";
    /// {sessionId, pinned}
    pub const SESSION_PIN: &str = "session.pin";
    /// {sessionId, mode: structured | terminal}：同一个会话在对话模式和终端模式之间切换，接着同一个工具会话
    pub const SESSION_SET_MODE: &str = "session.setMode";
    /// {sessionId, taskId}：停止一个后台任务
    pub const SESSION_STOP_TASK: &str = "session.stopTask";
    pub const SESSION_COMPACT: &str = "session.compact";
    /// {sessionId}：清空对话（/clear）：删掉聊天记录、用量清零，下次发消息新开一个工具会话，Yi Console 会话不变
    pub const SESSION_CLEAR: &str = "session.clear";
    /// 桌面客户端把可转发的 SSH 主机同步给本机 yi-agentd（只接受本机连接）
    pub const PROXY_SET_HOSTS: &str = "proxy.setHosts";
    /// 已配对设备查询可经本机转发的主机
    pub const PROXY_LIST: &str = "proxy.list";
    /// 连接的第一条消息：把这条连接转成到目标主机 `yi-agentd bridge` 的透传
    pub const PROXY_CONNECT: &str = "proxy.connect";
    /// 连接的第一条消息 {port}：之后这条连接原样转到主机上的 127.0.0.1:port（端口转发）
    pub const PORT_CONNECT: &str = "port.connect";
    /// {} -> {ports:[{port, addr, pid?, process?}]}：主机上正在监听的 TCP 端口
    pub const PORTS_LIST: &str = "ports.list";
    /// {root, query, limit} -> [{path, isDir}]：在目录下按名字模糊找文件（输入框 @ 用）
    pub const FS_SEARCH: &str = "fs.search";
    /// {query, limit} -> [{sessionId, seq, ts, kind, snippet}]：在所有会话的消息里搜
    pub const SEARCH_MESSAGES: &str = "search.messages";
    /// {cwd, diff?, paths?} -> {repo, branch, files:[{path, status, diff}]}；diff=false 不算 diff，paths 只返回这些文件
    pub const GIT_CHANGES: &str = "git.changes";
    /// {cwd, paths}：丢弃这些文件的改动
    pub const GIT_DISCARD: &str = "git.discard";
    /// {cwd, paths, message}：提交这些文件
    pub const GIT_COMMIT: &str = "git.commit";
    /// 状态栏要在主机上算的部分 {sessionId} → {git: {branch, ahead, behind, changed}?, command: {text, source}?, error?}
    pub const SESSION_STATUS_LINE: &str = "session.statusLine";
    /// 往前翻更早的事件：{sessionId, beforeSeq, limit} → {events, hasMore}
    pub const SESSION_HISTORY: &str = "session.history";
    /// 取用户消息里的图片 {sessionId, imageId} -> {mimeType, data(base64)}
    pub const SESSION_IMAGE: &str = "session.image";
    /// 粘贴 / 拖进输入框的文件存到会话的上传目录，返回主机上的绝对路径，前端插入 @路径 引用
    pub const SESSION_ATTACH_FILE: &str = "session.attachFile";
    /// {sessionId, path, data(base64)} -> {path, size}：分块上传的后续块，接到 attachFile 建的文件末尾
    pub const SESSION_ATTACH_APPEND: &str = "session.attachAppend";
    /// 换个工具继续：工具自己的会话记录文件 {sessionId} -> {path: string | null}（没有可读文件时为 null）
    pub const SESSION_LOG_FILE: &str = "session.logFile";
    /// 工具可选模型（新建会话时用）{tool} -> [ModelOption]
    pub const TOOLS_MODELS: &str = "tools.models";
    /// 工具账号额度 {tool, refresh?}
    pub const TOOLS_QUOTA: &str = "tools.quota";

    // 通知
    pub const EV_SESSION_EVENT: &str = "session.event";
    pub const EV_SESSION_UPDATED: &str = "session.updated";
    pub const EV_SESSION_REMOVED: &str = "session.removed";
    pub const EV_PTY_OUTPUT: &str = "pty.output";
    pub const EV_NOTIFY: &str = "notify";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_roundtrip() {
        let e = Event {
            seq: 3,
            ts: 1,
            body: EventBody::ToolCall {
                id: "t".into(),
                name: "Bash".into(),
                title: "ls".into(),
                input: serde_json::json!({"command":"ls"}),
                diffs: vec![],
            },
        };
        let s = serde_json::to_string(&e).unwrap();
        assert!(s.contains("\"kind\":\"tool_call\""));
        let back: Event = serde_json::from_str(&s).unwrap();
        assert_eq!(back.body, e.body);
    }

    #[test]
    fn message_untagged() {
        let m: Message = serde_json::from_str(r#"{"id":1,"result":{"a":1}}"#).unwrap();
        assert!(matches!(m, Message::Response(_)));
        let m: Message = serde_json::from_str(r#"{"id":1,"method":"hello"}"#).unwrap();
        assert!(matches!(m, Message::Request(_)));
        let m: Message = serde_json::from_str(r#"{"method":"x","params":{}}"#).unwrap();
        assert!(matches!(m, Message::Notification(_)));
    }
}
