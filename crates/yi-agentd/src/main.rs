//! yi-agentd: the Yi Console daemon.
//!
//!   yi-agentd serve [-f]        start the daemon (-f runs in the foreground, logging to stderr)
//!   yi-agentd bridge [--local]  connect stdin/stdout to the local daemon, starting it if needed;
//!                            local clients pass --local, SSH remotes don't (the daemon uses this to restrict local-only methods)
//!   yi-agentd install|uninstall register/remove the autostart service
//!   yi-agentd status|stop|version
//!   yi-agentd relay setup <access link | wss://relay-address [--invite code] [--fp cert-fingerprint]>
//!   yi-agentd relay pair        generate a one-time pairing QR code (scan with a phone to connect, valid for 10 minutes)
//!   yi-agentd relay clients | revoke <id> | reset | disable

mod adapters;
mod fsapi;
mod gitops;
mod tmux;
mod history;
mod hub;
mod install;
mod log;
mod computer_use;
mod mcp;
mod paths;
mod ports;
mod probe;
mod proctree;
mod proxy;
mod statusline;
mod relay;
mod server;
mod settings;
mod stats;
mod store;
mod tools;
mod toolupdate;
mod transport;
mod util;

use anyhow::Result;
use std::time::Duration;

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    // --home <dir> is the same as YI_CONSOLE_HOME. spawn_detached passes it on Windows, because processes started via WMI
    // don't inherit our environment; set it here, before any threads, so tool child processes inherit it too
    if let Some(i) = args.iter().position(|a| a == "--home") {
        if let Some(dir) = args.get(i + 1).cloned() {
            std::env::set_var("YI_CONSOLE_HOME", dir);
            args.drain(i..=i + 1);
        }
    }
    let cmd = args.first().map(String::as_str).unwrap_or("help");
    let res = match cmd {
        "computer-use-mcp" => computer_use::proxy_main(&args[1..]),
        "serve" => serve(args.iter().any(|a| a == "-f" || a == "--foreground")),
        "bridge" => bridge(args.iter().any(|a| a == "--local")),
        "install" => install::install().map(|m| println!("{m}")),
        "uninstall" => install::uninstall().map(|m| println!("{m}")),
        "status" => {
            println!("{}", install::status());
            Ok(())
        }
        "stop" => {
            install::stop_running();
            Ok(())
        }
        "version" | "--version" | "-V" => {
            println!("yi-agentd {}", yi_proto::DAEMON_VERSION);
            Ok(())
        }
        "relay" => relay_cmd(&args[1..]),
        _ => {
            println!(
                "yi-agentd {}\n\n用法:\n  yi-agentd serve [-f]\n  yi-agentd bridge\n  yi-agentd install | uninstall | status | stop | version\n  yi-agentd relay setup <yiconsole://relay?… 接入链接>\n  yi-agentd relay setup <wss://中继地址> [--invite 邀请码] [--fp 证书指纹]\n  yi-agentd relay pair | clients | revoke <id> | reset | disable",
                yi_proto::DAEMON_VERSION
            );
            Ok(())
        }
    };
    if let Err(e) = res {
        eprintln!("错误: {e:#}");
        std::process::exit(1);
    }
}

fn relay_cmd(args: &[String]) -> Result<()> {
    const USAGE: &str = "用法: yi-agentd relay setup <接入链接 | wss://中继地址 [--invite 邀请码] [--fp 指纹]> | pair | clients | revoke <id> | reset | disable";
    let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    match args.first().map(String::as_str) {
        Some("setup") => {
            let url = args.get(1).ok_or_else(|| anyhow::anyhow!("{USAGE}"))?;
            if opt("--token").is_some() || args.iter().any(|a| a == "--reset-key") {
                anyhow::bail!("--token / --reset-key 已经去掉：登记中继用邀请（--invite 或接入链接），重置配对用 yi-agentd relay reset");
            }
            // An access link can also be given directly; address, fingerprint and invite code are all taken from it
            let c = if url.starts_with("yiconsole://") {
                let a = yi_link::RelayAccess::parse(url)?;
                relay::setup(&a.url, Some(&a.fp), &a.invite)?
            } else {
                relay::setup(url, opt("--fp").as_deref(), &opt("--invite").unwrap_or_default())?
            };
            println!("中继已设置：{}", c.url);
            // Make the running daemon reload its config
            install::stop_running();
            spawn_detached()?;
            println!("守护进程已重启。运行 `yi-agentd relay pair` 生成配对二维码。");
            Ok(())
        }
        Some("pair") => {
            let r = daemon_call("relay.pairStart", serde_json::json!({}))?;
            let uri = r["uri"].as_str().unwrap_or("");
            println!("用 Yi Console App 扫描下面的二维码，或复制配对链接（10 分钟内有效，只能用一次）：\n");
            relay::print_qr(uri);
            println!("{uri}\n");
            if r["online"] != true {
                println!("注意：守护进程还没连上中继，对方要等这边连上后才能完成配对。");
            }
            Ok(())
        }
        Some("clients") => {
            let r = daemon_call("relay.clients", serde_json::json!({}))?;
            let list = r.as_array().cloned().unwrap_or_default();
            if list.is_empty() {
                println!("还没有配对的设备");
            }
            for c in list {
                println!(
                    "{}  {}{}",
                    c["id"].as_str().unwrap_or(""),
                    c["name"].as_str().unwrap_or(""),
                    if c["online"] == true { "  （在线）" } else { "" }
                );
            }
            Ok(())
        }
        Some("revoke") => {
            let id = args.get(1).ok_or_else(|| anyhow::anyhow!("用法: yi-agentd relay revoke <设备 id>（yi-agentd relay clients 查看）"))?;
            daemon_call("relay.revoke", serde_json::json!({ "id": id }))?;
            println!("已移除 {id}，它的连接已断开");
            Ok(())
        }
        Some("reset") => {
            daemon_call("relay.reset", serde_json::json!({}))?;
            println!("已清空所有配对，旧的配对链接和已配对设备都失效了");
            Ok(())
        }
        Some("disable") => {
            relay::disable()?;
            install::stop_running();
            spawn_detached()?;
            println!("已关闭中继");
            Ok(())
        }
        _ => {
            println!("{USAGE}");
            Ok(())
        }
    }
}

/// Call a method on the local daemon from the command line (starting it first if not running)
fn daemon_call(method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    rt.block_on(async {
        let mut conn = None;
        for i in 0..50 {
            match transport::connect().await {
                Ok(c) => {
                    conn = Some(c);
                    break;
                }
                Err(_) if i == 0 => {
                    paths::ensure_base()?;
                    spawn_detached()?;
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
        let conn = conn.ok_or_else(|| anyhow::anyhow!("连不上守护进程"))?;
        let (r, mut w) = tokio::io::split(conn);
        let req = serde_json::json!({"id": 1, "method": method, "params": params});
        w.write_all(format!("{req}\n").as_bytes()).await?;
        w.flush().await?;
        let mut lines = BufReader::new(r).lines();
        while let Some(line) = tokio::time::timeout(Duration::from_secs(30), lines.next_line()).await?? {
            let v: serde_json::Value = serde_json::from_str(&line).unwrap_or_default();
            if v["id"] != 1 {
                continue;
            }
            if let Some(m) = v["error"]["message"].as_str() {
                anyhow::bail!("{m}");
            }
            return Ok(v["result"].clone());
        }
        anyhow::bail!("守护进程没有响应")
    })
}

fn serve(foreground: bool) -> Result<()> {
    // When run directly, e.g. by login autostart (HKCU Run), Windows opens a window for console programs;
    // in that case restart ourselves hidden
    #[cfg(windows)]
    if !foreground && owns_console_window() {
        return spawn_detached();
    }
    paths::ensure_base()?;
    log::init(foreground);
    tools::fix_path();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    rt.block_on(async {
        // Take the socket / pipe first; until we own it, touch nothing (database, relay, resume timers)
        let listener = match transport::bind().await {
            Ok(l) => l,
            Err(e) => {
                // Nobody watches stderr when running in the background, so the reason goes to the log
                crate::warn!("监听失败，守护进程退出: {e:#}");
                return Err(e);
            }
        };
        let store = store::Store::open(&paths::db_path())?;
        let hub = hub::Hub::new(store, settings::load());
        info!("yi-agentd {} starting, pid {}", yi_proto::DAEMON_VERSION, std::process::id());
        tokio::spawn(relay::run(hub.clone()));
        hub.reattach_terminals();
        hub.arm_pending_resumes();
        {
            let hub = hub.clone();
            tokio::spawn(async move {
                let mut t = tokio::time::interval(Duration::from_secs(60));
                loop {
                    t.tick().await;
                    hub.release_idle();
                    hub.track_terminal_sessions().await;
                }
            });
        }
        let listen = transport::serve(listener, hub.clone());
        tokio::select! {
            r = listen => if let Err(e) = r {
                // Nobody watches stderr when running in the background, so the reason goes to the log
                crate::warn!("本地连接中断，守护进程退出: {e:#}");
                return Err(e);
            },
            _ = hub.shutdown.notified() => info!("shutdown requested"),
            _ = tokio::signal::ctrl_c() => info!("interrupted"),
        }
        // Leaving the select! dropped the listener: the socket is gone and the single-instance lock released
        Ok::<_, anyhow::Error>(())
    })?;
    // Don't wait for background tasks (dropping their JsonProc kills tool process trees; on Windows the job objects also go when we exit)
    rt.shutdown_timeout(Duration::from_secs(2));
    Ok(())
}

/// The current process owns a visible console window by itself (not running inside someone else's terminal).
/// Visibility must be checked: processes started by spawn_detached also own their console, just hidden; otherwise it would restart forever
#[cfg(windows)]
fn owns_console_window() -> bool {
    use windows_sys::Win32::System::Console::{GetConsoleProcessList, GetConsoleWindow};
    use windows_sys::Win32::UI::WindowsAndMessaging::IsWindowVisible;
    unsafe {
        let w = GetConsoleWindow();
        if w.is_null() || IsWindowVisible(w) == 0 {
            return false;
        }
        let mut pids = [0u32; 4];
        GetConsoleProcessList(pids.as_mut_ptr(), pids.len() as u32) == 1
    }
}

/// Start a `yi-agentd serve` in the background, detached from the current terminal/SSH session
pub fn spawn_detached() -> Result<()> {
    let exe = std::env::current_exe()?;
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let mut cmd = std::process::Command::new(&exe);
        cmd.arg("serve")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        cmd.spawn()?;
    }
    #[cfg(windows)]
    {
        // Processes started over SSH live in a job object and are killed with it when the session disconnects;
        // starting via WMI's Win32_Process.Create puts the process outside that job
        let mut cmdline = format!("\"{}\" serve", exe.display());
        // WMI processes get the user's registry environment, not ours: pass a custom data directory explicitly,
        // otherwise a test daemon would start on the real ~/.yi-console. Trailing backslashes would escape the closing quote
        if let Some(home) = std::env::var_os("YI_CONSOLE_HOME").filter(|v| !v.is_empty()) {
            cmdline.push_str(&format!(" --home \"{}\"", home.to_string_lossy().trim_end_matches('\\')));
        }
        let cmdline = cmdline.replace('\'', "''");
        // WMI opens a visible window for console programs by default; ShowWindow=0 hides it;
        // children inherit this hidden console and don't pop up their own windows (WMI doesn't accept CREATE_NO_WINDOW)
        let ps = format!(
            "$si = New-CimInstance -ClassName Win32_ProcessStartup -ClientOnly -Property @{{ShowWindow=[uint16]0}}; \
             $r = Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{{CommandLine='{cmdline}'; ProcessStartupInformation=$si}}; \
             exit $r.ReturnValue"
        );
        use std::os::windows::process::CommandExt;
        let st = std::process::Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &ps])
            .creation_flags(0x0800_0000)
            .status()?;
        if !st.success() {
            anyhow::bail!("启动守护进程失败");
        }
    }
    Ok(())
}

/// stdin/stdout <-> local daemon. Without --local, first sends a remote marker line (see `BRIDGE_REMOTE_MARK`)
fn bridge(local: bool) -> Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        let conn = match transport::connect().await {
            Ok(c) => c,
            // The daemon is running but we lack permission to connect: starting another would just exit because the pipe name is taken
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => anyhow::bail!("{e}"),
            Err(_) => {
                paths::ensure_base()?;
                spawn_detached()?;
                let mut last = None;
                let mut got = None;
                for _ in 0..50 {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    match transport::connect().await {
                        Ok(c) => {
                            got = Some(c);
                            break;
                        }
                        Err(e) => last = Some(e),
                    }
                }
                match got {
                    Some(c) => c,
                    None => anyhow::bail!("无法启动守护进程: {:?}", last),
                }
            }
        };
        let (mut r, mut w) = tokio::io::split(conn);
        if !local {
            use tokio::io::AsyncWriteExt;
            w.write_all(format!("{}\n", yi_proto::BRIDGE_REMOTE_MARK).as_bytes()).await?;
        }
        let mut stdin = tokio::io::stdin();
        let mut stdout = tokio::io::stdout();
        let up = async {
            let _ = tokio::io::copy(&mut stdin, &mut w).await;
        };
        let down = async {
            use tokio::io::AsyncWriteExt;
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                use tokio::io::AsyncReadExt;
                match r.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if stdout.write_all(&buf[..n]).await.is_err() {
                            break;
                        }
                        let _ = stdout.flush().await;
                    }
                }
            }
        };
        tokio::select! { _ = up => {}, _ = down => {} }
        Ok::<(), anyhow::Error>(())
    })?;
    // The stdin reader thread is still blocked, and runtime shutdown would wait on it forever; once the daemon disconnects we must exit immediately
    // so the client notices and reconnects (reconnecting starts the daemon with the new yi-agentd)
    std::process::exit(0)
}
