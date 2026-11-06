//! Adapters for each AI coding tool. Each adapter is a tokio task that owns the tool's child process,
//! receives commands on `rx`, and translates tool output into unified events for the `Emitter`.

pub mod acp;
pub mod claude;
pub mod codex;
pub mod pty;

use crate::hub::Emitter;
use yi_proto::{QuestionAnswer, SessionMode, ToolKind};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc;

/// Tells the model in chat mode how to show diagrams and images: Yi Console displays Markdown images in replies that reference local files on the host.
/// Claude gets it via `--append-system-prompt`, Codex via `developerInstructions`; ACP has no system prompt entry point, so it's not added
pub const IMAGE_HINT: &str = "Your replies are shown in the Yi Console app, which renders Markdown. \
To show the user an image file on this machine (a screenshot, chart or generated picture), embed it with \
Markdown image syntax and its absolute path, e.g. ![chart](/abs/path/chart.png); wrap a path containing \
spaces in angle brackets: ![chart](</abs/path/my chart.png>). \
For flowcharts and other supported diagrams, use fenced code blocks with the language mermaid; \
Yi Console renders them inline. Start with the diagram type, use plain text labels, close the code fence, \
and omit configuration directives, HTML, external resources, click actions and custom style statements.";

/// The tool's turn failed because the account's usage limit is used up. The hub decides whether and when to continue automatically.
/// Adapters for other tools only need to emit this before `TurnEnd` to get automatic continue (`resets_at` ms; None lets the hub ask the quota probe)
pub fn limit_hit(resets_at: Option<i64>, limit: Option<String>) -> yi_proto::EventBody {
    yi_proto::EventBody::RateLimit { phase: "hit".into(), resets_at, limit, resume_at: None, reason: None }
}

/// Fallback when the tool gives no structured signal: Claude "You've hit your limit · resets 3pm",
/// "Claude AI usage limit reached|<epoch>" from older versions, Codex "You’ve hit your usage limit. Try again at …"
pub fn is_usage_limit_text(msg: &str) -> bool {
    let m = msg.to_lowercase().replace('’', "'");
    m.contains("you've hit your") && m.contains("limit") || m.contains("usage limit reached")
}

/// One piece of user input
#[derive(Debug, Clone)]
pub struct Prompt {
    pub text: String,
    pub images: Vec<PromptImage>,
}

#[derive(Debug, Clone)]
pub struct PromptImage {
    pub mime_type: String,
    /// File already saved in yi-agentd's data directory (Codex reads it by path)
    pub path: PathBuf,
    /// base64 (inlined directly for Claude and ACP)
    pub data: String,
}

#[derive(Debug)]
pub enum AdapterCmd {
    Send(Prompt),
    /// Switch model, effective next turn
    SetModel(String),
    /// Set a supported ACP model parameter without restarting the process
    SetModelConfig { id: String, value: String },
    /// Thinking level; an empty string restores the tool's default
    SetEffort(String),
    /// Switch permission mode (default / acceptEdits / plan / bypass)
    SetPermissionMode(String),
    Interrupt,
    /// Stop a background task
    StopTask(String),
    /// Answer an approval; for question requests option_id is answer / skip and the content is in answers
    Permission { request_id: String, option_id: String, answers: Vec<QuestionAnswer> },
    Kill,
    PtyInput(Vec<u8>),
    PtyResize { cols: u16, rows: u16 },
}

#[derive(Debug, Clone)]
pub struct StartSpec {
    pub tool: ToolKind,
    pub mode: SessionMode,
    pub cwd: PathBuf,
    /// Tool-side session id; resumes when set
    pub resume: Option<String>,
    /// Whether history replayed by the tool on resume is recorded in the event log (true when importing history)
    pub replay: bool,
    /// During replay, skip this many user messages (with their replies) before recording: switching back to chat only records what was said in the terminal
    pub replay_skip: usize,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub model_config_options: Vec<yi_proto::ModelConfigOption>,
    /// default / acceptEdits / bypass / plan
    pub permission_mode: String,
    pub command: Option<String>,
    pub program: PathBuf,
    pub extra_args: Vec<String>,
    /// Session-scoped stdio servers supplied through ACP, separate from CLI arguments.
    pub mcp_servers: Vec<serde_json::Value>,
    pub mcp_fingerprint: String,
    pub cols: u16,
    pub rows: u16,
    /// Run terminal mode inside this tmux session (so it survives daemon restarts)
    pub tmux: Option<String>,
    /// Session id preassigned (--session-id) when terminal mode starts a new Claude session; later restarts --resume it
    pub session_id: Option<String>,
}

pub fn start(spec: StartSpec, em: Emitter, rx: mpsc::UnboundedReceiver<AdapterCmd>) {
    match (spec.mode, spec.tool) {
        (SessionMode::Terminal, _) | (_, ToolKind::Shell) => pty::start(spec, em, rx),
        (SessionMode::Structured, ToolKind::Claude) => {
            tokio::spawn(claude::run(spec, em, rx));
        }
        (SessionMode::Structured, ToolKind::Codex) => {
            tokio::spawn(codex::run(spec, em, rx));
        }
        (SessionMode::Structured, ToolKind::Cursor | ToolKind::Kiro | ToolKind::Qoder | ToolKind::Codebuddy) => {
            tokio::spawn(acp::run(spec, em, rx));
        }
        // Files sessions have no process; Hub::launch never gets here
        (SessionMode::Structured, ToolKind::Files) => {}
    }
}

/// On Windows, npm-installed tools are .cmd scripts that can't be launched with CreateProcess directly and must go through cmd /c
pub fn std_command(program: &Path, args: &[String]) -> std::process::Command {
    let is_script = program
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("cmd") || e.eq_ignore_ascii_case("bat"))
        .unwrap_or(false);
    let mut c = if cfg!(windows) && is_script {
        let mut c = std::process::Command::new("cmd");
        c.arg("/c").arg(program);
        c
    } else {
        std::process::Command::new(program)
    };
    c.args(args);
    c
}

/// A child process that talks line-delimited JSON
pub struct JsonProc {
    pub child: Child,
    pub stdin: ChildStdin,
    pub stdout: tokio::io::Lines<BufReader<ChildStdout>>,
    pub stderr_tail: Arc<Mutex<VecDeque<String>>>,
    /// The child's whole process tree; killing only `child` would orphan node.exe behind a `cmd /c tool.cmd` shim.
    /// Dropping it (session ended, probe done, daemon shutting down) kills whatever is still running in the tree
    tree: crate::proctree::Tree,
}

impl JsonProc {
    pub fn spawn(program: &Path, args: &[String], cwd: &Path) -> std::io::Result<JsonProc> {
        let mut cmd: Command = std_command(program, args).into();
        cmd.current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(windows)]
        {
            // CREATE_NO_WINDOW, so no console window pops up in service mode
            cmd.creation_flags(0x0800_0000);
        }
        crate::proctree::prepare(&mut cmd);
        let mut child = cmd.spawn()?;
        let tree = crate::proctree::Tree::attach(&child);
        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout")).lines();
        let stderr = child.stderr.take().expect("stderr");
        let tail = Arc::new(Mutex::new(VecDeque::new()));
        let t2 = tail.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(l)) = lines.next_line().await {
                let mut t = t2.lock().unwrap();
                t.push_back(l);
                if t.len() > 30 {
                    t.pop_front();
                }
            }
        });
        Ok(JsonProc {
            child,
            stdin,
            stdout,
            stderr_tail: tail,
            tree,
        })
    }

    pub async fn send(&mut self, v: &serde_json::Value) -> std::io::Result<()> {
        let mut s = serde_json::to_string(v)?;
        s.push('\n');
        self.stdin.write_all(s.as_bytes()).await?;
        self.stdin.flush().await
    }

    /// Wait for the process to exit; returns an error message for the user (None on a normal exit)
    pub async fn finish(mut self, killed: bool) -> Option<String> {
        if killed {
            self.tree.kill();
            let _ = self.child.start_kill();
        }
        drop(self.stdin);
        let status =
            match tokio::time::timeout(std::time::Duration::from_secs(5), self.child.wait()).await
            {
                Ok(Ok(s)) => Some(s),
                _ => {
                    self.tree.kill();
                    let _ = self.child.start_kill();
                    None
                }
            };
        if killed {
            return None;
        }
        match status {
            Some(s) if s.success() => None,
            Some(s) => {
                let tail = self.stderr_tail.lock().unwrap();
                let tail: Vec<String> = tail.iter().cloned().collect();
                Some(format!(
                    "进程退出（{s}）{}",
                    if tail.is_empty() {
                        String::new()
                    } else {
                        format!("\n{}", tail.join("\n"))
                    }
                ))
            }
            None => None,
        }
    }
}

/// Compress a tool call's input into a one-line title
pub fn summarize_input(name: &str, input: &serde_json::Value) -> String {
    let s = |k: &str| input.get(k).and_then(|v| v.as_str()).map(str::to_string);
    let t = match name {
        "Bash" | "shell" | "PowerShell" => s("command"),
        "Read" | "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => s("file_path").or(s("notebook_path")),
        "Grep" | "Glob" => s("pattern"),
        "WebFetch" => s("url"),
        "WebSearch" => s("query"),
        "Task" | "Agent" => s("description"),
        "TodoWrite" => Some("更新任务列表".into()),
        _ => None,
    };
    let t = t.unwrap_or_else(|| {
        let compact = input.to_string();
        if compact == "{}" || compact == "null" {
            String::new()
        } else {
            compact
        }
    });
    crate::util::truncate(&t.replace('\n', " "), 200)
}

/// The two buttons of a question request: kind isn't allow_* / reject_*, so phone notifications don't show "Allow / Deny"
pub fn question_options() -> Vec<yi_proto::PermissionOption> {
    vec![
        yi_proto::PermissionOption {
            id: yi_proto::QUESTION_ANSWER.into(),
            label: "提交".into(),
            kind: "answer".into(),
        },
        yi_proto::PermissionOption {
            id: yi_proto::QUESTION_SKIP.into(),
            label: "跳过".into(),
            kind: "skip".into(),
        },
    ]
}

/// Write one question's answer as text: the selected option names plus any free text
pub fn answer_text(q: &yi_proto::Question, answers: &[QuestionAnswer]) -> String {
    let Some(a) = answers.iter().find(|a| a.question_id == q.id) else {
        return String::new();
    };
    let mut parts: Vec<String> = a
        .selected
        .iter()
        .map(|id| q.options.iter().find(|o| &o.id == id).map(|o| o.label.clone()).unwrap_or_else(|| id.clone()))
        .collect();
    if !a.text.trim().is_empty() {
        parts.push(a.text.trim().to_string());
    }
    parts.join(", ")
}
