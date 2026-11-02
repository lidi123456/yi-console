//! 版本号带上提交号：同一个 0.1.0 下守护进程和客户端不是同一次构建时也能看出来，提示重启

use std::path::Path;
use std::process::Command;
use std::time::UNIX_EPOCH;

fn main() {
    println!("cargo:rerun-if-env-changed=GITHUB_SHA");
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/refs/heads");
    let sha = std::env::var("GITHUB_SHA").ok().filter(|s| !s.is_empty()).or_else(|| {
        let out = Command::new("git").args(["rev-parse", "HEAD"]).output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    });
    let build = sha.map(|s| s.chars().take(7).collect()).unwrap_or_else(|| "dev".to_string());
    println!("cargo:rustc-env=YI_CONSOLE_BUILD={build}");
    // 构建时间：用来判断两端谁更新（提交号比不出先后），同一份源码编出的客户端和各平台 yi-agentd 必须一致。
    // 干净的 git 工作区用提交时间；有未提交改动或没有 git（拷源码构建）时用 crates 下源码最新的修改时间，
    // 不能用编译时的当前时间：客户端和各平台 yi-agentd 分几次编译，时间各不相同，升级完也对不上
    let dirty = Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=no"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| !o.stdout.is_empty());
    if dirty != Some(false) {
        println!("cargo:rerun-if-changed=../../crates");
    }
    let commit_time = || {
        let o = Command::new("git").args(["log", "-1", "--format=%ct"]).output().ok()?;
        let t = String::from_utf8_lossy(&o.stdout).trim().to_string();
        (o.status.success() && !t.is_empty()).then_some(t)
    };
    // CI 显式给出（提交时间）：Linux 的 yi-agentd 在 cross 的容器里编译，容器里不一定能跑 git，
    // 退回源码修改时间就会和其他平台的构建对不上
    println!("cargo:rerun-if-env-changed=YI_CONSOLE_BUILD_TIME");
    let time = std::env::var("YI_CONSOLE_BUILD_TIME")
        .ok()
        .filter(|t| t.parse::<u64>().is_ok())
        .or_else(|| (dirty == Some(false)).then(commit_time).flatten())
        .unwrap_or_else(|| newest_mtime(Path::new("../..").join("crates").as_path()).to_string());
    println!("cargo:rustc-env=YI_CONSOLE_BUILD_TIME={time}");
}

/// 目录下 .rs / .toml 文件最新的修改时间（秒），跳过 target
fn newest_mtime(dir: &Path) -> u64 {
    let Ok(rd) = std::fs::read_dir(dir) else { return 0 };
    rd.flatten()
        .map(|e| {
            let p = e.path();
            if p.is_dir() {
                if p.file_name().is_some_and(|n| n == "target") { 0 } else { newest_mtime(&p) }
            } else if p.extension().is_some_and(|x| x == "rs" || x == "toml") {
                e.metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_secs())
            } else {
                0
            }
        })
        .max()
        .unwrap_or(0)
}
