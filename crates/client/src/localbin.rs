//! Fixed location of the local yi-agentd: `~/.yi-console/bin/yi-agentd` (same as on SSH hosts).
//!
//! We don't run the copy in the installer directly: a resident daemon would lock files in the install directory and make app upgrades fail on Windows;
//! on macOS, running from a DMG or Downloads gives a randomized path, so a registered autostart service wouldn't find it next time.
//! So before each connection the yi-agentd shipped in the installer is synced here, and the daemon, bridge and autostart service all use this copy.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

fn exe_name() -> &'static str {
    if cfg!(windows) {
        "yi-agentd.exe"
    } else {
        "yi-agentd"
    }
}

/// Keep in sync with yi-agentd's `paths::base_dir`
pub fn base_dir() -> PathBuf {
    if let Ok(p) = std::env::var("YI_CONSOLE_HOME") {
        return PathBuf::from(p);
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".yi-console")
}

pub fn installed_path() -> PathBuf {
    base_dir().join("bin").join(exe_name())
}

/// Sync `bundled` to the fixed location and return that path. Does nothing if the contents are identical.
///
/// A running old daemon is unaffected: on Unix, replacing by rename doesn't affect files already open;
/// on Windows a running exe can't be overwritten but can be renamed, so move it aside as `.old` before placing the new one.
/// The new version only takes effect after the daemon restarts (the UI shows a version mismatch notice).
pub fn sync(bundled: &Path) -> Result<PathBuf> {
    sync_to(bundled, &installed_path())
}

/// Files shipped next to the bundled yi-agentd builds that the local daemon reads from its own bin directory:
/// CuaDriver.app for computer use, only in the macOS installer
const EXTRAS: &[&str] = &["computer-use-macos.tgz"];

/// Sync the extras present in `bundled_dir` next to the installed yi-agentd. Failures only cost the feature that
/// needs the file, so they are logged rather than stopping the connection.
pub fn sync_extras(bundled_dir: &Path) {
    for name in EXTRAS {
        let src = bundled_dir.join(name);
        if src.is_file() {
            if let Err(e) = sync_to(&src, &base_dir().join("bin").join(name)) {
                eprintln!("同步 {name} 失败: {e:#}");
            }
        }
    }
}

fn sync_to(bundled: &Path, target: &Path) -> Result<PathBuf> {
    let old = target.with_extension("old");
    // Old file left by the last replacement; can be deleted once its process has exited
    let _ = std::fs::remove_file(&old);
    if same_content(bundled, target) {
        return Ok(target.to_path_buf());
    }
    let dir = target.parent().context("yi-agentd 安装路径无效")?;
    std::fs::create_dir_all(dir).with_context(|| format!("创建 {} 失败", dir.display()))?;
    let tmp = target.with_extension("new");
    std::fs::copy(bundled, &tmp).with_context(|| format!("复制 yi-agentd 到 {} 失败", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    }
    if std::fs::rename(&tmp, target).is_err() {
        // Windows: the target is running, move it aside first
        std::fs::rename(target, &old).context("替换 yi-agentd 失败：旧文件被占用")?;
        std::fs::rename(&tmp, target)?;
    }
    Ok(target.to_path_buf())
}

fn same_content(a: &Path, b: &Path) -> bool {
    let (Ok(ma), Ok(mb)) = (std::fs::metadata(a), std::fs::metadata(b)) else {
        return false;
    };
    if ma.len() != mb.len() {
        return false;
    }
    matches!((std::fs::read(a), std::fs::read(b)), (Ok(x), Ok(y)) if x == y)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_then_skips_then_replaces() {
        let d = std::env::temp_dir().join(format!("ah-localbin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let src = d.join("bundled");
        let target = d.join("bin").join("yi-agentd");
        std::fs::write(&src, b"v1").unwrap();
        sync_to(&src, &target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"v1");
        let m1 = std::fs::metadata(&target).unwrap().modified().unwrap();
        sync_to(&src, &target).unwrap();
        assert_eq!(std::fs::metadata(&target).unwrap().modified().unwrap(), m1);
        std::fs::write(&src, b"v2").unwrap();
        sync_to(&src, &target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"v2");
        assert!(!target.with_extension("new").exists());
        let _ = std::fs::remove_dir_all(&d);
    }
}
