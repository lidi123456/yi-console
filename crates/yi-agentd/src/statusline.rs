//! Parts of the session status line computed on the host: git branch and change count, custom command output.
//!
//! Custom commands follow the same convention as Claude Code's `statusLine`: they receive JSON on stdin,
//! and the first line of output is shown (ANSI colors allowed). So scripts written for Claude Code work as is;
//! without a configured command, Claude sessions reuse the statusLine from `~/.claude/settings.json`; a project's `.claude/settings*.json`
//! is only used if Claude Code has trusted the directory (config in a repo may have been written by someone else and mustn't run just by opening it).
//! Our own fields go under `yi_console`.

use crate::hub::Hub;
use anyhow::Result;
use yi_proto::{SessionInfo, ToolKind};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;

/// When several clients view the same session, reuse the last result within 2 seconds
const CACHE_TTL: Duration = Duration::from_secs(2);
const CMD_TIMEOUT: Duration = Duration::from_secs(3);
const GIT_TIMEOUT: Duration = Duration::from_secs(2);

static CACHE: Mutex<Option<HashMap<String, (Instant, Value)>>> = Mutex::new(None);
/// Max status line commands running at once, so many sessions refreshing together don't bog down the machine
static SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);

pub async fn query(hub: &Arc<Hub>, sid: &str, want_git: bool) -> Result<Value> {
    let key = format!("{sid}:{want_git}");
    if let Some((t, v)) = CACHE.lock().unwrap().get_or_insert_with(HashMap::new).get(&key) {
        if t.elapsed() < CACHE_TTL {
            return Ok(v.clone());
        }
    }
    let info = hub.info(sid)?;
    let st = hub.settings.read().unwrap().status_line.clone();
    let git = if want_git { git_info(&info.cwd).await } else { None };

    let mut out = json!({});
    if let Some(g) = &git {
        out["git"] = g.clone();
    }
    let cmd = if !st.command.trim().is_empty() {
        Some((st.command.trim().to_string(), "settings", false))
    } else if st.use_claude && info.tool == ToolKind::Claude {
        claude_command(&info.cwd).map(|c| (c, "claude", true))
    } else {
        None
    };
    if let Some((cmd, source, bash)) = cmd {
        let payload = payload(&info, git.as_ref());
        match run(&cmd, &info.cwd, &payload, bash).await {
            Ok(text) => out["command"] = json!({ "text": text, "source": source }),
            Err(e) => out["error"] = json!(format!("状态栏命令失败：{e}")),
        }
    }
    CACHE
        .lock()
        .unwrap()
        .get_or_insert_with(HashMap::new)
        .insert(key, (Instant::now(), out.clone()));
    Ok(out)
}

/// One `git status --porcelain -b` gives the branch, ahead/behind and changed file count
async fn git_info(cwd: &str) -> Option<Value> {
    if cwd.is_empty() || !Path::new(cwd).is_dir() {
        return None;
    }
    let mut cmd = tokio::process::Command::new("git");
    cmd.args(["-C", cwd, "status", "--porcelain=v1", "-b", "--untracked-files=normal"])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000);
    let o = tokio::time::timeout(GIT_TIMEOUT, cmd.output()).await.ok()?.ok()?;
    if !o.status.success() {
        return None;
    }
    parse_git(&String::from_utf8_lossy(&o.stdout))
}

fn parse_git(s: &str) -> Option<Value> {
    let mut lines = s.lines();
    let head = lines.next()?.strip_prefix("## ")?;
    let changed = lines.filter(|l| !l.trim().is_empty()).count();
    // "main...origin/main [ahead 1, behind 2]", "No commits yet on main", "HEAD (no branch)"
    let (name, rest) = head.split_once(' ').unwrap_or((head, ""));
    let branch = if let Some(b) = head.strip_prefix("No commits yet on ") {
        b.to_string()
    } else if name == "HEAD" {
        "HEAD（游离）".to_string()
    } else {
        name.split("...").next().unwrap_or(name).to_string()
    };
    let num = |k: &str| {
        rest.split(|c| c == '[' || c == ']' || c == ',')
            .find_map(|p| p.trim().strip_prefix(k)?.trim().parse::<u64>().ok())
            .unwrap_or(0)
    };
    Some(json!({ "branch": branch, "ahead": num("ahead"), "behind": num("behind"), "changed": changed }))
}

/// Find the statusLine command with Claude Code's precedence: project local > project > user.
/// Project config comes with the repo and may be written by someone else: like Claude Code, it's only used if this directory (or a parent)
/// was marked "trusted" in Claude Code; otherwise only the user's own config is considered
fn claude_command(cwd: &str) -> Option<String> {
    let user = claude_config_dir();
    let proj = Path::new(cwd).join(".claude");
    let mut files = vec![];
    if claude_trusted(cwd) {
        files.push(proj.join("settings.local.json"));
        files.push(proj.join("settings.json"));
    }
    files.push(user.join("settings.json"));
    files.iter().find_map(|p| {
        let v: Value = serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()?;
        let c = v["statusLine"]["command"].as_str()?.trim().to_string();
        (!c.is_empty()).then_some(c)
    })
}

fn claude_config_dir() -> PathBuf {
    std::env::var("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| crate::paths::home().join(".claude"))
}

/// Claude Code records trusted directories in `~/.claude.json` (in that directory if CLAUDE_CONFIG_DIR is set) under
/// `projects.<path>.hasTrustDialogAccepted`; trusting a parent directory counts too
fn claude_trusted(cwd: &str) -> bool {
    let file = match std::env::var("CLAUDE_CONFIG_DIR") {
        Ok(d) => PathBuf::from(d).join(".claude.json"),
        Err(_) => crate::paths::home().join(".claude.json"),
    };
    let Some(v) = std::fs::read_to_string(file).ok().and_then(|s| serde_json::from_str::<Value>(&s).ok()) else {
        return false;
    };
    trusted_in(&v, cwd)
}

fn trusted_in(cfg: &Value, cwd: &str) -> bool {
    let Some(projects) = cfg["projects"].as_object() else { return false };
    let norm = |p: &str| {
        let p = p.replace('\\', "/");
        let p = p.trim_end_matches('/');
        if cfg!(windows) { p.to_lowercase() } else { p.to_string() }
    };
    let trusted: Vec<String> = projects
        .iter()
        .filter(|(_, v)| v["hasTrustDialogAccepted"].as_bool() == Some(true))
        .map(|(k, _)| norm(k))
        .filter(|k| !k.is_empty())
        .collect();
    let mut dir = Some(Path::new(cwd));
    while let Some(d) = dir {
        let n = norm(&d.to_string_lossy());
        if !n.is_empty() && trusted.contains(&n) {
            return true;
        }
        dir = d.parent();
    }
    false
}

fn claude_transcript(info: &SessionInfo) -> Option<PathBuf> {
    let id = info.external_id.as_deref()?;
    let name = format!("{id}.jsonl");
    std::fs::read_dir(crate::history::claude_root())
        .ok()?
        .flatten()
        .map(|d| d.path().join(&name))
        .find(|p| p.is_file())
}

/// Field names follow Claude Code's statusLine input format; our extras go under yi_console
fn payload(info: &SessionInfo, git: Option<&Value>) -> Value {
    let u = &info.usage;
    let model = info.model.clone().unwrap_or_default();
    let transcript = if info.tool == ToolKind::Claude {
        claude_transcript(info).map(|p| p.to_string_lossy().to_string())
    } else {
        None
    };
    json!({
        "hook_event_name": "Status",
        "session_id": info.external_id.clone().unwrap_or_else(|| info.id.clone()),
        "transcript_path": transcript,
        "cwd": info.cwd,
        "model": { "id": model, "display_name": model },
        "workspace": { "current_dir": info.cwd, "project_dir": info.cwd },
        "version": env!("CARGO_PKG_VERSION"),
        "output_style": { "name": "default" },
        "cost": {
            "total_cost_usd": u.cost_usd,
            "total_duration_ms": (crate::util::now_ms() - info.created_at).max(0),
        },
        "context_window": {
            "used_percentage": info.context_percent,
            "total_input_tokens": u.input_tokens + u.cache_read_tokens + u.cache_write_tokens,
            "total_output_tokens": u.output_tokens,
        },
        "yi_console": {
            "session_id": info.id,
            "tool": info.tool,
            "title": info.title,
            "status": info.status,
            "context_percent": info.context_percent,
            "usage": u,
            "git": git,
        },
    })
}

/// On Windows, Claude's statusLine command runs in Git Bash as Claude Code does; our own commands use cmd
fn shell(cmd: &str, bash: bool) -> tokio::process::Command {
    if cfg!(windows) {
        if let Some(b) = bash.then(git_bash).flatten() {
            let mut c = tokio::process::Command::new(b);
            c.args(["-c", cmd]);
            return c;
        }
        #[allow(unused_mut)]
        let mut c = tokio::process::Command::new("cmd");
        // Pass it to cmd as is, without Rust adding backslashes to quotes per C runtime rules (cmd doesn't understand them)
        #[cfg(windows)]
        c.raw_arg(format!("/D /S /C \"{cmd}\""));
        c
    } else {
        let mut c = tokio::process::Command::new("sh");
        c.args(["-c", cmd]);
        c
    }
}

/// The bash that ships with Git for Windows (not the WSL bash in System32)
fn git_bash() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("CLAUDE_CODE_GIT_BASH_PATH") {
        return Some(PathBuf::from(p));
    }
    let git = crate::tools::which("git")?;
    // ...\Git\cmd\git.exe → ...\Git\bin\bash.exe
    let b = git.parent()?.parent()?.join("bin").join("bash.exe");
    b.is_file().then_some(b)
}

async fn run(cmd: &str, cwd: &str, payload: &Value, bash: bool) -> Result<String> {
    let _slot = SLOTS.acquire().await?;
    let mut c = shell(cmd, bash);
    if Path::new(cwd).is_dir() {
        c.current_dir(cwd);
    }
    c.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    c.creation_flags(0x0800_0000);
    let mut child = c.spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        let data = serde_json::to_vec(payload)?;
        // Fine if the command doesn't read stdin; ignore write failures
        let _ = stdin.write_all(&data).await;
    }
    let o = tokio::time::timeout(CMD_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| anyhow::anyhow!("超过 {} 秒没有输出", CMD_TIMEOUT.as_secs()))??;
    let stdout = String::from_utf8_lossy(&o.stdout);
    let line = stdout.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim_end();
    if line.is_empty() && !o.status.success() {
        let err = String::from_utf8_lossy(&o.stderr);
        anyhow::bail!("{}", crate::util::truncate(err.lines().next().unwrap_or("退出码非 0"), 200));
    }
    Ok(crate::util::truncate(line, 400))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_status_head() {
        let v = parse_git("## main...origin/main [ahead 2, behind 1]\n M a.rs\n?? b.rs\n").unwrap();
        assert_eq!(v["branch"], "main");
        assert_eq!(v["ahead"], 2);
        assert_eq!(v["behind"], 1);
        assert_eq!(v["changed"], 2);
        let v = parse_git("## No commits yet on dev\n").unwrap();
        assert_eq!(v["branch"], "dev");
        assert_eq!(v["changed"], 0);
        let v = parse_git("## feature/x\n").unwrap();
        assert_eq!(v["branch"], "feature/x");
    }

    #[test]
    fn project_trust() {
        let cfg = json!({ "projects": {
            "/data/git/ok": { "hasTrustDialogAccepted": true },
            "/data/git/no": { "hasTrustDialogAccepted": false },
            "/srv/": { "hasTrustDialogAccepted": true },
        }});
        assert!(trusted_in(&cfg, "/data/git/ok"));
        assert!(trusted_in(&cfg, "/data/git/ok/sub/dir"));
        assert!(trusted_in(&cfg, "/srv/app"));
        assert!(!trusted_in(&cfg, "/data/git/no"));
        assert!(!trusted_in(&cfg, "/data/git/okay"));
        assert!(!trusted_in(&cfg, "/tmp/evil"));
        assert!(!trusted_in(&json!({}), "/data/git/ok"));
    }
}
