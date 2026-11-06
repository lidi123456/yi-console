//! Protocol between the Yi Console client and yi-agentd.
//!
//! The transport is newline-delimited JSON (one JSON-RPC 2.0 message per line), over a unix socket / named pipe locally,
//! and over the stdio of SSH exec `yi-agentd bridge` remotely.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROTOCOL_VERSION: u32 = 1;
pub const DAEMON_VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "+", env!("YI_CONSOLE_BUILD"));

/// Build time (unix seconds); the client uses it to tell whether the daemon is older than itself
pub fn build_time() -> u64 {
    env!("YI_CONSOLE_BUILD_TIME").parse().unwrap_or(0)
}

// ---------- JSON-RPC envelope ----------

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

/// A line on the wire may be one of three kinds. Order matters: Request requires id + method
/// and must come before Response, which only requires id.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Message {
    Request(Request),
    Response(Response),
    Notification(Notification),
}

// ---------- Domain model ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ToolKind {
    Claude,
    Codex,
    Cursor,
    Kiro,
    /// Alibaba's Qoder CLI (qodercli), via ACP
    Qoder,
    /// Tencent's CodeBuddy Code (codebuddy), via ACP
    Codebuddy,
    /// Any command, terminal mode only
    Shell,
    /// No process: the session page is just the workspace file panel (plus the bottom terminal panel)
    Files,
}

impl ToolKind {
    pub const ALL: [ToolKind; 8] = [
        ToolKind::Claude,
        ToolKind::Codex,
        ToolKind::Cursor,
        ToolKind::Kiro,
        ToolKind::Qoder,
        ToolKind::Codebuddy,
        ToolKind::Shell,
        ToolKind::Files,
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
            ToolKind::Files => "files",
        }
    }

    /// Sessions that never start a process (Files)
    pub fn has_process(&self) -> bool {
        !matches!(self, ToolKind::Files)
    }

    pub fn parse(s: &str) -> Option<ToolKind> {
        ToolKind::ALL.into_iter().find(|t| t.as_str() == s)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SessionMode {
    /// Structured: parse the tool's machine interface and render it in the unified chat UI
    Structured,
    /// Terminal: PTY passthrough, rendered with xterm.js
    Terminal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SessionStatus {
    Starting,
    Idle,
    Running,
    WaitingPermission,
    /// The tool process has exited; sending a new message resumes automatically
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
    /// Tools billed in credits, such as Kiro
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
    /// The tool's own session id (claude session_id / codex thread id / ACP sessionId), used to resume
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
    /// Percentage of the context window used (0–100), only if the tool reports it
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_percent: Option<f64>,
    /// Permission mode: default / acceptEdits / plan / bypass; kept when the session is resumed
    #[serde(default)]
    pub permission_mode: Option<String>,
    /// Thinking level; the tool's default if unset
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Session-local ACP model parameters, including their supported values
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub model_config_options: Vec<ModelConfigOption>,
    /// The selected model is waiting for an authoritative ACP parameter snapshot.
    #[serde(default)]
    pub model_config_pending: bool,
    /// Pinned in the sidebar
    #[serde(default)]
    pub pinned: bool,
    /// Messages to send once the current turn ends. Kept in the `queues` table, so they survive daemon restarts
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub queued: Vec<QueuedMessage>,
    /// The queue is held: nothing is sent when a turn ends until the user resumes it. Why: restart (the daemon restarted
    /// with messages queued), interrupt (the user interrupted the turn), user (paused by hand), handoff (moved here from
    /// the session this one took over)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_paused: Option<String>,
    /// A chat-mode turn was still running when the daemon stopped (its tool process went with it). Set by `restore`,
    /// cleared by the next message or `session.interrupted`. "Resume" on the queue first asks the tool to finish that turn
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub restart_interrupted: bool,
    /// Tasks the tool runs in the background (in memory only, cleared when the process exits)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub background_tasks: Vec<BackgroundTask>,
    /// Terminal opened in the session page's bottom panel: id of the chat session it belongs to. Not listed separately in the sidebar; deleted along with the chat
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// Number of user messages already in the tool session when switching from chat to terminal mode. Switching back to chat only imports the part after this
    /// (i.e. what was said in the terminal); no value means the session started in terminal mode, and everything is imported on switching back
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synced_users: Option<u32>,
    /// The tool hit its usage limit; "continue" goes out automatically at `at` (chat mode, Claude / Codex). Persisted, so it survives daemon restarts
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_resume: Option<AutoResume>,
    /// Usage limit hits in a row since the last turn that ended normally; past `MAX_AUTO_RESUMES` no more automatic continues
    #[serde(default, skip_serializing_if = "is_zero")]
    pub limit_hits: u32,
    /// Optional desktop control; disabled again after daemon restart.
    #[serde(default)]
    pub computer_use: bool,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// Pending automatic continue after a usage limit
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AutoResume {
    /// When the continue message is sent (ms)
    pub at: i64,
    /// When the tool says the limit resets (ms)
    pub resets_at: i64,
    /// Which limit was hit (five_hour / seven_day / …), if the tool says
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<String>,
}

/// Automatic continues in a row before giving up (the limit keeps coming back right after it resets)
pub const MAX_AUTO_RESUMES: u32 = 3;
/// Limits that reset further out than this aren't waited for automatically (same as Claude Code's own auto-continue)
pub const AUTO_RESUME_HORIZON_MS: i64 = 24 * 3600 * 1000;
/// Wait this long after the reset time before continuing, so the first request isn't rejected again
pub const AUTO_RESUME_DELAY_MS: i64 = 60 * 1000;

/// Background tasks: Claude's background shells, background subagents, etc.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundTask {
    pub id: String,
    /// Type given by the tool, e.g. local_bash, local_agent
    pub kind: String,
    pub description: String,
    /// When this machine discovered it (milliseconds)
    pub started_at: i64,
    /// Output is written to this file on the host and can be viewed with fs.read
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_file: Option<String>,
}

/// A queued message
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct QueuedMessage {
    pub id: String,
    pub text: String,
    #[serde(default)]
    pub images: u32,
}

/// A slash command provided by the tool
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SlashCommand {
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Argument hint
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub hint: String,
}

/// A select parameter reported by an ACP tool. Model and permission-mode selectors are handled separately.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ModelConfigOption {
    pub id: String,
    pub name: String,
    /// ACP category identifies reasoning controls independently of provider-specific parameter ids.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub category: String,
    #[serde(default)]
    pub description: String,
    pub current_value: String,
    pub options: Vec<ModelConfigValue>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelConfigValue {
    pub value: String,
    pub name: String,
}

/// A model the tool offers
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ModelOption {
    /// Value passed to the tool
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// The model an alias actually maps to (Claude's opus / sonnet etc.), used to match the session's current model
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub resolved: String,
    /// Supported thinking levels (low / medium / high / xhigh / max …); empty means not adjustable
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub efforts: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub default_effort: String,
}

/// An image in a user message. The file lives in yi-agentd's data directory; fetch the content with session.image
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
    /// Used when the tool provides a unified diff directly
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

/// A question the AI asks the user (Claude's AskUserQuestion, Codex's requestUserInput, Cursor's ask_question,
/// numbered options in Kiro's reply text), sent the same way as approvals: PermissionRequest carries questions, and answers come back in answers
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct Question {
    pub id: String,
    /// Short title (the header from Claude / Codex)
    #[serde(default)]
    pub header: String,
    pub question: String,
    #[serde(default)]
    pub options: Vec<QuestionOption>,
    #[serde(default)]
    pub multi_select: bool,
    /// Allow skipping the options and writing free text
    #[serde(default)]
    pub allow_other: bool,
    /// Input isn't echoed (passwords and the like)
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
    /// Selected option ids
    #[serde(default)]
    pub selected: Vec<String>,
    /// Free-text answer
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
}

/// The two fixed option ids for question requests
pub const QUESTION_ANSWER: &str = "answer";
pub const QUESTION_SKIP: &str = "skip";

/// Unified event. `seq` is assigned by yi-agentd; `delta` events aren't persisted and have no seq.
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
    /// Streaming chunk, pushed but not persisted; AssistantText holds the final content
    AssistantDelta {
        id: String,
        delta: String,
    },
    Thinking {
        id: String,
        text: String,
    },
    /// Streaming chunk of thinking; like AssistantDelta, pushed but not persisted
    ThinkingDelta {
        id: String,
        delta: String,
    },
    /// Desktop action metadata; images use the session's existing image endpoint.
    ComputerUse {
        action_id: String,
        tool: String,
        status: String,
        message: String,
        duration_ms: u64,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<ImageRef>,
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
        /// Model used for this turn; if the adapter leaves it empty, the hub fills in the session's current model
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
    /// Context compaction: phase = start / done / failed; trigger = auto / manual (empty if the tool doesn't say)
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
    /// Usage limit. phase:
    /// - hit: reported by the adapter (`resets_at` ms and `limit` when known)
    /// - scheduled: the hub will send "continue" at `resume_at`
    /// - skipped: no automatic continue; `reason` = off / unknown / far / repeated
    /// - resumed: "continue" was sent (`reason` = auto / now)
    /// - cancelled: `reason` = user / manual (the user sent a message) / off / busy
    RateLimit {
        phase: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resets_at: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resume_at: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// A turn was cut off because the daemon stopped (recorded when it starts again)
    Interrupted {
        reason: String,
    },
    /// Continue with another tool: `to` true in the original session (the work went to `sessionId`), false in the new
    /// one (it took over from `sessionId`)
    Handoff {
        session_id: String,
        tool: ToolKind,
        to: bool,
    },
    /// /clear: all events before this one were deleted; on receipt the client drops events it holds with a smaller seq
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
    /// 0 means a transient event (not persisted)
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
    /// Whether structured mode is supported
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
    /// Older daemons lack this field; treated as 0 (oldest)
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
    /// Whether history can be read offline (claude/codex read files; ACP tools need a resume to view)
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
    /// Override the executable path
    #[serde(default)]
    pub command: String,
    /// Extra arguments
    #[serde(default)]
    pub extra_args: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonSettings {
    /// Host-owned MCP definitions shared by AI chat adapters; an empty tool list selects all AI tools.
    #[serde(default)]
    pub mcp_servers: Vec<McpServerConfig>,
    #[serde(default)]
    pub tools: std::collections::BTreeMap<String, ToolOverride>,
    #[serde(default)]
    pub status_line: StatusLineSettings,
    /// Prompt snippets: selectable by typing / in the input box
    #[serde(default)]
    pub snippets: Vec<Snippet>,
    /// Minutes a session can idle before its tool process is ended to save memory; the next message resumes automatically;
    /// 30 minutes if unset, 0 means never release
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_release_minutes: Option<u32>,
    /// Max concurrent chat-mode tool processes; beyond that the least recently used idle process is ended (the next message reconnects automatically);
    /// 5 if unset, 0 means unlimited. Terminal mode doesn't count
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_live_processes: Option<u32>,
    /// Run terminal sessions inside tmux (survives daemon restarts); on if unset, no effect without tmux installed
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_tmux: Option<bool>,
    /// After a usage limit, send "continue" automatically once it resets (chat mode, Claude / Codex); on if unset
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_resume: Option<bool>,
    /// UI language last reported by a client (zh / en), used for the automatic continue message
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui_lang: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum McpTransport {
    #[default]
    Stdio,
    Http,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct McpServerConfig {
    pub name: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub transport: McpTransport,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub headers: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub tools: Vec<ToolKind>,
}

pub const DEFAULT_IDLE_RELEASE_MINUTES: u32 = 30;
pub const DEFAULT_MAX_LIVE_PROCESSES: u32 = 5;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct Snippet {
    pub name: String,
    pub text: String,
}

/// Status line under the session input box
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct StatusLineSettings {
    /// Built-in fields, shown in order (model / context / tokens / cost / lastTurn / dir / branch / status / quota …)
    #[serde(default = "default_status_items")]
    pub items: Vec<String>,
    /// Custom format, e.g. `{model} · {branch}{dirty}`; replaces items when non-empty
    #[serde(default)]
    pub template: String,
    /// Custom command: receives JSON in Claude Code statusLine format on stdin; the first line of output is shown in the status line
    #[serde(default)]
    pub command: String,
    /// Without command, Claude sessions reuse the statusLine from ~/.claude/settings.json
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

// ---------- Method name constants, shared by both ends to avoid typos ----------

/// `yi-agentd bridge` (without `--local`, i.e. remote over SSH, or forwarded through a computer to an SSH host) sends this line first after connecting to the daemon,
/// so the daemon treats the connection as remote and refuses local-only methods (fs.open, proxy.setHosts).
/// Deliberately has no id: older daemons ignore it as a bad request instead of replying with a response the client never asked for.
pub const BRIDGE_REMOTE_MARK: &str = r#"{"yiConsolePeer":"ssh"}"#;

pub mod methods {
    pub const HELLO: &str = "hello";
    pub const TOOLS_DETECT: &str = "tools.detect";
    pub const SESSION_LIST: &str = "session.list";
    pub const COMPUTER_USE_STATUS: &str = "computerUse.status";
    pub const COMPUTER_USE_TARGETS: &str = "computerUse.targets";
    pub const SESSION_COMPUTER_USE: &str = "session.computerUse";
    /// Private, locally authenticated MCP proxy traffic; remote peers cannot invoke it.
    pub const COMPUTER_USE_MCP: &str = "computerUse.mcp";
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
    /// {sessionId, lines, cancel?} -> {inTmux, position, history}: scroll the history of a terminal session in tmux
    pub const PTY_SCROLL: &str = "pty.scroll";
    pub const HISTORY_LIST: &str = "history.list";
    pub const HISTORY_READ: &str = "history.read";
    pub const FS_LIST: &str = "fs.list";
    /// {path, tail?} -> {path, size, modified, truncated, binary, content}; with image: true (≤10MB) or raw: true (any file ≤50MB) returns {path, size, modified, base64}
    pub const FS_READ: &str = "fs.read";
    /// {path, content, expectModified?, createOnly?} -> {path, size, modified}
    pub const FS_WRITE: &str = "fs.write";
    /// {path}: create a folder
    pub const FS_MKDIR: &str = "fs.mkdir";
    /// {from, to}
    pub const FS_RENAME: &str = "fs.rename";
    /// {path}: delete a file or empty folder
    pub const FS_DELETE: &str = "fs.delete";
    /// Drive list on Windows; ["/"] on other systems
    pub const FS_ROOTS: &str = "fs.roots";
    /// {root, query, caseSensitive?, wholeWord?, regex?, include?, exclude?(comma-separated globs), paths?(only these), replace?, preserveCase?, limit?}
    /// -> {hits: [{path(relative), line, col, len, offset, text, matches:[[col, len, text, replacement?]]}], truncated, files, stat:{path:[modified, size]}}: search file contents
    pub const FS_GREP: &str = "fs.grep";
    /// {same options as fs.grep with replace, files:[{path, stat?, matches?:[[line, col]], content?}], dryRun?}
    /// -> {files:[{path, abs, count, before?, after?, modified?, size?}], skipped:[{path, reason}]}: replace search results
    pub const FS_REPLACE: &str = "fs.replace";
    /// {path, data(base64), overwrite?} -> {path, size, modified}: upload a file
    pub const FS_UPLOAD: &str = "fs.upload";
    /// {path, reveal?}: open with the system default program / reveal in the file manager; local connections only
    pub const FS_OPEN: &str = "fs.open";
    pub const STATS_USAGE: &str = "stats.usage";
    pub const SETTINGS_GET: &str = "settings.get";
    pub const SETTINGS_SET: &str = "settings.set";
    pub const DAEMON_SHUTDOWN: &str = "daemon.shutdown";
    /// Models available to a session {sessionId} -> {current, models}
    pub const SESSION_MODELS: &str = "session.models";
    /// Switch model {sessionId, model}, effective next turn
    pub const SESSION_SET_MODEL: &str = "session.setModel";
    pub const SESSION_SET_MODEL_CONFIG: &str = "session.setModelConfig";
    pub const SESSION_SET_EFFORT: &str = "session.setEffort";
    /// {sessionId, mode}: switch permission mode, effective immediately while the process runs
    pub const SESSION_SET_PERMISSION_MODE: &str = "session.setPermissionMode";
    /// {sessionId} -> [SlashCommand]: slash commands reported by the tool
    pub const SESSION_COMMANDS: &str = "session.commands";
    /// {sessionId, action, id?, text?, to?}: manage queued messages. With `id`: remove (returns {text} for the input box),
    /// sendNow, edit {text}, move {to: index}. Whole queue: pause, resume (sends the next one if the session is idle), clear
    pub const SESSION_QUEUE: &str = "session.queue";
    /// {sessionId, action: continue | dismiss}: a turn cut off by a daemon restart (`restart_interrupted`); continue
    /// asks the tool to finish it, and queued messages follow after that turn
    pub const SESSION_INTERRUPTED: &str = "session.interrupted";
    /// {sessionId, pinned}
    pub const SESSION_PIN: &str = "session.pin";
    /// {sessionId, mode: structured | terminal}: switch the same session between chat and terminal mode, continuing the same tool session
    pub const SESSION_SET_MODE: &str = "session.setMode";
    /// {sessionId, taskId}: stop a background task
    pub const SESSION_STOP_TASK: &str = "session.stopTask";
    pub const SESSION_COMPACT: &str = "session.compact";
    /// {sessionId}: clear the conversation (/clear): delete the chat history and reset usage; the next message starts a new tool session; the Yi Console session stays
    pub const SESSION_CLEAR: &str = "session.clear";
    /// The desktop client syncs forwardable SSH hosts to the local yi-agentd (local connections only)
    pub const PROXY_SET_HOSTS: &str = "proxy.setHosts";
    /// Paired devices query the hosts reachable through this machine
    pub const PROXY_LIST: &str = "proxy.list";
    /// First message on a connection: turn it into a passthrough to `yi-agentd bridge` on the target host
    pub const PROXY_CONNECT: &str = "proxy.connect";
    /// First message on a connection {port}: afterwards the connection is relayed as is to 127.0.0.1:port on the host (port forwarding)
    pub const PORT_CONNECT: &str = "port.connect";
    /// {} -> {ports:[{port, addr, pid?, process?}]}: TCP ports listening on the host
    pub const PORTS_LIST: &str = "ports.list";
    /// {root, query, limit} -> [{path, isDir}]: fuzzy-find files by name under a directory (for @ in the input box)
    pub const FS_SEARCH: &str = "fs.search";
    /// {query, limit} -> [{sessionId, seq, ts, kind, snippet}]: search messages across all sessions
    pub const SEARCH_MESSAGES: &str = "search.messages";
    /// {cwd, diff?, paths?} -> {repo, branch, head, total?, files:[{path, status, diff}]}; diff=false skips diffs, paths returns only those files; at most 10000 files, total is set when more were left out
    pub const GIT_CHANGES: &str = "git.changes";
    /// {cwd, paths}: discard changes to these files
    pub const GIT_DISCARD: &str = "git.discard";
    /// {cwd, paths, message}: commit these files
    pub const GIT_COMMIT: &str = "git.commit";
    /// {cwd, skip?, limit?} -> {repo, commits:[{hash, short, parents, author, time, refs, subject}], more}: commit history of HEAD, newest first
    pub const GIT_LOG: &str = "git.log";
    /// {cwd, hash, paths?} -> {hash, parent, message, author, time, files:[{path, status, added, deleted, diff}]}:
    /// files a commit changed relative to its first parent; diffs only for the files in paths
    pub const GIT_SHOW: &str = "git.show";
    /// {cwd, path, rev} -> {exists, text?, binary, tooLarge, notUtf8}: a file's full content at a revision, for the side-by-side diff.
    /// rev is "HEAD", a commit hash, or a hash followed by "^" (its first parent); path is relative to the repo root
    pub const GIT_BLOB: &str = "git.blob";
    /// Parts of the status line computed on the host {sessionId} → {git: {branch, ahead, behind, changed}?, command: {text, source}?, error?}
    pub const SESSION_STATUS_LINE: &str = "session.statusLine";
    /// Page back to older events: {sessionId, beforeSeq, limit} → {events, hasMore}
    pub const SESSION_HISTORY: &str = "session.history";
    /// Fetch an image from a user message {sessionId, imageId} -> {mimeType, data(base64)}
    pub const SESSION_IMAGE: &str = "session.image";
    /// Files pasted / dropped into the input box are saved to the session's upload directory; returns the absolute path on the host, and the frontend inserts an @path reference
    pub const SESSION_ATTACH_FILE: &str = "session.attachFile";
    /// {sessionId, path, data(base64)} -> {path, size}: a subsequent chunk of a chunked upload, appended to the file created by attachFile
    pub const SESSION_ATTACH_APPEND: &str = "session.attachAppend";
    /// Continue with another tool: the tool's own session log file {sessionId} -> {path: string | null, hasUser: bool}
    /// (path null when there's no readable file; hasUser: the session has at least one user message)
    pub const SESSION_LOG_FILE: &str = "session.logFile";
    /// Models a tool offers (for new sessions) {tool} -> [ModelOption]
    pub const TOOLS_MODELS: &str = "tools.models";
    /// Tool account quota {tool, refresh?}
    pub const TOOLS_QUOTA: &str = "tools.quota";
    /// {refresh?} -> {tools: {<tool>: {latest?, error?, via?, canUpdate}}, jobs: {<tool>: ToolJob}, info: [ToolInfo]}: the newest published
    /// version of each tool (npm registry, where the tool is published there), how it was installed, and upgrade jobs
    pub const TOOLS_LATEST: &str = "tools.latest";
    /// {tool} -> {started}: upgrade a tool with its own update command; progress arrives as `tools.updating` events
    pub const TOOLS_UPDATE: &str = "tools.update";
    /// Pending automatic continue after a usage limit {sessionId, action: cancel | now}
    pub const SESSION_AUTO_RESUME: &str = "session.autoResume";
    /// The client's UI language {lang: zh | en}, remembered for messages the daemon sends by itself
    pub const SETTINGS_UI_LANG: &str = "settings.uiLang";

    // Notifications
    pub const EV_SESSION_EVENT: &str = "session.event";
    pub const EV_SESSION_UPDATED: &str = "session.updated";
    pub const EV_SESSION_REMOVED: &str = "session.removed";
    pub const EV_PTY_OUTPUT: &str = "pty.output";
    pub const EV_NOTIFY: &str = "notify";
    /// {tool, job: ToolJob, line?, tools?}: an upgrade started, printed a line, or ended (then with the re-detected tools)
    pub const EV_TOOLS_UPDATING: &str = "tools.updating";
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
