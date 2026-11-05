//! Tool versions and upgrades for the tool list in the host manager.
//!
//! Upgrades run the tool's own update command (`claude update`, `codex update`, …), so each tool picks the right way for
//! how it was installed (npm, its install script, a package manager); we never install a copy of our own. The newest
//! published version is only known for tools on npm; the others show an upgrade button without a version to compare.

use crate::hub::Hub;
use anyhow::{anyhow, bail, Result};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use yi_proto::{methods, ToolKind};

/// An update command still running after this long is ended
const MAX_RUN: Duration = Duration::from_secs(600);
/// Lines of output kept per job (for a client that opens the list while it runs)
const KEEP_LINES: usize = 200;
/// How long a latest-version lookup is reused
const LATEST_TTL_MS: i64 = 10 * 60_000;

/// The tool's own update subcommand
fn update_args(kind: ToolKind) -> Option<&'static [&'static str]> {
    Some(match kind {
        ToolKind::Claude | ToolKind::Codex | ToolKind::Cursor | ToolKind::Qoder | ToolKind::Codebuddy => &["update"],
        // Without it Kiro asks for confirmation on stdin
        ToolKind::Kiro => &["update", "--non-interactive"],
        ToolKind::Shell | ToolKind::Files => return None,
    })
}

/// npm package, for tools published there
fn npm_package(kind: ToolKind) -> Option<&'static str> {
    match kind {
        ToolKind::Claude => Some("@anthropic-ai/claude-code"),
        ToolKind::Codex => Some("@openai/codex"),
        ToolKind::Codebuddy => Some("@tencent-ai/codebuddy-code"),
        _ => None,
    }
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolJob {
    pub running: bool,
    /// Set when finished: whether the update command succeeded
    pub ok: Option<bool>,
    pub error: Option<String>,
    /// Versions reported by `--version` before and after
    pub from: Option<String>,
    pub to: Option<String>,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub output: VecDeque<String>,
}

fn jobs() -> &'static Mutex<HashMap<ToolKind, ToolJob>> {
    static J: OnceLock<Mutex<HashMap<ToolKind, ToolJob>>> = OnceLock::new();
    J.get_or_init(Default::default)
}

type Latest = (i64, Result<String, String>);
fn latest_cache() -> &'static Mutex<HashMap<ToolKind, Latest>> {
    static C: OnceLock<Mutex<HashMap<ToolKind, Latest>>> = OnceLock::new();
    C.get_or_init(Default::default)
}

/// "npm" when the executable lives in (or links into) an npm global install, else "native"
fn install_via(path: &Path) -> &'static str {
    let real = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let s = |p: &Path| p.to_string_lossy().replace('\\', "/").to_lowercase();
    let (a, b) = (s(path), s(&real));
    // Windows npm shims: %APPDATA%\npm\tool.cmd
    if a.contains("/node_modules/") || b.contains("/node_modules/") || a.contains("/appdata/roaming/npm/") {
        "npm"
    } else {
        "native"
    }
}

fn npm_latest(pkg: &str) -> Result<String, String> {
    let reg = std::env::var("npm_config_registry")
        .or_else(|_| std::env::var("NPM_CONFIG_REGISTRY"))
        .unwrap_or_else(|_| "https://registry.npmjs.org".into());
    let url = format!("{}/{pkg}/latest", reg.trim_end_matches('/'));
    let mut resp = ureq::get(&url)
        .header("Accept", "application/json")
        .config()
        .timeout_global(Some(Duration::from_secs(15)))
        .build()
        .call()
        .map_err(|e| format!("查询最新版本失败：{e}"))?;
    let v: Value = serde_json::from_str(&resp.body_mut().read_to_string().map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    v["version"].as_str().map(str::to_string).ok_or_else(|| "查询最新版本失败：返回内容里没有版本号".into())
}

/// `tools.latest`
pub fn latest(hub: &Hub, refresh: bool) -> Value {
    let tools = crate::server::tools(hub, false);
    let now = crate::util::now_ms();
    // Look up the npm tools in parallel; each is one small HTTPS request
    let lookups: Vec<_> = tools
        .iter()
        .filter(|t| t.available)
        .filter_map(|t| npm_package(t.kind).map(|p| (t.kind, p)))
        .filter(|(k, _)| {
            refresh || latest_cache().lock().unwrap().get(k).map_or(true, |(at, _)| now - at > LATEST_TTL_MS)
        })
        .map(|(k, p)| (k, std::thread::spawn(move || npm_latest(p))))
        .collect();
    for (k, h) in lookups {
        let r = h.join().unwrap_or_else(|_| Err("查询最新版本失败".into()));
        latest_cache().lock().unwrap().insert(k, (now, r));
    }
    let cache = latest_cache().lock().unwrap();
    let mut out = serde_json::Map::new();
    for t in &tools {
        if update_args(t.kind).is_none() {
            continue;
        }
        let mut v = json!({ "canUpdate": t.available });
        if let Some(p) = &t.path {
            v["via"] = json!(install_via(Path::new(p)));
        }
        match cache.get(&t.kind).filter(|_| t.available) {
            Some((_, Ok(l))) => v["latest"] = json!(l),
            Some((_, Err(e))) => v["error"] = json!(e),
            None => {}
        }
        out.insert(t.kind.as_str().into(), v);
    }
    let jobs: serde_json::Map<String, Value> = jobs()
        .lock()
        .unwrap()
        .iter()
        .map(|(k, j)| (k.as_str().to_string(), serde_json::to_value(j).unwrap_or(Value::Null)))
        .collect();
    // `info`: the daemon's current detection, which the client's cached hello misses after an upgrade
    json!({ "tools": out, "jobs": jobs, "info": tools })
}

/// `tools.update`: checks, then runs the update in the background and returns at once
pub async fn start(hub: &Arc<Hub>, kind: ToolKind) -> Result<Value> {
    let args = update_args(kind).ok_or_else(|| anyhow!("这个工具不能升级"))?;
    let info = crate::server::tools(hub, false).into_iter().find(|t| t.kind == kind);
    let Some(path) = info.as_ref().and_then(|t| t.path.clone()).filter(|_| info.as_ref().is_some_and(|t| t.available)) else {
        bail!("这台机器上没有安装 {}", kind.as_str());
    };
    if jobs().lock().unwrap().values().any(|j| j.running) {
        bail!("另一个工具正在升级，等它完成后再试");
    }
    // Windows won't replace a running .exe, so idle sessions' processes are ended first (they restart on the next
    // message, like the idle release); sessions in a turn block the upgrade on every system
    let (busy, released) = hub.tool_in_use(kind, cfg!(windows));
    if busy > 0 {
        bail!("有 {busy} 个 {} 会话正在使用，等它们结束后再升级", kind.as_str());
    }
    for _ in 0..50 {
        if released.iter().all(|tx| tx.is_closed()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    {
        let mut g = jobs().lock().unwrap();
        if g.values().any(|j| j.running) {
            bail!("另一个工具正在升级，等它完成后再试");
        }
        g.insert(
            kind,
            ToolJob {
                running: true,
                from: info.and_then(|t| t.version),
                started_at: crate::util::now_ms(),
                ..Default::default()
            },
        );
    }
    publish(hub, kind, None, None);
    let hub2 = hub.clone();
    tokio::spawn(async move {
        let (ok, error) = match run(&hub2, kind, Path::new(&path), args).await {
            Ok(true) => (true, None),
            Ok(false) => (false, None),
            Err(e) => (false, Some(e.to_string())),
        };
        let hub3 = hub2.clone();
        // Re-detect: the update may have moved the executable or changed its version
        let tools = tokio::task::spawn_blocking(move || {
            crate::tools::fix_path();
            crate::tools::forget_probes();
            crate::server::tools(&hub3, true)
        })
        .await
        .unwrap_or_default();
        {
            let mut g = jobs().lock().unwrap();
            if let Some(j) = g.get_mut(&kind) {
                j.running = false;
                j.ok = Some(ok);
                j.error = error;
                j.to = tools.iter().find(|t| t.kind == kind).and_then(|t| t.version.clone());
                j.finished_at = Some(crate::util::now_ms());
            }
        }
        latest_cache().lock().unwrap().remove(&kind);
        publish(&hub2, kind, None, Some(serde_json::to_value(&tools).unwrap_or(Value::Null)));
    });
    Ok(json!({ "started": true }))
}

fn publish(hub: &Hub, kind: ToolKind, line: Option<&str>, tools: Option<Value>) {
    let job = {
        let g = jobs().lock().unwrap();
        let mut j = g.get(&kind).cloned().unwrap_or_default();
        // Lines are sent one by one; the full output only goes with the start / end
        if line.is_some() {
            j.output.clear();
        }
        j
    };
    let mut p = json!({ "tool": kind.as_str(), "job": job });
    if let Some(l) = line {
        p["line"] = json!(l);
    }
    if let Some(t) = tools {
        p["tools"] = t;
    }
    hub.publish(None, methods::EV_TOOLS_UPDATING, p);
}

/// Spinners redraw with \r and colour codes; keep the readable text
fn clean(s: &str) -> String {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(\x07|\x1b\\)|\x1b[()][A-Z0-9]").unwrap());
    re.replace_all(s, "").trim().to_string()
}

async fn run(hub: &Arc<Hub>, kind: ToolKind, path: &Path, args: &[&str]) -> Result<bool> {
    use tokio::io::AsyncReadExt;
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let mut cmd: tokio::process::Command = crate::adapters::std_command(path, &args).into();
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .env("NO_COLOR", "1")
        .current_dir(crate::paths::home());
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000);
    crate::proctree::prepare(&mut cmd);
    let line = |l: &str| {
        let l = clean(l);
        if l.is_empty() {
            return;
        }
        {
            let mut g = jobs().lock().unwrap();
            let Some(j) = g.get_mut(&kind) else { return };
            // A spinner repeating the same text
            if j.output.back() == Some(&l) {
                return;
            }
            j.output.push_back(l.clone());
            while j.output.len() > KEEP_LINES {
                j.output.pop_front();
            }
        }
        publish(hub, kind, Some(&l), None);
    };
    line(&format!("$ {} {}", path.display(), args.join(" ")));
    let mut child = cmd.spawn().map_err(|e| anyhow!("启动升级命令失败：{e}"))?;
    let tree = crate::proctree::Tree::attach(&child);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    for r in [
        child.stdout.take().map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Unpin + Send>),
        child.stderr.take().map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Unpin + Send>),
    ]
    .into_iter()
    .flatten()
    {
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut r = r;
            let mut buf = [0u8; 4096];
            let mut acc = Vec::new();
            while let Ok(n) = r.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                for &b in &buf[..n] {
                    if b == b'\n' || b == b'\r' {
                        let _ = tx.send(std::mem::take(&mut acc));
                    } else {
                        acc.push(b);
                    }
                }
            }
            if !acc.is_empty() {
                let _ = tx.send(acc);
            }
        });
    }
    drop(tx);
    let deadline = tokio::time::Instant::now() + MAX_RUN;
    let mut open = true;
    let status = loop {
        tokio::select! {
            l = rx.recv(), if open => match l {
                Some(b) => line(&String::from_utf8_lossy(&b)),
                None => open = false,
            },
            st = child.wait() => break st?,
            _ = tokio::time::sleep_until(deadline) => {
                tree.kill();
                let _ = child.wait().await;
                bail!("升级命令超过 {} 分钟没有结束，已停止", MAX_RUN.as_secs() / 60);
            }
        }
    };
    // Output still buffered after exit
    while let Ok(Some(b)) = tokio::time::timeout(Duration::from_millis(500), rx.recv()).await {
        line(&String::from_utf8_lossy(&b));
    }
    // Some updaters hand over to a helper that replaces the executable after they exit; dropping the tree would kill it
    std::mem::forget(tree);
    if !status.success() {
        line(&format!("exit {}", status.code().map(|c| c.to_string()).unwrap_or_else(|| "?".into())));
    }
    Ok(status.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_strips_escapes() {
        assert_eq!(clean("\x1b[32m✔\x1b[0m Updated to 1.2.3  "), "✔ Updated to 1.2.3");
        assert_eq!(clean("\x1b[?25l\x1b[2K"), "");
        assert_eq!(clean("\x1b]0;title\x07done"), "done");
    }

    #[test]
    fn npm_install_detected() {
        assert_eq!(install_via(Path::new("/usr/lib/node_modules/@openai/codex/bin/codex.js")), "npm");
        assert_eq!(install_via(Path::new(r"C:\Users\a\AppData\Roaming\npm\codebuddy.cmd")), "npm");
        assert_eq!(install_via(Path::new("/nonexistent/.local/bin/kiro-cli")), "native");
    }
}
