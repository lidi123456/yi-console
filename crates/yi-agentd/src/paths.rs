//! yi-agentd 的所有本地文件都放在 ~/.yi-console 下。

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

pub fn log_path() -> PathBuf {
    base_dir().join("yi-agentd.log")
}

#[cfg(unix)]
pub fn socket_path() -> PathBuf {
    let p = base_dir().join("yi-agentd.sock");
    // unix socket 路径上限约 104~108 字节；家目录很深时退到 /tmp，按数据目录哈希区分
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
    // 设了 YI_CONSOLE_HOME（测试用的隔离环境）就按数据目录哈希另起一个管道，免得连到正在用的守护进程
    if std::env::var_os("YI_CONSOLE_HOME").is_some_and(|v| !v.is_empty()) {
        let h = base_dir()
            .to_string_lossy()
            .bytes()
            .fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3));
        return format!(r"\\.\pipe\yi-console-{user}-{:08x}", h as u32);
    }
    format!(r"\\.\pipe\yi-console-{user}")
}

/// 把 `~` 开头的路径展开成绝对路径
pub fn expand(p: &str) -> PathBuf {
    if p == "~" || p.is_empty() {
        return home();
    }
    if let Some(rest) = p.strip_prefix("~/").or_else(|| p.strip_prefix("~\\")) {
        return home().join(rest);
    }
    PathBuf::from(p)
}
