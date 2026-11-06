//! macOS driver: cua-driver's own signed CuaDriver.app (bundle id com.trycua.driver) runs the daemon, so
//! Accessibility and Screen Recording are granted to Cua Driver once and survive Yi Console upgrades (our builds
//! are not signed, and an unsigned responsible process loses its grants whenever its binary changes).
//!
//! The upstream proxy would start the daemon with `open -a CuaDriver`, which needs it in /Applications. We start it
//! ourselves from our own copy with an explicit path and socket, then talk to it through `cua-driver mcp --socket`.
//! CUA_DRIVER_EMBEDDED must stay unset: in embedded mode the proxy refuses a daemon it did not start.
use super::{DRIVER_ENV, VERSION};
use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// SHA-256 of CuaDriver.app/Contents/MacOS/cua-driver in the pinned darwin-universal release
const APP_BINARY_SHA: &str = "54e5d48128fb62323366c49b6be480ccef83cdee23bd7fc1f19431347a5df86b";
/// Copied next to yi-agentd by the macOS app (`localbin::sync_extras`); only the macOS installer carries it
pub const ARCHIVE: &str = "computer-use-macos.tgz";
/// sockaddr_un.sun_path is 104 bytes on macOS, including the terminator
const MAX_SOCKET: usize = 103;

fn sha(path: &Path) -> Option<String> {
    std::fs::read(path).ok().map(|b| format!("{:x}", Sha256::digest(b)))
}

/// Whether this host has the driver at all: only the macOS app ships it, an SSH-deployed daemon has none
pub fn installed() -> bool {
    let bin = crate::paths::base_dir().join("bin");
    bin.join(ARCHIVE).is_file() || bin.join(format!("computer-use-{VERSION}/CuaDriver.app")).is_dir()
}

/// The extracted CuaDriver.app, verified by the digest of its executable (the bundle's own signature covers the rest)
pub fn app_path() -> Result<PathBuf> {
    let bin = crate::paths::base_dir().join("bin");
    let dir = bin.join(format!("computer-use-{VERSION}"));
    let app = dir.join("CuaDriver.app");
    if sha(&app.join("Contents/MacOS/cua-driver")).as_deref() == Some(APP_BINARY_SHA) {
        return Ok(app);
    }
    let archive = bin.join(ARCHIVE);
    if !archive.is_file() {
        bail!("未找到 macOS 桌面驱动，请用 macOS 安装包重新安装 Yi Console");
    }
    let temp = bin.join(format!("computer-use-{VERSION}.tmp"));
    let _ = std::fs::remove_dir_all(&temp);
    std::fs::create_dir_all(&temp)?;
    let status = std::process::Command::new("/usr/bin/tar")
        .arg("-xzf")
        .arg(&archive)
        .arg("-C")
        .arg(&temp)
        .status()
        .context("解压 macOS 桌面驱动失败")?;
    if !status.success() || sha(&temp.join("CuaDriver.app/Contents/MacOS/cua-driver")).as_deref() != Some(APP_BINARY_SHA) {
        let _ = std::fs::remove_dir_all(&temp);
        bail!("内置桌面驱动校验失败");
    }
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::rename(&temp, &dir)?;
    Ok(app)
}

pub fn executable(app: &Path) -> PathBuf {
    app.join("Contents/MacOS/cua-driver")
}

/// The per-user temporary directory (private to the user, unlike /tmp), independent of TMPDIR, which a daemon
/// started by launchd or over SSH may not have
fn user_temp_dir() -> PathBuf {
    let mut buf = vec![0 as libc::c_char; 1024];
    // Safety: the buffer length is passed; the result is NUL-terminated when it fits
    let n = unsafe { libc::confstr(libc::_CS_DARWIN_USER_TEMP_DIR, buf.as_mut_ptr(), buf.len()) };
    if n > 1 && n <= buf.len() {
        let bytes: Vec<u8> = buf[..n - 1].iter().map(|c| *c as u8).collect();
        return PathBuf::from(String::from_utf8_lossy(&bytes).into_owned());
    }
    std::env::temp_dir()
}

/// One socket per data directory, so an isolated test daemon (YI_CONSOLE_HOME) never shares the user's driver
pub fn socket_path() -> Result<PathBuf> {
    let tag = format!("{:x}", Sha256::digest(crate::paths::base_dir().to_string_lossy().as_bytes()));
    let path = user_temp_dir().join(format!("yi-cu-{}.sock", &tag[..12]));
    if path.as_os_str().len() > MAX_SOCKET {
        bail!("桌面驱动 socket 路径过长：{}", path.display());
    }
    Ok(path)
}

async fn listening(socket: &Path) -> bool {
    tokio::net::UnixStream::connect(socket).await.is_ok()
}

/// Start the CuaDriver.app daemon through LaunchServices unless one already listens (left from an earlier daemon run).
/// The daemon binds its socket before its first-run permission panel, so this returns while grants are still missing.
pub async fn start(app: &Path, socket: &Path) -> Result<()> {
    if listening(socket).await {
        return Ok(());
    }
    // `open` passes our environment on to the app, which turns off its telemetry and update checks
    let status = tokio::process::Command::new("/usr/bin/open")
        .args(["-n", "-g"])
        .arg(app)
        .args(["--args", "serve", "--socket"])
        .arg(socket)
        .envs(DRIVER_ENV.iter().copied())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .context("无法启动 macOS 桌面驱动")?;
    if !status.success() {
        bail!("无法启动 macOS 桌面驱动");
    }
    for _ in 0..100 {
        if listening(socket).await {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    bail!("桌面驱动启动超时")
}

/// Stop the daemon once no session uses the desktop (it is not our child, so dropping the proxy leaves it running)
pub async fn stop() {
    let (Ok(app), Ok(socket)) = (app_path(), socket_path()) else {
        return;
    };
    if !listening(&socket).await {
        return;
    }
    let _ = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::process::Command::new(executable(&app))
            .args(["stop", "--socket"])
            .arg(&socket)
            .envs(DRIVER_ENV.iter().copied())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status(),
    )
    .await;
}

/// The System Settings pane for one grant
pub fn open_settings(kind: &str) -> Result<()> {
    let url = match kind {
        "accessibility" => "x-apple.systempreferences:com.apple.settings.PrivacySecurity.extension?Privacy_Accessibility",
        "screenRecording" => "x-apple.systempreferences:com.apple.settings.PrivacySecurity.extension?Privacy_ScreenCapture",
        _ => bail!("未知的系统设置页面"),
    };
    let status = std::process::Command::new("/usr/bin/open").arg(url).status()?;
    if !status.success() {
        bail!("无法打开系统设置");
    }
    Ok(())
}

/// GUI work needs the user signed in at the Mac: the console belongs to the daemon's user
pub fn desktop_available() -> bool {
    use std::os::unix::fs::MetadataExt;
    // Safety: geteuid has no preconditions
    let me = unsafe { libc::geteuid() };
    std::fs::metadata("/dev/console").is_ok_and(|m| m.uid() == me)
}
