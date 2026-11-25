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
    /// Google's Antigravity CLI (agy), via ACP
    Antigravity,
    /// Any command, terminal mode only
    Shell,
    /// No process: the session page is just the workspace file panel (plus the bottom terminal panel)
    Files,
}

impl ToolKind {
    pub const ALL: [ToolKind; 9] = [
        ToolKind::Claude,
        ToolKind::Codex,
        ToolKind::Cursor,
        ToolKind::Kiro,
        ToolKind::Qoder,
        ToolKind::Codebuddy,
        ToolKind::Antigravity,
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
            ToolKind::Antigravity => "antigravity",
            ToolKind::Shell => "shell",
            ToolKind::Files => "files",
        }
    }

    /// Sessions that never start a process (Files)
    pub fn has_process(&self) -> bool {
        !matches!(self, ToolKind::Files)
    }

    /// AI coding tools that can manage multiple OAuth / API-key accounts
    pub fn supports_accounts(&self) -> bool {
        !matches!(self, ToolKind::Shell | ToolKind::Files)
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
    /// When the tool process last exited with an error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_process_error_at: Option<i64>,
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
    /// True when this child session (`parent_id` set) is a hidden side chat started beside the parent thread
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub side_chat: bool,
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
    /// Optional AI browser (`yi_browser`); disabled again after daemon restart.
    #[serde(default)]
    pub browser_use: bool,
    /// The oldest approval or question still waiting, so lists (the overview board) can show it without subscribing to
    /// the session. Cleared together with `pending_permissions`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<PendingBrief>,
    /// The current or last chat turn (chat mode only)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<TurnInfo>,
    /// Set by a desktop client on a Bot's own session and on the tasks it spawned; absent for ordinary sessions.
    /// The daemon only stores and returns it, so every client can group these sessions under their Bot
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bot: Option<BotLink>,
    /// MCP servers served by a client through the client MCP bridge (see `ClientMcp`)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub client_mcp: Vec<ClientMcp>,
    /// Extra system prompt; delivered per tool like the computer-use hint (Claude `--append-system-prompt`,
    /// Codex `developerInstructions`, ACP tools a `<system-reminder>` before the first prompt of a new tool session)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub append_system_prompt: Option<String>,
    /// Selected account/profile id for this session (None uses tool's active/default account or system default)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_label: Option<String>,
    /// Per-turn hidden git checkpoints (`refs/yi/checkpoints/<sid>/...`) captured around structured turns
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checkpoints: Vec<TurnCheckpoint>,
    /// Metadata for a read-only cross-tool AI code review side chat
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<ReviewMeta>,
    /// Managed Git worktree bound to this session
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<SessionWorktree>,
}

/// Scope and filter metadata attached to a read-only AI code review side chat
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct ReviewMeta {
    /// uncommitted | branch | commits | range
    pub scope: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uncommitted_filter: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_uncommitted: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_base: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_rev: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_rev: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commits: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contiguous: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range_expr: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope_label: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub included_files: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excluded_files: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_files_count: Option<u32>,
}

/// Managed Git worktree bound to an ordinary session (`~/.yi-console/worktrees/<repo>-<hash>/<name>`)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionWorktree {
    /// Main repository root directory this worktree belongs to (used for sidebar grouping and Apply/Merge target)
    pub repo_root: String,
    /// Branch name checked out in this worktree (e.g. `yi/feat-auth`)
    pub branch: String,
    /// Base branch in the main repository when the worktree was created (e.g. `main`)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_branch: Option<String>,
    /// Base commit OID when the worktree was created
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_commit: Option<String>,
    /// True while the branch is still an auto-generated placeholder eligible for automatic semantic rename
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub temporary_branch: bool,
    /// True when the session attached to an existing worktree rather than creating it
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub attached: bool,
}

/// One structured turn's hidden git checkpoint pair (pre-turn base and post-turn end when files changed)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TurnCheckpoint {
    /// 1-based turn index within the session
    pub turn: u32,
    /// Sequence number of the turn's UserMessage event
    pub user_seq: i64,
    /// Sequence number of the turn's TurnEnd event once completed
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_end_seq: Option<i64>,
    /// Hidden git ref captured before the turn started
    pub base_ref: String,
    /// Hidden git ref captured when the turn ended (absent when no files changed)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_ref: Option<String>,
    #[serde(default)]
    pub files_changed: u32,
    #[serde(default)]
    pub added: u64,
    #[serde(default)]
    pub deleted: u64,
    /// When captured during concurrent activity in a shared directory, restricts diff viewing to tool-touched paths
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
}

/// Which Bot a session belongs to. Bot state itself lives in the desktop client that runs the Bot
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct BotLink {
    pub bot_id: String,
    pub bot_name: String,
    /// bot (the Bot's own session) / task (spawned by it); older sessions may say assistant (the removed config assistant)
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    /// The desktop client running the Bot (`client.hello`)
    pub client_id: String,
}

/// An MCP server whose tools are implemented by a connected client. The AI tool starts `yi-agentd client-mcp`, which
/// relays `tools/call` through the daemon to the client (`clientMcp.call` / `clientMcp.result`). The tool list is
/// given up front, so the server starts and lists its tools even while the client is offline
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ClientMcp {
    /// MCP server name as the model sees it ([A-Za-z0-9_-], must not clash with the host's MCP servers)
    pub name: String,
    pub client_id: String,
    /// MCP tool definitions ({name, description, inputSchema})
    #[serde(default)]
    pub tools: Vec<Value>,
    /// Text returned for a tool call while the client is offline, per tool name; tools not listed fail with an error
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub offline: std::collections::BTreeMap<String, String>,
}

/// Summary of a waiting approval / question
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PendingBrief {
    pub request_id: String,
    pub tool_name: String,
    pub title: String,
    /// The concrete command or file path from the request input, truncated
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// command / file / other
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail_kind: Option<String>,
    /// When the request came in (ms)
    pub at: i64,
    /// It asks questions (answered on the session page), not a plain approval
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub question: bool,
    /// The approval's options, so it can be answered from a list; empty for questions
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<PermissionOption>,
}

/// One chat turn: from the user message to `turn_end`
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TurnInfo {
    pub started_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<i64>,
    /// Distinct files the tool reported editing in this turn (from tool call diffs, no git involved)
    #[serde(default, skip_serializing_if = "is_zero")]
    pub files_changed: u32,
    /// The latest tool call of a running turn; cleared when the turn ends. Broadcasts are throttled by the hub
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<TurnActivity>,
    /// The last approval the user rejected in this turn (its command / path, else its title)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rejected: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TurnActivity {
    /// Tool name as the adapter reports it (Bash, Edit, shell, apply_patch, an ACP kind …)
    pub name: String,
    /// One line, truncated
    pub title: String,
    pub at: i64,
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
    /// Type given by the tool, e.g. local_bash, local_agent, workflow, timer
    pub kind: String,
    pub description: String,
    /// When this machine discovered it (milliseconds)
    pub started_at: i64,
    /// Output is written to this file on the host and can be viewed with fs.read
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_file: Option<String>,
    /// True when the tool gives us no way to stop this task from outside (e.g. agy's internal background tasks)
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unstoppable: bool,
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

/// Lightweight summary of a user message in a session, for prompt outline navigation
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UserMessageSummary {
    pub seq: i64,
    pub ts: i64,
    pub text: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub images: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
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

/// A question the AI asks the user (Claude's AskUserQuestion, Codex's requestUserInput, Cursor's ask_question),
/// sent the same way as approvals: PermissionRequest carries questions, and answers come back in answers
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
        /// Who sent it when not the person: `bot-engine` for what the desktop app's Bot engine sends to a Bot or
        /// task session (events, reports, notices), shown as a system note instead of the person's bubble
        #[serde(default, skip_serializing_if = "Option::is_none")]
        origin: Option<String>,
    },
    AssistantText {
        id: String,
        text: String,
    },
    /// Questions attached to the reply `id` that don't block the tool (Codex `request_user_input_async`): the turn
    /// keeps running, and the answer is sent as an ordinary message (steered into the turn if it's still running).
    /// Follows the AssistantText with the same id, which holds the readable text
    AsyncQuestion {
        id: String,
        questions: Vec<Question>,
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
    /// AI browser action metadata (`yi_browser`), like ComputerUse plus the page it ended on
    BrowserUse {
        action_id: String,
        tool: String,
        status: String,
        message: String,
        duration_ms: u64,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        url: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<ImageRef>,
    },
    /// Personal memory change made or proposed by the AI via `yi_memory`
    Memory {
        /// "saved" | "deleted" | "pending_save" | "pending_delete"
        action: String,
        scope: MemoryScope,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project_key: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project_name: Option<String>,
        id: String,
        title: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        description: String,
        #[serde(default)]
        mode: MemoryMode,
        /// Revision id in `.history/` for 1-click undo (present on direct save/delete)
        #[serde(default, skip_serializing_if = "Option::is_none")]
        undo_id: Option<String>,
        /// Pending proposal id (present when host write policy is `confirm`)
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pending_id: Option<String>,
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
        /// The tool keeps this list for the session and can update it after a turn ends.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        session_scoped: bool,
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
    /// Account switched within the same session (manual switch, host-active change, or automatic rate-limit failover)
    AccountSwitched {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from_label: Option<String>,
        to_id: String,
        to_label: String,
        /// manual / rate_limit / host_active
        reason: String,
        /// native_resume / summary_injected / fresh
        context_carried: String,
    },
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum AccountAuthKind {
    #[default]
    Oauth,
    ApiKey,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum AccountIsolation {
    /// Directory and/or env-var isolated: concurrent sessions on one host can use different accounts
    Isolated,
    /// Shared host-level credentials / OS keyring: one account active per host at a time
    #[default]
    HostActive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum AccountState {
    #[default]
    Ready,
    NeedsLogin,
    Limited,
    Error,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AccountIdentity {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_key: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ApiKeyConfig {
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub auth_token: String,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub default_model: String,
    #[serde(default)]
    pub provider_name: String,
    #[serde(default)]
    pub extra_env: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountProfileView {
    pub id: String,
    pub tool: ToolKind,
    pub label: String,
    pub auth_kind: AccountAuthKind,
    pub isolation: AccountIsolation,
    pub active: bool,
    #[serde(default)]
    pub in_use_sessions: usize,
    pub allow_auto_failover: bool,
    pub priority: i32,
    #[serde(default)]
    pub identity: AccountIdentity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_config: Option<ApiKeyConfig>,
    #[serde(default)]
    pub state: AccountState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota: Option<serde_json::Value>,
    #[serde(default)]
    pub updated_at: i64,
    /// What the tool really authenticates with when it differs from `auth_kind` (Codex: a custom provider selected in config.toml)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual: Option<ActualAuth>,
}

/// Codex's `model_provider` in config.toml, when it isn't the built-in OpenAI one: requests go to that provider,
/// so the ChatGPT login in auth.json isn't what the tool uses
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ActualAuth {
    /// providerApiKey (key from `env_key`) | providerOpenaiAuth (`requires_openai_auth`, auth.json sent to the provider) | providerNoAuth
    pub method: String,
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_key: Option<String>,
    /// The key the provider needs is available (env var set, or stored in the account)
    #[serde(default)]
    pub key_set: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolNativeStatus {
    pub logged_in: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_method: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_override: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched_account_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_api_key_matched_id: Option<String>,
    #[serde(default)]
    pub has_local_oauth: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual: Option<ActualAuth>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolAccountsSummary {
    pub tool: ToolKind,
    pub oauth_isolation: AccountIsolation,
    pub supports_oauth: bool,
    pub supports_api_key: bool,
    pub supports_device_auth: bool,
    pub supports_interactive_login: bool,
    pub auto_failover: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_account_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_status: Option<ToolNativeStatus>,
    pub accounts: Vec<AccountProfileView>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountLoginUpdate {
    pub job_id: String,
    pub tool: ToolKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    pub mode: String,
    pub running: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ok: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default)]
    pub needs_input: bool,
    #[serde(default)]
    pub output: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// When the device code stops working (ms since the epoch), if the CLI said so
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
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
    #[serde(default)]
    pub oauth_isolation: AccountIsolation,
    #[serde(default)]
    pub account_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_account_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_account_label: Option<String>,
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
    /// Where `git.worktreeAdd` puts new worktrees (`~` allowed); `~/.yi-console/worktrees` if unset
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_root: Option<String>,
    /// Branch prefix for new session worktrees (e.g. `yi`, `feat`, or `""` for no prefix); defaults to `yi` when unset
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_branch_prefix: Option<String>,
    /// Automatically rename temporary worktree branches from the first prompt or session title; on when unset
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_auto_rename: Option<bool>,
    /// The AI browser (`yi_browser`)
    #[serde(default, skip_serializing_if = "BrowserSettings::is_default")]
    pub browser: BrowserSettings,
    /// Host personal long-term memory (`yi_memory`)
    #[serde(default, skip_serializing_if = "MemorySettings::is_default")]
    pub memory: MemorySettings,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum MemoryScope {
    #[default]
    Global,
    Project,
}

impl MemoryScope {
    pub fn as_str(&self) -> &'static str {
        match self {
            MemoryScope::Global => "global",
            MemoryScope::Project => "project",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "kebab-case")]
pub enum MemoryMode {
    #[default]
    OnDemand,
    Always,
}

impl MemoryMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            MemoryMode::OnDemand => "on-demand",
            MemoryMode::Always => "always",
        }
    }
}

fn default_memory_write_policy() -> String {
    "direct".into()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MemorySettings {
    /// Master switch for host personal long-term memory (`yi_memory`); enabled by default
    #[serde(default = "yes")]
    pub enabled: bool,
    /// AI write policy: "direct" (save + undo snapshot) | "confirm" (pending review) | "readonly"
    #[serde(default = "default_memory_write_policy")]
    pub write_policy: String,
}

impl Default for MemorySettings {
    fn default() -> Self {
        Self {
            enabled: true,
            write_policy: default_memory_write_policy(),
        }
    }
}

impl MemorySettings {
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }

    pub fn normalized_policy(&self) -> &'static str {
        match self.write_policy.trim() {
            "confirm" => "confirm",
            "readonly" => "readonly",
            _ => "direct",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MemoryMeta {
    pub id: String,
    pub scope: MemoryScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_key: Option<String>,
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub mode: MemoryMode,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default)]
    pub updated_at: i64,
    #[serde(default)]
    pub size: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MemoryEntry {
    #[serde(flatten)]
    pub meta: MemoryMeta,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MemoryProjectInfo {
    pub key: String,
    pub display_name: String,
    pub root_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_common_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PendingMemory {
    pub pending_id: String,
    /// "save" | "delete"
    pub action: String,
    pub scope: MemoryScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_name: Option<String>,
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub mode: MemoryMode,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default)]
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MemoryImportCandidate {
    pub source: String,
    pub path: String,
    pub suggested_scope: MemoryScope,
    pub suggested_id: String,
    pub title: String,
    pub description: String,
    pub suggested_mode: MemoryMode,
    pub content: String,
    pub already_exists: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BrowserSettings {
    /// Chrome / Edge / Chromium to use instead of the one found on this host
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable_path: Option<String>,
    /// Keep cookies and logins in one shared profile (one session at a time) instead of a fresh one per session
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub persistent_profile: bool,
    /// Hosts pages may reach (`example.com`, `*.example.com`); empty means no limit
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_domains: Vec<String>,
    /// Action categories always refused (`eval`, `upload`, `download`, `network`, `storage`, ...)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub denied_actions: Vec<String>,
    /// Accept invalid HTTPS certificates on every site (localhost always accepts them)
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ignore_https_errors: bool,
    /// Render a headless browser at 2x so the local live view is sharp on high-DPI screens (SSH / relay views stay small)
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hi_dpi: bool,
}

impl BrowserSettings {
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }
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
    pub const BROWSER_USE_STATUS: &str = "browserUse.status";
    pub const SESSION_BROWSER_USE: &str = "session.browserUse";
    /// Private, locally authenticated `yi_browser` MCP proxy traffic; remote peers cannot invoke it.
    pub const BROWSER_USE_MCP: &str = "browserUse.mcp";
    /// Watch a session's AI browser on this connection: `browser.frame` / `browser.status` notifications follow
    /// (`sessionId`, optional `maxFps` / `maxWidth` / `quality`). Only the latest frame is kept; the next is sent after
    /// `browser.view.ack`
    pub const BROWSER_VIEW_ATTACH: &str = "browser.view.attach";
    pub const BROWSER_VIEW_DETACH: &str = "browser.view.detach";
    pub const BROWSER_VIEW_ACK: &str = "browser.view.ack";
    /// A person's input on the live view: `events` of mouse / key / text / touch / dialog, in viewport CSS pixels
    pub const BROWSER_VIEW_INPUT: &str = "browser.view.input";
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
    /// {cwd, baseRef, endRef, diff?, paths?} -> {repo, baseRef, endRef, total?, files}: diff between two hidden checkpoint refs
    pub const GIT_REF_DIFF: &str = "git.refDiff";
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
    /// {root, path?, line?, col?, symbol?, kind?, pathText?, container?, content?, limit?}
    /// -> {provider, kind, query, atDefinition, locations:[{path, absPath, line, col, len, endLine?, kind, symbol, container?, preview, score, source}]}:
    /// resolve symbol definition(s) or file/import path targets in the workspace
    pub const NAV_DEFINITION: &str = "nav.definition";
    /// {root, path?, line?, col?, symbol, includeDeclaration?, contextLines?, limit?}
    /// -> {provider, symbol, truncated, totalFiles, totalHits, hits:[{path, absPath, line, col, len, text, isDefinition, contextStartLine, context}]}:
    /// find symbol references with surrounding context lines
    pub const NAV_REFERENCES: &str = "nav.references";
    /// {query, limit} -> [{sessionId, seq, ts, kind, snippet}]: search messages across all sessions
    pub const SEARCH_MESSAGES: &str = "search.messages";
    /// {cwd, diff?, paths?} -> {repo, branch, head, total?, files:[{path, status, diff}]}; diff=false skips diffs, paths returns only those files; at most 10000 files, total is set when more were left out
    pub const GIT_CHANGES: &str = "git.changes";
    /// {cwd} -> {branch, ahead, behind, changed} or null when not a git repo. One `git status --porcelain -b` with a
    /// 2 s timeout, no diffs (the overview's project list)
    pub const GIT_STATUS: &str = "git.status";
    /// {cwd, paths}: discard changes to these files
    pub const GIT_DISCARD: &str = "git.discard";
    /// {cwd, paths, message}: commit these files
    pub const GIT_COMMIT: &str = "git.commit";
    /// {cwd, skip?, limit?, query?} -> {repo, commits:[{hash, short, parents, author, time, refs, subject}], more}: commit history of HEAD, newest first
    pub const GIT_LOG: &str = "git.log";
    /// {cwd, hash, paths?} -> {hash, parent, message, author, time, files:[{path, status, added, deleted, diff}]}:
    /// files a commit changed relative to its first parent; diffs only for the files in paths
    pub const GIT_SHOW: &str = "git.show";
    /// {cwd, path, rev} -> {exists, text?, binary, tooLarge, notUtf8}: a file's full content at a revision, for the side-by-side diff.
    /// rev is "HEAD", a commit hash, or a hash followed by "^" (its first parent); path is relative to the repo root
    pub const GIT_BLOB: &str = "git.blob";
    /// {cwd, details?} -> {repo, current, defaultBranch?, worktrees:[{path, head, branch, detached, isMain, isCurrent, bare, prunable, dirtyCount?, upstream?, ahead?, behind?, aheadBase?, merged?}]}: list all worktrees of the repo
    pub const GIT_WORKTREES: &str = "git.worktrees";
    /// {cwd} -> {repo, current, defaultBranch, branches:[{name, current, upstream?, ahead?, behind?, merged?}], remoteBranches:[string]}: list local and remote branches
    pub const GIT_BRANCHES: &str = "git.branches";
    /// {cwd, branch, create?, base?} -> {branch}: switch to an existing (local or remote tracking) branch or create and switch to a new one
    pub const GIT_CHECKOUT: &str = "git.checkout";
    /// {cwd, oldBranch?, newBranch} -> {oldBranch, branch, changed}: rename a local branch via `git branch -m`
    pub const GIT_RENAME_BRANCH: &str = "git.renameBranch";
    /// {cwd, remote?, branch?, setUpstream?} -> {branch, remote, output}: push the current branch to a remote
    pub const GIT_PUSH: &str = "git.push";
    /// {cwd, remote?, prune?} -> {output}: fetch updates from remote(s)
    pub const GIT_FETCH: &str = "git.fetch";
    /// {cwd, rebase?} -> {branch, output}: pull remote changes into the current branch
    pub const GIT_PULL: &str = "git.pull";
    /// {cwd, source, target?, squash?, message?} -> {source, target, output, alreadyUpToDate?}: merge source branch into target (or current) branch
    pub const GIT_MERGE: &str = "git.merge";
    /// {cwd, onto} -> {branch, onto, output}: rebase the current branch onto another ref
    pub const GIT_REBASE: &str = "git.rebase";
    /// {cwd, branch, force?} -> {branch}: delete a local branch
    pub const GIT_BRANCH_DELETE: &str = "git.branchDelete";
    /// {cwd, paths?, deleteBranches?} -> {removed:[string], skipped:[{path, reason}]}: safely remove clean, merged worktrees
    pub const GIT_WORKTREE_PRUNE: &str = "git.worktreePrune";
    /// {cwd, base?, diff?, paths?} -> {repo, branch, base, mergeBase, total?, files:[{path, status, diff, added, deleted}]}:
    /// working tree (including branch commits and untracked files) compared to merge-base(base, HEAD)
    pub const GIT_BRANCH_DIFF: &str = "git.branchDiff";
    /// {cwd, mode, uncommittedFilter?, base?, target?, includeUncommitted?, commits?, range?, diff?, paths?} ->
    /// {repo, mode, branch, base?, target?, targetIsHead?, includeUncommitted?, mergeBase?, baseRev?, endRev?,
    ///  rangeExpr?, contiguous?, commitCount?, counts?, total?, files:[{path, status, diff, added, deleted, size?, binary?, untracked?, staged?}], commits?:[...], error?}:
    /// resolve and list files/stats for a cross-tool code review scope
    pub const GIT_REVIEW_SCOPE: &str = "git.reviewScope";
    /// Parts of the status line computed on the host {sessionId} → {git: {branch, ahead, behind, changed}?, command: {text, source}?, error?}
    pub const SESSION_STATUS_LINE: &str = "session.statusLine";
    /// Page back to older events: {sessionId, beforeSeq, limit} → {events, hasMore}
    pub const SESSION_HISTORY: &str = "session.history";
    /// All user messages in a session (lightweight outline for long-session prompt navigation):
    /// {sessionId} -> {items: [UserMessageSummary]}
    pub const SESSION_USER_MESSAGES: &str = "session.userMessages";
    /// Fetch an image from a user message {sessionId, imageId} -> {mimeType, data(base64)}
    pub const SESSION_IMAGE: &str = "session.image";
    /// Files pasted / dropped into the input box are saved to the session's upload directory; returns the absolute path on the host, and the frontend inserts an @path reference
    pub const SESSION_ATTACH_FILE: &str = "session.attachFile";
    /// {sessionId, path, data(base64)} -> {path, size}: a subsequent chunk of a chunked upload, appended to the file created by attachFile
    pub const SESSION_ATTACH_APPEND: &str = "session.attachAppend";
    /// Continue with another tool: the tool's own session log file {sessionId} -> {path: string | null, hasUser: bool}
    /// (path null when there's no readable file; hasUser: the session has at least one user message)
    pub const SESSION_LOG_FILE: &str = "session.logFile";
    /// Revoke a read grant written by an `@` session reference (`session.send` `readSessions`) or a hand-off
    /// {sessionId, targetId} -> null
    pub const SESSION_REVOKE_READ: &str = "session.revokeRead";
    /// {sessionId} -> [{sessionId, title, tool, status, grantedAt, inherited}]
    pub const SESSION_READ_GRANTS: &str = "session.readGrants";
    /// Budgeted summary of a session for an `@` reference or a hand-off message {sessionId} -> {text}
    pub const SESSION_DIGEST: &str = "session.digest";
    /// Models a tool offers (for new sessions) {tool} -> [ModelOption]
    pub const TOOLS_MODELS: &str = "tools.models";
    /// Cursor model parameters for the new session page {model, cwd?} -> [ModelConfigOption]: cached, else read from a
    /// short-lived Cursor process (several seconds); an empty list is a model without parameters
    pub const TOOLS_MODEL_CONFIG: &str = "tools.modelConfig";
    /// Tool account quota {tool, accountId?, refresh?}
    pub const TOOLS_QUOTA: &str = "tools.quota";
    /// Quotas across all configured accounts {tool?, refresh?, refreshInactive?} -> {tools: [...]}
    pub const TOOLS_QUOTAS: &str = "tools.quotas";
    /// Multi-account configuration & current tool status {tool?, checkNative?} -> {tools: [ToolAccountsSummary]}
    pub const TOOLS_ACCOUNTS_LIST: &str = "tools.accounts.list";
    /// Capture current system/default CLI login as an AccountProfile {tool, label?} -> AccountProfileView
    pub const TOOLS_ACCOUNTS_CAPTURE: &str = "tools.accounts.capture";
    /// Create/update API-key profile or update account metadata/failover policy -> AccountProfileView
    pub const TOOLS_ACCOUNTS_SAVE: &str = "tools.accounts.save";
    /// Remove an account profile {tool, accountId}
    pub const TOOLS_ACCOUNTS_DELETE: &str = "tools.accounts.delete";
    /// Activate/set default account for a tool {tool, accountId}
    pub const TOOLS_ACCOUNTS_ACTIVATE: &str = "tools.accounts.activate";
    /// Set per-tool failover policy {tool, autoFailover}
    pub const TOOLS_ACCOUNTS_SET_POLICY: &str = "tools.accounts.setPolicy";
    /// Start an interactive / device-code / OAuth login flow inside Yi Console {tool, mode?, accountId?, label?, token?} -> AccountLoginUpdate
    pub const TOOLS_ACCOUNTS_LOGIN_START: &str = "tools.accounts.loginStart";
    /// Send auth code / callback URL / PTY input to a running login job {jobId, input}
    pub const TOOLS_ACCOUNTS_LOGIN_INPUT: &str = "tools.accounts.loginInput";
    /// Cancel a running login job {jobId}
    pub const TOOLS_ACCOUNTS_LOGIN_CANCEL: &str = "tools.accounts.loginCancel";
    /// Export account credential bundle for cross-host sync {tool, accountId}
    pub const TOOLS_ACCOUNTS_EXPORT: &str = "tools.accounts.export";
    /// Read unredacted API-key config for revealing secrets when editing an account {tool, accountId} -> ApiKeyConfig
    pub const TOOLS_ACCOUNTS_REVEAL: &str = "tools.accounts.reveal";
    /// Import account credential bundle on this host {bundle}
    pub const TOOLS_ACCOUNTS_IMPORT: &str = "tools.accounts.import";
    /// Codex account whose config.toml selects a custom provider {tool, accountId, to: apiKey | oauth} -> AccountProfileView:
    /// apiKey keeps the provider and stores its key; oauth removes `model_provider` so the ChatGPT login is used
    pub const TOOLS_ACCOUNTS_CONVERT: &str = "tools.accounts.convert";
    /// {tool, ids: [accountId]}: new list order; automatic failover tries accounts top to bottom
    pub const TOOLS_ACCOUNTS_REORDER: &str = "tools.accounts.reorder";
    /// {tool, apiConfig, accountId?} -> {ok, models?, error?}: whether an API key profile's endpoint answers
    /// (Claude / Codex / Antigravity: GET the models list). A redacted key falls back to the saved account's
    pub const TOOLS_ACCOUNTS_TEST: &str = "tools.accounts.test";
    /// Switch account in-place inside an existing session {sessionId, accountId, autoContinue?}
    pub const SESSION_SWITCH_ACCOUNT: &str = "session.switchAccount";
    /// {refresh?} -> {tools: {<tool>: {latest?, error?, via?, canUpdate}}, jobs: {<tool>: ToolJob}, info: [ToolInfo]}: the newest published
    /// version of each tool (npm registry, where the tool is published there), how it was installed, and upgrade jobs
    pub const TOOLS_LATEST: &str = "tools.latest";
    /// {tool} -> {started}: upgrade a tool with its own update command; progress arrives as `tools.updating` events
    pub const TOOLS_UPDATE: &str = "tools.update";
    /// Pending automatic continue after a usage limit {sessionId, action: cancel | now}
    pub const SESSION_AUTO_RESUME: &str = "session.autoResume";
    /// The client's UI language {lang: zh | en}, remembered for messages the daemon sends by itself
    pub const SETTINGS_UI_LANG: &str = "settings.uiLang";

    /// {clientId, kind, name}: a client says who it is, so the daemon can call it back (client MCP bridge).
    /// kind: desktop / mobile / web. The latest connection of a clientId wins
    pub const CLIENT_HELLO: &str = "client.hello";
    /// {callId, result?, error?}: a client answers a `clientMcp.call`
    pub const CLIENT_MCP_RESULT: &str = "clientMcp.result";
    /// Private, locally authenticated traffic from `yi-agentd client-mcp`; remote peers cannot invoke it
    pub const CLIENT_MCP_PROXY: &str = "clientMcp.proxy";
    /// {sessionId, clientMcp: [ClientMcp]}: replace the client MCP servers; applies after the current turn
    pub const SESSION_SET_CLIENT_MCP: &str = "session.setClientMcp";
    /// {sessionId, prompt: string | null}: replace the appended system prompt; applies after the current turn
    pub const SESSION_SET_SYSTEM_PROMPT: &str = "session.setSystemPrompt";
    /// {sessionId, bot: BotLink | null}
    pub const SESSION_SET_BOT: &str = "session.setBot";
    /// {cwd, name, branch?, base?, existingBranch?, prepareEnv?} -> {path, branch, repo}: add a worktree for the repo at cwd under the worktree root
    /// (`<root>/<repo name>-<hash>/<name>`), on a new branch (default `yi/<name>`) from base (default HEAD), or checking out an existing branch
    pub const GIT_WORKTREE_ADD: &str = "git.worktreeAdd";
    /// {path, cwd?, force?, deleteBranch?}: remove a worktree made by git.worktreeAdd (under the worktree root) or any linked worktree of `cwd`'s repo
    pub const GIT_WORKTREE_REMOVE: &str = "git.worktreeRemove";
    /// Migrate an existing main-directory session in-place into a new managed Git worktree {sessionId, name?, branch?, base?} -> SessionInfo
    pub const SESSION_ENTER_WORKTREE: &str = "session.enterWorktree";
    /// Rename the Git branch of a session's bound worktree in-place {sessionId, branch} -> SessionInfo
    pub const SESSION_RENAME_WORKTREE: &str = "session.renameWorktree";
    /// -> [{clientId, name}]: desktop apps registered on this machine (`client.hello` over local IPC), whose Bots a
    /// phone or yi-web can control through `client.call`
    pub const CLIENTS_LIST: &str = "clients.list";
    /// {clientId, cmd, args} -> the command's result: run a `bots.*` / `tasks.*` / `connectors.*` command in that
    /// desktop app (remote control). Not accepted from processes started by this daemon (the AI tools)
    pub const CLIENT_CALL: &str = "client.call";
    /// {callId, result?, error?}: a desktop app answers a `client.call` notification
    pub const CLIENT_RESULT: &str = "client.result";
    /// {event}: a registered desktop app pushes a state change (`bots.changed`, ...) to every connected client
    pub const CLIENT_EVENT: &str = "client.event";

    /// Host personal long-term memory (`yi_memory`)
    /// {cwd?, projectKey?} -> {resolvedProject?, projects, global, project, byProject, pending, settings}
    pub const MEMORY_LIST: &str = "memory.list";
    /// {scope, projectKey?, cwd?, id} -> MemoryEntry
    pub const MEMORY_GET: &str = "memory.get";
    /// {scope, projectKey?, cwd?, id, title?, description?, mode?, tags?, content, prevId?} -> {entry, undoId}
    pub const MEMORY_SAVE: &str = "memory.save";
    /// {scope, projectKey?, cwd?, id} -> {undoId}
    pub const MEMORY_DELETE: &str = "memory.delete";
    /// {fromScope, fromProjectKey?, id, toScope, toProjectKey?, cwd?, newId?} -> {entry, undoId}
    pub const MEMORY_MOVE: &str = "memory.move";
    /// {undoId} -> {ok, action, scope, id}
    pub const MEMORY_UNDO: &str = "memory.undo";
    /// {pendingId, approve, scope?, projectKey?, id?, title?, description?, mode?, tags?, content?} -> {ok, entry?}
    pub const MEMORY_RESOLVE_PENDING: &str = "memory.resolvePending";
    /// {cwd?, projectKey?} -> {prompt, project?, alwaysCount, indexCount, omittedCount, bytes}
    pub const MEMORY_PREVIEW: &str = "memory.preview";
    /// {cwd?} -> {candidates, project?}
    pub const MEMORY_SCAN_IMPORT: &str = "memory.scanImport";
    /// {items: [...]} -> {imported, skipped}
    pub const MEMORY_IMPORT: &str = "memory.import";
    /// {projectKey, cwd} -> MemoryProjectInfo
    pub const MEMORY_REBIND_PROJECT: &str = "memory.rebindProject";
    /// Private, locally authenticated `yi_memory` MCP proxy traffic; remote peers cannot invoke it
    pub const MEMORY_MCP: &str = "memory.mcp";
    /// Private, locally authenticated `yi_session` read-only MCP proxy traffic; remote peers cannot invoke it
    pub const SESSION_MCP: &str = "session.mcp";
    /// Private, locally authenticated `yi_review` MCP proxy traffic; remote peers cannot invoke it.
    pub const REVIEW_MCP: &str = "review.mcp";

    // Notifications
    pub const EV_SESSION_EVENT: &str = "session.event";
    pub const EV_SESSION_UPDATED: &str = "session.updated";
    pub const EV_SESSION_REMOVED: &str = "session.removed";
    pub const EV_PTY_OUTPUT: &str = "pty.output";
    pub const EV_NOTIFY: &str = "notify";
    /// {tool, job: ToolJob, line?, tools?}: an upgrade started, printed a line, or ended (then with the re-detected tools)
    pub const EV_TOOLS_UPDATING: &str = "tools.updating";
    /// {tool?}: account list or active account changed on the host
    pub const EV_ACCOUNTS_UPDATED: &str = "accounts.updated";
    /// AccountLoginUpdate: interactive login progress, OAuth link / device code / completion
    pub const EV_ACCOUNT_LOGIN: &str = "tools.accountLogin";
    /// {callId, sessionId, server, tool, arguments}: sent only to the client named in the session's `ClientMcp`;
    /// it answers with `clientMcp.result`
    pub const EV_CLIENT_MCP_CALL: &str = "clientMcp.call";
    /// {callId, cmd, args}: a remote-control command for this desktop app; answered with `client.result`
    pub const EV_CLIENT_CALL: &str = "client.call";
    /// {clientId, event}: a desktop app's state changed (from `client.event`)
    pub const EV_CLIENT_EVENT: &str = "client.event";
    /// {}: a desktop app registered or went away; refetch `clients.list`
    pub const EV_CLIENTS_CHANGED: &str = "clients.changed";
    /// A JPEG frame of a watched AI browser, only to connections that attached
    pub const EV_BROWSER_FRAME: &str = "browser.frame";
    /// Address, tabs, dialog, busy and paused state of a watched AI browser
    pub const EV_BROWSER_STATUS: &str = "browser.status";
    /// {}: host personal memories or pending proposals changed
    pub const EV_MEMORY_CHANGED: &str = "memory.changed";
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
