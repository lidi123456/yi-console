//! 各 AI coding 工具的适配器。每个适配器是一个 tokio 任务，独占工具子进程，
//! 从 `rx` 收命令，把工具输出翻译成统一事件交给 `Emitter`。

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

/// 对话模式下告诉模型怎么给用户看图片：Yi Console 会把回复里引用主机本地文件的 Markdown 图片显示出来。
/// Claude 用 `--append-system-prompt`，Codex 用 `developerInstructions`；ACP 没有系统提示词入口，不加
pub const IMAGE_HINT: &str = "Your replies are shown in the Yi Console app, which renders Markdown. \
To show the user an image file on this machine (a screenshot, chart or generated picture), embed it with \
Markdown image syntax and its absolute path, e.g. ![chart](/abs/path/chart.png); wrap a path containing \
spaces in angle brackets: ![chart](</abs/path/my chart.png>).";

/// 一条用户输入
#[derive(Debug, Clone)]
pub struct Prompt {
    pub text: String,
    pub images: Vec<PromptImage>,
}

#[derive(Debug, Clone)]
pub struct PromptImage {
    pub mime_type: String,
    /// 已存到 yi-agentd 数据目录的文件（Codex 按路径读）
    pub path: PathBuf,
    /// base64（Claude、ACP 直接内联）
    pub data: String,
}

#[derive(Debug)]
pub enum AdapterCmd {
    Send(Prompt),
    /// 切换模型，下一轮生效
    SetModel(String),
    /// 思考等级，空串表示恢复工具默认
    SetEffort(String),
    /// 切换审批模式（default / acceptEdits / plan / bypass）
    SetPermissionMode(String),
    Interrupt,
    /// 停止一个后台任务
    StopTask(String),
    /// 回答审批；提问类请求 option_id 是 answer / skip，回答内容在 answers 里
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
    /// 工具侧会话 id，有值时走 resume
    pub resume: Option<String>,
    /// resume 时工具回放的历史是否要记进事件日志（导入历史时为 true）
    pub replay: bool,
    /// replay 时先跳过这么多条用户消息（连同回复）再开始记：切回对话模式时只记在终端里新聊的
    pub replay_skip: usize,
    pub model: Option<String>,
    pub effort: Option<String>,
    /// default / acceptEdits / bypass / plan
    pub permission_mode: String,
    pub command: Option<String>,
    pub program: PathBuf,
    pub extra_args: Vec<String>,
    pub cols: u16,
    pub rows: u16,
    /// 终端模式放进这个 tmux 会话里跑（守护进程重启后还能接上）
    pub tmux: Option<String>,
    /// 终端模式新开 Claude 会话时预先指定的会话 id（--session-id），之后重启据此 --resume
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
    }
}

/// Windows 上 npm 装的工具是 .cmd 脚本，不能直接 CreateProcess，要经 cmd /c
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

/// 以按行 JSON 方式通信的子进程
pub struct JsonProc {
    pub child: Child,
    pub stdin: ChildStdin,
    pub stdout: tokio::io::Lines<BufReader<ChildStdout>>,
    pub stderr_tail: Arc<Mutex<VecDeque<String>>>,
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
            // CREATE_NO_WINDOW，避免服务模式下弹出控制台窗口
            cmd.creation_flags(0x0800_0000);
        }
        let mut child = cmd.spawn()?;
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
        })
    }

    pub async fn send(&mut self, v: &serde_json::Value) -> std::io::Result<()> {
        let mut s = serde_json::to_string(v)?;
        s.push('\n');
        self.stdin.write_all(s.as_bytes()).await?;
        self.stdin.flush().await
    }

    /// 等待进程结束，返回给用户看的错误信息（正常退出返回 None）
    pub async fn finish(mut self, killed: bool) -> Option<String> {
        if killed {
            let _ = self.child.start_kill();
        }
        drop(self.stdin);
        let status =
            match tokio::time::timeout(std::time::Duration::from_secs(5), self.child.wait()).await
            {
                Ok(Ok(s)) => Some(s),
                _ => {
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

/// 把工具调用的输入压缩成一行标题
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

/// 提问类请求的两个按钮：kind 不用 allow_* / reject_*，手机通知上就不会出"允许 / 拒绝"
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

/// 一道题的回答写成一段文字：选中的选项名，加上自己填的内容
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
