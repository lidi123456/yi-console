//! 远程部署 yi-agentd：探测平台 → SFTP 上传对应二进制 → 可选注册自启服务。

use crate::ssh::SshSession;
use anyhow::{anyhow, bail, Result};
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;

#[derive(Debug, Clone)]
pub struct Platform {
    /// linux / macos / windows
    pub os: String,
    /// x86_64 / aarch64
    pub arch: String,
    pub yi_agentd_present: bool,
}

impl Platform {
    pub fn is_windows(&self) -> bool {
        self.os == "windows"
    }

    /// 远程 yi-agentd 的路径（相对家目录，SFTP 默认就在家目录）
    pub fn rel_path(&self) -> &'static str {
        if self.is_windows() {
            ".yi-console/bin/yi-agentd.exe"
        } else {
            ".yi-console/bin/yi-agentd"
        }
    }

    pub fn command(&self, args: &str) -> String {
        if self.is_windows() {
            format!(r#""%USERPROFILE%\.yi-console\bin\yi-agentd.exe" {args}"#)
        } else {
            format!(r#"exec "$HOME/.yi-console/bin/yi-agentd" {args}"#)
        }
    }
}

fn norm_arch(a: &str) -> String {
    match a.trim().to_lowercase().as_str() {
        "x86_64" | "amd64" | "x64" => "x86_64".into(),
        "aarch64" | "arm64" => "aarch64".into(),
        other => other.to_string(),
    }
}

pub async fn detect(s: &SshSession) -> Result<Platform> {
    let out = s
        .exec(r#"uname -sm; test -x "$HOME/.yi-console/bin/yi-agentd" && echo YI_AGENTD_OK"#)
        .await?;
    let first = out.stdout.lines().next().unwrap_or("").trim().to_string();
    let mut parts = first.split_whitespace();
    let (sys, mach) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    if sys == "Linux" || sys == "Darwin" {
        return Ok(Platform {
            os: if sys == "Linux" { "linux" } else { "macos" }.into(),
            arch: norm_arch(mach),
            yi_agentd_present: out.stdout.contains("YI_AGENTD_OK"),
        });
    }
    // Windows OpenSSH 默认 shell 是 cmd
    let out = s
        .exec(r#"echo WIN %PROCESSOR_ARCHITECTURE% & if exist "%USERPROFILE%\.yi-console\bin\yi-agentd.exe" echo YI_AGENTD_OK"#)
        .await?;
    if out.stdout.contains("WIN ") {
        let arch = out
            .stdout
            .split_whitespace()
            .nth(1)
            .map(norm_arch)
            .unwrap_or_else(|| "x86_64".into());
        return Ok(Platform {
            os: "windows".into(),
            arch,
            yi_agentd_present: out.stdout.contains("YI_AGENTD_OK"),
        });
    }
    bail!("无法识别远程系统（uname 输出：{first}）")
}

/// 客户端随附的 yi-agentd 二进制：<dir>/yi-agentd-<os>-<arch>[.exe]
pub fn local_binary(dir: Option<&Path>, os: &str, arch: &str) -> Option<PathBuf> {
    let name = format!(
        "yi-agentd-{os}-{arch}{}",
        if os == "windows" { ".exe" } else { "" }
    );
    let mut cands = vec![];
    if let Some(d) = dir {
        cands.push(d.join(&name));
    }
    if let Ok(d) = std::env::var("YI_CONSOLE_AGENTD_DIR") {
        cands.push(PathBuf::from(d).join(&name));
    }
    cands.into_iter().find(|p| p.is_file())
}

/// 上传（或升级）yi-agentd。先写临时文件再改名，运行中的旧进程不受影响。
pub async fn upload(s: &SshSession, p: &Platform, local: &Path, progress: impl Fn(u64, u64)) -> Result<()> {
    let data = tokio::fs::read(local).await?;
    let sftp = s.sftp().await?;
    for d in [".yi-console", ".yi-console/bin"] {
        if !sftp.try_exists(d).await.unwrap_or(false) {
            sftp.create_dir(d).await.map_err(|e| anyhow!("创建 {d} 失败: {e}"))?;
        }
    }
    let target = p.rel_path();
    let tmp = format!("{target}.new");
    {
        let mut f = sftp.create(&tmp).await.map_err(|e| anyhow!("上传失败: {e}"))?;
        let total = data.len() as u64;
        let mut sent = 0u64;
        for chunk in data.chunks(256 * 1024) {
            f.write_all(chunk).await?;
            sent += chunk.len() as u64;
            progress(sent, total);
        }
        f.flush().await?;
        f.shutdown().await?;
    }
    if !p.is_windows() {
        let mut attrs = russh_sftp::protocol::FileAttributes::empty();
        attrs.permissions = Some(0o755);
        sftp.set_metadata(&tmp, attrs).await?;
    }
    if sftp.try_exists(target).await.unwrap_or(false) {
        // Windows 不能覆盖运行中的 exe，但可以改名
        let old = format!("{target}.old");
        let _ = sftp.remove_file(&old).await;
        sftp.rename(target, &old).await?;
    }
    sftp.rename(&tmp, target).await?;
    let _ = sftp.close().await;
    Ok(())
}

pub async fn install_service(s: &SshSession, p: &Platform) -> Result<String> {
    let out = s.exec(&p.command("install")).await?;
    if out.code != Some(0) {
        bail!("注册服务失败: {}{}", out.stdout, out.stderr);
    }
    Ok(out.stdout.trim().to_string())
}
