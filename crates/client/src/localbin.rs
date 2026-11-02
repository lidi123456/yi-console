//! 本机 yi-agentd 的固定位置：`~/.yi-console/bin/yi-agentd`（和 SSH 主机上一样）。
//!
//! 不直接运行安装包里那份：常驻的守护进程会占住安装目录里的文件，Windows 上升级 App 会因此失败；
//! macOS 从 DMG 或"下载"里运行时路径是随机的，注册成自启服务下次就找不到了。
//! 所以每次连接前把安装包自带的 yi-agentd 同步过来，守护进程、bridge 和自启服务都用这份。

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

fn exe_name() -> &'static str {
    if cfg!(windows) {
        "yi-agentd.exe"
    } else {
        "yi-agentd"
    }
}

/// 和 yi-agentd 的 `paths::base_dir` 保持一致
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

/// 把 `bundled` 同步到固定位置，返回固定位置的路径。内容相同就什么都不做。
///
/// 正在运行的旧守护进程不受影响：Unix 上改名覆盖不影响已打开的文件；
/// Windows 上运行中的 exe 不能覆盖，但可以改名，所以先把它挪成 `.old` 再放新的。
/// 新版本要等守护进程重启后才生效（界面上会提示版本不一致）。
pub fn sync(bundled: &Path) -> Result<PathBuf> {
    sync_to(bundled, &installed_path())
}

fn sync_to(bundled: &Path, target: &Path) -> Result<PathBuf> {
    let old = target.with_extension("old");
    // 上次替换留下的旧文件，进程退出后就能删掉
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
        // Windows：目标正在运行，先挪开
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
