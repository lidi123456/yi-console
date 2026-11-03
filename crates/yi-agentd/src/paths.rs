//! All of yi-agentd's local files live under ~/.yi-console.

use std::path::PathBuf;

pub fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

pub fn base_dir() -> PathBuf {
    if let Ok(p) = std::env::var("YI_CONSOLE_HOME") {
        return PathBuf::from(p);
    }
    home().join(".yi-console")
}

pub fn ensure_base() -> std::io::Result<PathBuf> {
    let d = base_dir();
    std::fs::create_dir_all(&d)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700));
    }
    Ok(d)
}

pub fn db_path() -> PathBuf {
    base_dir().join("yi-agentd.db")
}

pub fn settings_path() -> PathBuf {
    base_dir().join("settings.json")
}

/// Single-instance lock for the daemon (Unix; on Windows the named pipe itself is the lock)
#[cfg(unix)]
pub fn lock_path() -> PathBuf {
    base_dir().join("yi-agentd.lock")
}

pub fn log_path() -> PathBuf {
    base_dir().join("yi-agentd.log")
}

#[cfg(unix)]
pub fn socket_path() -> PathBuf {
    let p = base_dir().join("yi-agentd.sock");
    // Unix socket paths are limited to about 104–108 bytes; when the home directory is deep, fall back to /tmp, distinguished by a hash of the data directory
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
    // With YI_CONSOLE_HOME set (isolated test environment), use a separate pipe keyed by a hash of the data directory, so we don't connect to the daemon in real use
    if std::env::var_os("YI_CONSOLE_HOME").is_some_and(|v| !v.is_empty()) {
        let h = base_dir()
            .to_string_lossy()
            .bytes()
            .fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3));
        return format!(r"\\.\pipe\yi-console-{user}-{:08x}", h as u32);
    }
    format!(r"\\.\pipe\yi-console-{user}")
}

/// Expand paths starting with `~` to absolute paths
pub fn expand(p: &str) -> PathBuf {
    if p == "~" || p.is_empty() {
        return home();
    }
    if let Some(rest) = p.strip_prefix("~/").or_else(|| p.strip_prefix("~\\")) {
        return home().join(rest);
    }
    PathBuf::from(p)
}
