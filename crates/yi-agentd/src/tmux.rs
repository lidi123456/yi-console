//! 终端模式的会话放在 tmux 里跑：守护进程重启、升级后进程还在，重新接上就能继续。
//! 只在类 Unix 上、装了 tmux 时启用；用单独的 socket（-L yi-console），不影响用户自己的 tmux。

use std::path::PathBuf;
use std::process::Command;

const SOCKET: &str = "yi-console";

/// 要 3.2 以上（new-session -e）；结果缓存，装了新 tmux 要重启守护进程才认
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

/// 配置文件：不显示状态栏、不抢鼠标（选中复制、滚轮都交给 xterm.js），
/// 关掉外层的备用屏幕，滚出屏幕的行进 xterm.js 的回滚缓冲，能直接往上翻
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

/// 启动或接上已有会话的命令行：tmux … new-session -A -s 名字 -c 目录 -- 程序 参数…
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

/// 翻 tmux 自己的历史（输出快时 tmux 会整屏重画，滚出去的行进不了 xterm.js 的回滚缓冲）。
/// lines > 0 往上翻、< 0 往下翻，翻回底部自动退出翻页；cancel 直接回到底部。
/// 返回 (离底部的行数, 历史总行数)；会话不在 tmux 里返回 None
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
        // -e：翻回底部时自动退出翻页模式
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

/// "1 120 5000" -> (120, 5000)；不在翻页模式时 scroll_position 为空或无意义，按 0 算
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

/// 窗格里直接跑的程序的进程号
fn pane_pid(name: &str) -> Option<i32> {
    let out = Command::new(program()?)
        .args(["-L", SOCKET, "display-message", "-p", "-t", &format!("={name}:"), "#{pane_pid}"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok().filter(|p| *p > 0)
}

/// 结束 tmux 会话，并等里面的程序真正退出。kill-session 只发 SIGHUP，程序收尾要一会儿，
/// 这期间工具会话还被它占着（Codex 会拒绝另一个进程接续），所以要等。
/// 3 秒还没退就连同它的子进程一起强杀。会话不存在返回 false
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
                // 窗格里的程序是进程组组长，按组杀掉 npm 包装脚本起的子进程
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
