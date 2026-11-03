//! Detect which AI coding tools are installed on this machine, and fix up the daemon's PATH.

use yi_proto::{DaemonSettings, ToolInfo, ToolKind};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Default executable names for each tool (tried in order)
fn binary_names(kind: ToolKind) -> &'static [&'static str] {
    match kind {
        ToolKind::Claude => &["claude"],
        ToolKind::Codex => &["codex"],
        ToolKind::Cursor => &["cursor-agent", "agent"],
        ToolKind::Kiro => &["kiro-cli"],
        ToolKind::Qoder => &["qodercli"],
        ToolKind::Codebuddy => &["codebuddy", "cbc"],
        ToolKind::Shell | ToolKind::Files => &[],
    }
}

/// Processes started by systemd / launchd have a short PATH without directories like nvm or ~/.local/bin,
/// and don't get API keys exported in rc files (e.g. CURSOR_API_KEY) either.
/// At startup, fetch the environment once from a login shell: merge PATH, and only fill in other variables that are missing; then add common install directories.
pub fn fix_path() {
    #[cfg(unix)]
    let login = login_shell_env();
    #[cfg(unix)]
    for (k, v) in &login {
        if k != "PATH" && !SKIP_ENV.contains(&k.as_str()) && std::env::var_os(k).is_none() {
            std::env::set_var(k, v);
        }
    }

    let mut parts: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();

    if let Ok(extra) = std::env::var("YI_CONSOLE_PATH") {
        for p in std::env::split_paths(&extra) {
            push_unique(&mut parts, p);
        }
    }

    #[cfg(unix)]
    if let Some((_, login)) = login.iter().find(|(k, _)| k == "PATH") {
        for p in std::env::split_paths(login) {
            push_unique(&mut parts, p);
        }
    }

    let home = crate::paths::home();
    let common: Vec<PathBuf> = if cfg!(windows) {
        let local = std::env::var("LOCALAPPDATA").unwrap_or_default();
        let appdata = std::env::var("APPDATA").unwrap_or_default();
        vec![
            home.join(".local").join("bin"),
            PathBuf::from(&appdata).join("npm"),
            PathBuf::from(&local).join("Kiro-Cli"),
            PathBuf::from(&local).join("cursor-agent"),
            // Qoder's install script puts it under ~/.qoder/bin/qodercli/, not on PATH
            home.join(".qoder").join("bin").join("qodercli"),
        ]
    } else {
        vec![
            home.join(".local/bin"),
            home.join(".npm-global/bin"),
            home.join(".bun/bin"),
            home.join(".cargo/bin"),
            home.join(".claude/local"),
            home.join(".qoder/bin/qodercli"),
            PathBuf::from("/usr/local/bin"),
            PathBuf::from("/opt/homebrew/bin"),
        ]
    };
    for p in common {
        if p.is_dir() {
            push_unique(&mut parts, p);
        }
    }
    if let Ok(joined) = std::env::join_paths(parts) {
        std::env::set_var("PATH", joined);
    }
}

fn push_unique(v: &mut Vec<PathBuf>, p: PathBuf) {
    if !p.as_os_str().is_empty() && !v.contains(&p) {
        v.push(p);
    }
}

/// Variables that belong only to that shell process and shouldn't be carried into the daemon
#[cfg(unix)]
const SKIP_ENV: &[&str] = &["PWD", "OLDPWD", "SHLVL", "_", "PS1", "PS2", "TERM", "COLUMNS", "LINES", "SHELLOPTS", "BASHOPTS"];

#[cfg(unix)]
fn login_shell_env() -> Vec<(String, String)> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into());
    // -i reads .bashrc (where nvm and API keys usually live); wrap the output in markers so anything printed by rc files doesn't get mixed in
    let Some(out) = run_with_timeout(
        std::process::Command::new(&shell)
            .args(["-lic", "printf '__AH__'; env -0; printf '__AH__'"])
            .stdin(std::process::Stdio::null()),
        Duration::from_secs(5),
    ) else {
        return vec![];
    };
    parse_env_block(&String::from_utf8_lossy(&out))
}

#[cfg(unix)]
fn parse_env_block(s: &str) -> Vec<(String, String)> {
    let Some(start) = s.find("__AH__").map(|i| i + 6) else {
        return vec![];
    };
    let Some(end) = s[start..].rfind("__AH__").map(|i| i + start) else {
        return vec![];
    };
    s[start..end]
        .split('\0')
        .filter_map(|kv| kv.split_once('='))
        .filter(|(k, _)| !k.is_empty() && !k.contains(char::is_whitespace))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn run_with_timeout(cmd: &mut std::process::Command, t: Duration) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut child = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() > t => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => return None,
        }
    }
    let mut buf = vec![];
    child.stdout.take()?.read_to_end(&mut buf).ok()?;
    Some(buf)
}

/// Find an executable on PATH
pub fn which(name: &str) -> Option<PathBuf> {
    let p = Path::new(name);
    if p.is_absolute() {
        return p.is_file().then(|| p.to_path_buf());
    }
    let path = std::env::var_os("PATH")?;
    let exts: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".EXE;.CMD;.BAT;.COM".into())
            .split(';')
            .map(|s| s.to_ascii_lowercase())
            .chain(std::iter::once(String::new()))
            .collect()
    } else {
        vec![String::new()]
    };
    for dir in std::env::split_paths(&path) {
        for ext in &exts {
            let cand = dir.join(format!("{name}{ext}"));
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    None
}

/// Resolve the program a tool should run: the override in settings first, then PATH
pub fn resolve(kind: ToolKind, settings: &DaemonSettings) -> Option<PathBuf> {
    if let Some(o) = settings.tools.get(kind.as_str()) {
        if !o.command.is_empty() {
            return which(&o.command).or_else(|| Some(PathBuf::from(&o.command)));
        }
    }
    if kind == ToolKind::Shell {
        return Some(PathBuf::from(default_shell()));
    }
    // Files sessions run nothing; report a placeholder so the tool shows as available
    if kind == ToolKind::Files {
        return Some(PathBuf::new());
    }
    binary_names(kind).iter().find_map(|n| which(n))
}

pub fn default_shell() -> String {
    if cfg!(windows) {
        if which("pwsh").is_some() {
            "pwsh".into()
        } else {
            "powershell".into()
        }
    } else {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into())
    }
}

pub fn extra_args(kind: ToolKind, settings: &DaemonSettings) -> Vec<String> {
    settings
        .tools
        .get(kind.as_str())
        .map(|o| o.extra_args.clone())
        .unwrap_or_default()
}

pub fn detect(settings: &DaemonSettings) -> Vec<ToolInfo> {
    ToolKind::ALL
        .into_iter()
        .map(|kind| {
            let path = resolve(kind, settings);
            let version = match (&path, kind) {
                (Some(p), k) if k != ToolKind::Shell && k.has_process() => version_of(p),
                _ => None,
            };
            ToolInfo {
                kind,
                available: path.is_some(),
                path: path.filter(|_| kind.has_process()).map(|p| p.display().to_string()),
                version,
                structured: !matches!(kind, ToolKind::Shell | ToolKind::Files),
            }
        })
        .collect()
}

/// Whether terminal-mode Codex needs --no-daemon. Newer Codex's interactive UI connects to a shared background service by default,
/// which keeps holding the session after exit, so chat mode's app-server later fails to continue it with "already has an active writer".
/// Older versions don't recognize the flag, so check --help first (cached by path)
pub fn codex_no_daemon(p: &Path) -> bool {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, bool>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(v) = cache.lock().unwrap().get(p) {
        return *v;
    }
    let mut cmd = crate::adapters::std_command(p, &["--help".to_string()]);
    cmd.stdin(std::process::Stdio::null());
    let Some(out) = run_with_timeout(&mut cmd, Duration::from_secs(8)) else {
        return false;
    };
    let v = String::from_utf8_lossy(&out).contains("--no-daemon");
    cache.lock().unwrap().insert(p.to_path_buf(), v);
    v
}

fn version_of(p: &Path) -> Option<String> {
    let mut cmd = std::process::Command::new(p);
    cmd.arg("--version").stdin(std::process::Stdio::null());
    let out = run_with_timeout(&mut cmd, Duration::from_secs(8))?;
    let s = String::from_utf8_lossy(&out);
    s.lines().find(|l| !l.trim().is_empty()).map(|l| l.trim().to_string())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn env_block_ignores_rc_noise() {
        let s = "welcome!\n__AH__A=1\0KEY=x=y\0MULTI=a\nb\0__AH__";
        let v = parse_env_block(s);
        assert_eq!(
            v,
            vec![("A".into(), "1".into()), ("KEY".into(), "x=y".into()), ("MULTI".into(), "a\nb".into())]
        );
    }
}
