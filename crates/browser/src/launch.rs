//! Finding and starting Chrome. Arguments and the `DevToolsActivePort` handshake follow agent-browser's
//! `cdp/chrome.rs` (Apache-2.0, Copyright Vercel, Inc.).

use anyhow::{anyhow, bail, Result};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::process::{Child, Command};

pub const DEFAULT_VIEWPORT: (u32, u32) = (1280, 720);

/// The browser to use: an explicit path if it exists, else the first known Chrome / Edge / Chromium install
pub fn find_browser(explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = explicit {
        return p.is_file().then(|| p.to_path_buf());
    }
    candidates().into_iter().find(|p| p.is_file()).or_else(test_caches)
}

fn candidates() -> Vec<PathBuf> {
    let mut out = vec![];
    #[cfg(windows)]
    {
        let env = |k: &str| std::env::var_os(k).map(PathBuf::from);
        for base in [env("LOCALAPPDATA"), env("ProgramFiles"), env("ProgramFiles(x86)")].into_iter().flatten() {
            out.push(base.join(r"Google\Chrome\Application\chrome.exe"));
        }
        // agent-browser stops at Chrome; Windows ships Edge, so it is the fallback here
        for base in [env("ProgramFiles(x86)"), env("ProgramFiles"), env("LOCALAPPDATA")].into_iter().flatten() {
            out.push(base.join(r"Microsoft\Edge\Application\msedge.exe"));
        }
    }
    #[cfg(target_os = "macos")]
    {
        let mut roots = vec![PathBuf::from("/Applications")];
        if let Some(h) = std::env::var_os("HOME") {
            roots.push(PathBuf::from(h).join("Applications"));
        }
        for r in roots {
            out.push(r.join("Google Chrome.app/Contents/MacOS/Google Chrome"));
            out.push(r.join("Chromium.app/Contents/MacOS/Chromium"));
            out.push(r.join("Microsoft Edge.app/Contents/MacOS/Microsoft Edge"));
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let path = std::env::var_os("PATH").unwrap_or_default();
        for name in ["google-chrome", "google-chrome-stable", "chromium-browser", "chromium", "microsoft-edge"] {
            for dir in std::env::split_paths(&path) {
                out.push(dir.join(name));
            }
        }
    }
    // A daemon started by systemd or a desktop launcher may have a trimmed PATH
    #[cfg(all(unix, not(target_os = "macos")))]
    for p in ["/opt/google/chrome/chrome", "/usr/bin/google-chrome", "/usr/bin/chromium", "/usr/bin/chromium-browser", "/snap/bin/chromium", "/opt/microsoft/msedge/msedge"] {
        out.push(PathBuf::from(p));
    }
    out
}

/// Chrome for Testing / Chromium downloaded by Playwright or Puppeteer
fn test_caches() -> Option<PathBuf> {
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from)?;
    let mut roots = vec![home.join(".cache").join("puppeteer").join("chrome")];
    #[cfg(windows)]
    if let Some(l) = std::env::var_os("LOCALAPPDATA") {
        roots.push(PathBuf::from(l).join("ms-playwright"));
    }
    #[cfg(target_os = "macos")]
    roots.push(home.join("Library/Caches/ms-playwright"));
    #[cfg(all(unix, not(target_os = "macos")))]
    roots.push(home.join(".cache/ms-playwright"));
    let exe: &[&str] = if cfg!(windows) {
        &["chrome-win64/chrome.exe", "chrome-win/chrome.exe"]
    } else if cfg!(target_os = "macos") {
        &[
            "chrome-mac-arm64/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing",
            "chrome-mac-x64/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing",
            "chrome-mac/Chromium.app/Contents/MacOS/Chromium",
        ]
    } else {
        &["chrome-linux64/chrome", "chrome-linux/chrome"]
    };
    for root in roots {
        let Ok(dirs) = std::fs::read_dir(&root) else { continue };
        let mut dirs: Vec<_> = dirs.filter_map(|d| d.ok()).map(|d| d.path()).filter(|p| p.is_dir()).collect();
        // Newest version first (directory names end in a version / revision)
        dirs.sort();
        for d in dirs.iter().rev() {
            if let Some(p) = exe.iter().map(|e| d.join(e)).find(|p| p.is_file()) {
                return Some(p);
            }
        }
    }
    None
}

#[derive(Debug, Clone)]
pub struct LaunchOptions {
    pub executable: PathBuf,
    pub profile_dir: PathBuf,
    pub headless: bool,
    pub viewport: (u32, u32),
    /// Raw extra switches (tests)
    pub extra_args: Vec<String>,
    /// Hosts pages may reach (`example.com`, `*.example.com`); empty: no limit
    pub allowed_domains: Vec<String>,
    /// Where downloads go; None refuses them
    pub download_dir: Option<PathBuf>,
    /// Extra environment for the browser process (DISPLAY of a private X server)
    pub env: Vec<(String, String)>,
    /// Accept invalid certificates everywhere (a host setting); localhost always accepts them
    pub ignore_https_errors: bool,
}

impl LaunchOptions {
    pub fn new(executable: PathBuf, profile_dir: PathBuf, headless: bool) -> Self {
        Self { executable, profile_dir, headless, viewport: DEFAULT_VIEWPORT, extra_args: vec![], allowed_domains: vec![], download_dir: None, env: vec![], ignore_https_errors: false }
    }
}

/// Whether a headed browser can open a window here without a private X server
pub fn has_display() -> bool {
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::env::var_os("DISPLAY").is_some_and(|d| !d.is_empty()) || std::env::var_os("WAYLAND_DISPLAY").is_some_and(|d| !d.is_empty())
    }
    #[cfg(not(all(unix, not(target_os = "macos"))))]
    true
}

/// Linux without a display: start an Xvfb only this browser uses (agent-browser does the same). Returns the server
/// and its DISPLAY, or None when there is a display already
pub async fn private_display(o: &LaunchOptions, prepare: &(dyn Fn(&mut Command) + Sync)) -> Result<Option<(Child, String)>> {
    if has_display() {
        return Ok(None);
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let exe = std::env::split_paths(&path).map(|d| d.join("Xvfb")).find(|p| p.is_file()).ok_or_else(|| anyhow!("这台主机没有图形桌面，也没有安装 Xvfb，不能用有窗口模式打开浏览器"))?;
        let mut cmd = Command::new(exe);
        // -displayfd: the server picks a free display number and writes it to stdout
        cmd.args(["-displayfd", "1", "-screen", "0", &format!("{}x{}x24", o.viewport.0, o.viewport.1), "-nolisten", "tcp"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        prepare(&mut cmd);
        let mut child = cmd.spawn().map_err(|e| anyhow!("无法启动 Xvfb：{e}"))?;
        let out = child.stdout.take().ok_or_else(|| anyhow!("Xvfb 没有输出"))?;
        use tokio::io::AsyncBufReadExt;
        let mut lines = tokio::io::BufReader::new(out).lines();
        let n = tokio::time::timeout(Duration::from_secs(10), lines.next_line()).await.map_err(|_| anyhow!("Xvfb 启动超时"))??;
        let n = n.unwrap_or_default().trim().to_string();
        if n.is_empty() || !n.chars().all(|c| c.is_ascii_digit()) {
            let _ = child.kill().await;
            bail!("Xvfb 没有给出显示编号");
        }
        Ok(Some((child, format!(":{n}"))))
    }
    #[cfg(not(all(unix, not(target_os = "macos"))))]
    {
        let _ = (o, prepare);
        Ok(None)
    }
}

pub fn build_args(o: &LaunchOptions) -> Vec<String> {
    let mut a: Vec<String> = [
        "--remote-debugging-port=0",
        "--no-first-run",
        "--no-default-browser-check",
        "--disable-background-networking",
        "--disable-backgrounding-occluded-windows",
        // Background tabs keep full-speed timers: the AI comes back to a tab it opened earlier (agent-browser 3f6600a)
        "--disable-renderer-backgrounding",
        "--disable-background-timer-throttling",
        // No "Restore pages?" bubble over the page after a crash of a kept profile
        "--hide-crash-restore-bubble",
        "--disable-session-crashed-bubble",
        // Dev servers on localhost with self-signed certificates (Vite / Next https, mkcert)
        "--allow-insecure-localhost",
        "--disable-component-update",
        "--disable-default-apps",
        "--disable-hang-monitor",
        "--disable-popup-blocking",
        "--disable-prompt-on-repost",
        "--disable-sync",
        "--disable-features=Translate",
        "--metrics-recording-only",
        "--password-store=basic",
        "--use-mock-keychain",
    ]
    .map(String::from)
    .to_vec();
    if o.headless {
        a.push("--headless=new".into());
        a.push("--hide-scrollbars".into());
        a.push("--enable-unsafe-swiftshader".into());
    }
    if o.ignore_https_errors {
        a.push("--ignore-certificate-errors".into());
    }
    if !o.allowed_domains.is_empty() {
        // WebRTC could reach any host over UDP; the page script removes RTCPeerConnection too
        a.push("--force-webrtc-ip-handling-policy=disable_non_proxied_udp".into());
    }
    a.push(format!("--window-size={},{}", o.viewport.0, o.viewport.1));
    a.push(format!("--user-data-dir={}", o.profile_dir.display()));
    a.extend(o.extra_args.iter().cloned());
    if needs_no_sandbox() {
        a.push("--no-sandbox".into());
    }
    if small_dev_shm() {
        a.push("--disable-dev-shm-usage".into());
    }
    // The first tab: blank, so nothing loads before we attach
    a.push("about:blank".into());
    a
}

/// Chrome's sandbox can't start as root or inside most containers
fn needs_no_sandbox() -> bool {
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let root = std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|s| s.lines().find(|l| l.starts_with("Uid:")).map(|l| l.split_whitespace().nth(1) == Some("0")))
            .unwrap_or(false);
        root || Path::new("/.dockerenv").exists() || Path::new("/run/.containerenv").exists()
    }
    #[cfg(not(all(unix, not(target_os = "macos"))))]
    false
}

fn small_dev_shm() -> bool {
    #[cfg(target_os = "linux")]
    {
        // statvfs would need libc; Docker's default is 64 MiB, which /proc/mounts shows as size=65536k
        std::fs::read_to_string("/proc/mounts")
            .ok()
            .and_then(|m| m.lines().find(|l| l.split_whitespace().nth(1) == Some("/dev/shm")).map(String::from))
            .and_then(|l| l.split("size=").nth(1).map(|s| s.split([',', ' ']).next().unwrap_or("").to_string()))
            .and_then(|s| s.strip_suffix('k').and_then(|n| n.parse::<u64>().ok()))
            .is_some_and(|kb| kb < 512 * 1024)
    }
    #[cfg(not(target_os = "linux"))]
    false
}

/// A started Chrome and the browser WebSocket URL read from `DevToolsActivePort`
pub struct Spawned {
    pub child: Child,
    pub ws_url: String,
}

/// Start Chrome. `prepare` runs on the command before spawning (process group / job setup by the caller)
pub async fn spawn(o: &LaunchOptions, prepare: &(dyn Fn(&mut Command) + Sync)) -> Result<Spawned> {
    std::fs::create_dir_all(&o.profile_dir)?;
    // A file left by an earlier run would point at a dead port
    let port_file = o.profile_dir.join("DevToolsActivePort");
    let _ = std::fs::remove_file(&port_file);
    let mut cmd = Command::new(&o.executable);
    cmd.args(build_args(o))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    for (k, v) in &o.env {
        cmd.env(k, v);
    }
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW: no console for the browser process
    prepare(&mut cmd);
    let mut child = cmd.spawn().map_err(|e| anyhow!("无法启动浏览器 {}：{e}", o.executable.display()))?;
    // Chrome logs to stderr all the time: drain it so the pipe never fills, keeping the last lines for errors
    let tail = std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::<String>::new()));
    if let Some(e) = child.stderr.take() {
        let tail = tail.clone();
        tokio::spawn(async move {
            use tokio::io::AsyncBufReadExt;
            let mut lines = tokio::io::BufReader::new(e).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let mut t = tail.lock().unwrap();
                t.push_back(line.chars().take(300).collect());
                if t.len() > 8 {
                    t.pop_front();
                }
            }
        });
    }
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(30) {
        if let Ok(text) = std::fs::read_to_string(&port_file) {
            if let Some(url) = parse_port_file(&text) {
                return Ok(Spawned { child, ws_url: url });
            }
        }
        if let Some(status) = child.try_wait()? {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let err = tail.lock().unwrap().iter().cloned().collect::<Vec<_>>().join("\n");
            if err.contains("ProcessSingleton") || err.contains("profile appears to be in use") {
                bail!("浏览器资料目录正被另一个浏览器进程使用");
            }
            bail!("浏览器启动后退出了（{status}）{}", if err.is_empty() { String::new() } else { format!("：{err}") });
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let _ = child.kill().await;
    bail!("浏览器启动超时，没有写出调试端口")
}

/// `DevToolsActivePort`: the port on the first line, the browser target path on the second
pub fn parse_port_file(text: &str) -> Option<String> {
    let mut lines = text.lines();
    let port: u16 = lines.next()?.trim().parse().ok()?;
    let path = lines.next()?.trim();
    (port > 0 && path.starts_with("/devtools/browser/")).then(|| format!("ws://127.0.0.1:{port}{path}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_file() {
        assert_eq!(parse_port_file("9222\n/devtools/browser/abc\n").as_deref(), Some("ws://127.0.0.1:9222/devtools/browser/abc"));
        assert_eq!(parse_port_file("9222\n"), None);
        assert_eq!(parse_port_file("x\n/devtools/browser/abc"), None);
        assert_eq!(parse_port_file("0\n/devtools/browser/abc"), None);
    }

    #[test]
    fn headless_args() {
        let o = LaunchOptions { viewport: (800, 600), ..LaunchOptions::new("chrome".into(), "/p".into(), true) };
        let a = build_args(&o);
        assert!(a.contains(&"--remote-debugging-port=0".to_string()));
        assert!(a.contains(&"--headless=new".to_string()));
        assert!(a.contains(&"--window-size=800,600".to_string()));
        assert_eq!(a.last().map(String::as_str), Some("about:blank"));
        let headed = build_args(&LaunchOptions { headless: false, ..o });
        assert!(!headed.iter().any(|x| x.starts_with("--headless")));
    }

    #[test]
    fn explicit_path_must_exist() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("chrome");
        assert_eq!(find_browser(Some(&f)), None);
        std::fs::write(&f, b"").unwrap();
        assert_eq!(find_browser(Some(&f)), Some(f));
    }
}
