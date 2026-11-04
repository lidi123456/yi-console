//! Terminal-mode sessions run inside tmux: the processes survive daemon restarts and upgrades, and pick up again on reattach.
//! Only enabled on Unix-likes with tmux installed; uses a separate socket (-L yi-console) so the user's own tmux is unaffected.

use std::path::PathBuf;
use std::process::Command;

const SOCKET: &str = "yi-console";

/// Requires 3.2+ (new-session -e); the result is cached, so a newly installed tmux needs a daemon restart to be detected
pub fn program() -> Option<PathBuf> {
    static P: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    P.get_or_init(|| {
        if cfg!(windows) {
            return None;
        }
        let t = crate::tools::which("tmux")?;
        let out = Command::new(&t).arg("-V").output().ok()?;
        let v = String::from_utf8_lossy(&out.stdout).to_string();
        version_ok(&v).then_some(t)
    })
    .clone()
}

/// "tmux 3.3a" / "tmux next-3.4" / "tmux 2.9"
fn version_ok(v: &str) -> bool {
    let num: String = v
        .trim()
        .rsplit(|c: char| c == ' ' || c == '-')
        .next()
        .unwrap_or("")
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let mut it = num.split('.').map(|x| x.parse::<u32>().unwrap_or(0));
    let (major, minor) = (it.next().unwrap_or(0), it.next().unwrap_or(0));
    (major, minor) >= (3, 2)
}

pub fn session_name(sid: &str) -> String {
    format!("ah-{}", sid.chars().filter(|c| c.is_ascii_alphanumeric()).take(12).collect::<String>())
}

/// Config file: no status bar, no mouse capture (selection, copy and the wheel all go to xterm.js),
/// and the outer alternate screen disabled, so lines scrolled off screen go into xterm.js's scrollback and can be scrolled back directly
fn conf() -> PathBuf {
    let p = crate::paths::base_dir().join("tmux.conf");
    let text = "\
set -g status off
set -g mouse off
set -g history-limit 50000
set -sg escape-time 0
set -g default-terminal xterm-256color
set -ga terminal-overrides ',xterm-256color:Tc,*:smcup@:rmcup@'
set -g remain-on-exit off
setw -g aggressive-resize on
set -g prefix None
unbind C-b
";
    if std::fs::read_to_string(&p).ok().as_deref() != Some(text) {
        let _ = std::fs::write(&p, text);
    }
    p
}

/// Command line to start or attach to a session: tmux … new-session -A -s name -c dir -- program args…
pub fn wrap(tmux: &PathBuf, name: &str, cwd: &str, program: &str, args: &[String], cols: u16, rows: u16) -> (PathBuf, Vec<String>) {
    let mut a: Vec<String> = vec![
        "-L".into(),
        SOCKET.into(),
        "-f".into(),
        conf().to_string_lossy().into_owned(),
        "new-session".into(),
        "-A".into(),
        "-s".into(),
        name.into(),
        "-x".into(),
        cols.to_string(),
        "-y".into(),
        rows.to_string(),
        "-c".into(),
        cwd.into(),
        "-e".into(),
        "COLORTERM=truecolor".into(),
        program.into(),
    ];
    a.extend(args.iter().cloned());
    (tmux.clone(), a)
}

pub fn alive(name: &str) -> bool {
    let Some(t) = program() else { return false };
    Command::new(t)
        .args(["-L", SOCKET, "has-session", "-t", &format!("={name}")])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Scroll tmux's own history (when output is fast tmux redraws whole screens, so scrolled-off lines never reach xterm.js's scrollback).
/// lines > 0 scrolls up, < 0 down; reaching the bottom exits copy mode automatically; cancel goes straight back to the bottom.
/// Returns (lines from the bottom, total history lines); None if the session isn't in tmux
pub fn scroll(name: &str, lines: i64, cancel: bool) -> Option<(u64, u64)> {
    let t = program()?;
    let target = format!("={name}:");
    let mut args: Vec<String> = vec!["-L".into(), SOCKET.into()];
    let mut push = |cmd: &[&str]| {
        if args.len() > 2 {
            args.push(";".into());
        }
        args.extend(cmd.iter().map(|s| s.to_string()));
    };
    if cancel {
        push(&["if-shell", "-F", "-t", &target, "#{pane_in_mode}", &format!("send-keys -t {target} -X cancel")]);
    } else if lines > 0 {
        // -e: exit copy mode automatically when scrolled back to the bottom
        push(&["copy-mode", "-e", "-t", &target]);
        push(&["send-keys", "-t", &target, "-X", "-N", &lines.to_string(), "scroll-up"]);
    } else if lines < 0 {
        push(&["if-shell", "-F", "-t", &target, "#{pane_in_mode}",
               &format!("send-keys -t {target} -X -N {} scroll-down", -lines)]);
    }
    push(&["display-message", "-p", "-t", &target, "#{pane_in_mode} #{scroll_position} #{history_size}"]);
    let out = Command::new(t).args(&args).stdin(std::process::Stdio::null()).output().ok()?;
    if !out.status.success() {
        return None;
    }
    parse_scroll(&String::from_utf8_lossy(&out.stdout))
}

/// "1 120 5000" -> (120, 5000); outside copy mode scroll_position is empty or meaningless, treated as 0
fn parse_scroll(s: &str) -> Option<(u64, u64)> {
    let line = s.lines().last()?.trim().to_string();
    let parts: Vec<&str> = line.split_whitespace().collect();
    let (in_mode, pos, hist) = match parts.as_slice() {
        [m, p, h] => (*m == "1", p.parse().unwrap_or(0), h.parse().ok()?),
        [m, h] => (*m == "1", 0, h.parse().ok()?),
        _ => return None,
    };
    Some((if in_mode { pos } else { 0 }, hist))
}

/// PID of the program running directly in the pane
pub(crate) fn pane_pid(name: &str) -> Option<i32> {
    let out = Command::new(program()?)
        .args(["-L", SOCKET, "display-message", "-p", "-t", &format!("={name}:"), "#{pane_pid}"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok().filter(|p| *p > 0)
}

/// End the tmux session and wait for the program inside to actually exit. kill-session only sends SIGHUP and the program takes a while to wrap up,
/// during which it still holds the tool session (Codex refuses to let another process continue it), so we wait.
/// If it hasn't exited after 3 seconds, kill it along with its children. Returns false if the session doesn't exist
pub async fn kill_and_wait(name: &str) -> bool {
    let pid = pane_pid(name);
    kill(name);
    #[cfg(unix)]
    if let Some(pid) = pid {
        let alive = || unsafe { libc::kill(pid, 0) == 0 };
        for i in 0..40 {
            if !alive() {
                break;
            }
            if i == 30 {
                // The program in the pane is the process group leader; kill by group to get children started by npm wrapper scripts
                unsafe { libc::kill(-pid, libc::SIGKILL) };
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }
    pid.is_some()
}

pub fn kill(name: &str) {
    if let Some(t) = program() {
        let _ = Command::new(t)
            .args(["-L", SOCKET, "kill-session", "-t", &format!("={name}")])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn scroll_output() {
        assert_eq!(super::parse_scroll("1 120 5000\n"), Some((120, 5000)));
        assert_eq!(super::parse_scroll("0  300\n"), Some((0, 300)));
        assert_eq!(super::parse_scroll("0 7 300"), Some((0, 300)));
        assert_eq!(super::parse_scroll(""), None);
    }

    #[test]
    fn name() {
        assert_eq!(super::session_name("e44942e3-5dbd-4cd4-89c3"), "ah-e44942e35dbd");
        assert!(super::version_ok("tmux 3.3a"));
        assert!(super::version_ok("tmux next-3.4"));
        assert!(!super::version_ok("tmux 2.9"));
        assert!(!super::version_ok("tmux 3.1c"));
    }
}
