//! Direct local IPC connection to a running yi-agentd daemon (~/.yi-console/yi-agentd.sock on Unix, named pipe on Windows).

use std::path::PathBuf;
use tokio::io::{AsyncRead, AsyncWrite};

pub trait LocalStream: AsyncRead + AsyncWrite + Unpin + Send + 'static {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> LocalStream for T {}

fn home() -> PathBuf {
    #[cfg(windows)]
    if std::env::var_os("YI_CONSOLE_HOME").is_some_and(|v| !v.is_empty()) {
        if let Some(p) = std::env::var_os("USERPROFILE").filter(|v| !v.is_empty()) {
            return PathBuf::from(p);
        }
    }
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

pub fn base_dir() -> PathBuf {
    if let Ok(p) = std::env::var("YI_CONSOLE_HOME") {
        return PathBuf::from(p);
    }
    home().join(".yi-console")
}

/// Expand `~` / `~/.yi-console` paths identically to yi-agentd::paths::expand
pub fn expand_path(p: &str) -> PathBuf {
    if p == "~" || p.is_empty() {
        return home();
    }
    if p == "~/.yi-console" || p == "~\\.yi-console" {
        return base_dir();
    }
    if let Some(rest) = p.strip_prefix("~/.yi-console/").or_else(|| p.strip_prefix("~\\.yi-console\\")) {
        return base_dir().join(rest);
    }
    if let Some(rest) = p.strip_prefix("~/").or_else(|| p.strip_prefix("~\\")) {
        return home().join(rest);
    }
    PathBuf::from(p)
}

#[cfg(unix)]
pub fn socket_path() -> PathBuf {
    let p = base_dir().join("yi-agentd.sock");
    if p.as_os_str().len() < 100 {
        return p;
    }
    let uid = unsafe { libc::getuid() };
    let h = base_dir()
        .to_string_lossy()
        .bytes()
        .fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3));
    PathBuf::from(format!("/tmp/yi-console-{uid}-{:08x}.sock", h as u32))
}

#[cfg(windows)]
pub fn pipe_name() -> String {
    let user = std::env::var("USERNAME").unwrap_or_else(|_| "user".into());
    let user: String = user
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    if std::env::var_os("YI_CONSOLE_HOME").is_some_and(|v| !v.is_empty()) {
        let h = base_dir()
            .to_string_lossy()
            .bytes()
            .fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3));
        return format!(r"\\.\pipe\yi-console-{user}-{:08x}", h as u32);
    }
    format!(r"\\.\pipe\yi-console-{user}")
}

#[cfg(unix)]
pub async fn connect() -> std::io::Result<impl LocalStream> {
    tokio::net::UnixStream::connect(socket_path()).await
}

#[cfg(windows)]
pub async fn connect() -> std::io::Result<impl LocalStream> {
    use tokio::net::windows::named_pipe::ClientOptions;
    let name = pipe_name();
    for _ in 0..20 {
        match ClientOptions::new().open(&name) {
            Ok(c) => return Ok(c),
            Err(e) if e.raw_os_error() == Some(231) => {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await
            }
            Err(e) if e.raw_os_error() == Some(5) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "守护进程以另一种权限运行（例如管理员），当前进程无权连接。\
                     请先结束已有的 yi-agentd.exe，再重新连接",
                ))
            }
            Err(e) => return Err(e),
        }
    }
    ClientOptions::new().open(&name)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn socket_path_respects_unix_limit() {
        let p = socket_path();
        assert!(p.as_os_str().len() < 104, "{}", p.display());
    }
}
