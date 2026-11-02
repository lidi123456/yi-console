//! Include the commit hash in the version: even under the same 0.1.0, it shows when the daemon and client come from different builds, prompting a restart

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
    // Build time: used to tell which side is newer (commit hashes have no order); the client and every platform's yi-agentd built from the same sources must agree.
    // A clean git working tree uses the commit time; with uncommitted changes or no git (building from copied sources), use the latest modification time of sources under crates.
    // The current time at compile time won't do: the client and each platform's yi-agentd are compiled separately at different times and would never match after an upgrade
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
    // Given explicitly by CI (commit time): the Linux yi-agentd is compiled in a cross container where git may not run,
    // and falling back to source modification times wouldn't match the other platforms' builds
    println!("cargo:rerun-if-env-changed=YI_CONSOLE_BUILD_TIME");
    let time = std::env::var("YI_CONSOLE_BUILD_TIME")
        .ok()
        .filter(|t| t.parse::<u64>().is_ok())
        .or_else(|| (dirty == Some(false)).then(commit_time).flatten())
        .unwrap_or_else(|| newest_mtime(Path::new("../..").join("crates").as_path()).to_string());
    println!("cargo:rustc-env=YI_CONSOLE_BUILD_TIME={time}");
}

/// Latest modification time (seconds) of .rs / .toml files under a directory, skipping target
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
