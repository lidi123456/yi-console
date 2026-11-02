//! Register yi-agentd as a user-level service that starts at boot/login.
//! - Linux: systemd --user (and try enable-linger so it keeps running after logout); falls back to crontab @reboot without systemd
//! - macOS: launchd LaunchAgent
//! - Windows: HKCU Run registry entry (no admin needed), and start it right away via WMI (outside the SSH session's job)

use anyhow::{anyhow, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

const UNIT: &str = "yi-console.service";
#[cfg(target_os = "macos")]
const LABEL: &str = "dev.yiconsole.yi-agentd";

fn exe() -> Result<PathBuf> {
    Ok(std::env::current_exe()?.canonicalize()?)
}

fn run(cmd: &mut Command) -> (bool, String) {
    match cmd.output() {
        Ok(o) => (
            o.status.success(),
            format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            )
            .trim()
            .to_string(),
        ),
        Err(e) => (false, e.to_string()),
    }
}

pub fn install() -> Result<String> {
    crate::paths::ensure_base()?;
    crate::tools::fix_path();
    let exe = exe()?;
    platform_install(&exe)
}

pub fn uninstall() -> Result<String> {
    platform_uninstall()
}

#[cfg(target_os = "linux")]
fn platform_install(exe: &Path) -> Result<String> {
    let path = std::env::var("PATH").unwrap_or_default();
    let unit_dir = crate::paths::home().join(".config/systemd/user");
    std::fs::create_dir_all(&unit_dir)?;
    let unit = format!(
        "[Unit]\nDescription=Yi Console daemon\nAfter=network-online.target\n\n\
         [Service]\nExecStart={} serve\nRestart=always\nRestartSec=3\n\
         Environment=\"PATH={}\"\nKillMode=process\n\n\
         [Install]\nWantedBy=default.target\n",
        exe.display(),
        path.replace('"', "")
    );
    std::fs::write(unit_dir.join(UNIT), unit)?;

    let (ok, out) = run(Command::new("systemctl").args(["--user", "daemon-reload"]));
    if ok {
        // If an instance not started by systemd is already running, stop it first and let systemd take over
        stop_running();
        let (ok, out2) = run(Command::new("systemctl").args(["--user", "enable", "--now", UNIT]));
        if !ok {
            return Err(anyhow!("systemctl enable 失败: {out2}"));
        }
        let user = std::env::var("USER").unwrap_or_default();
        let (linger, lout) = run(Command::new("loginctl").args(["enable-linger", &user]));
        let mut msg = format!("已注册 systemd 用户服务 {UNIT}");
        if !linger {
            msg.push_str(&format!(
                "\n注意：enable-linger 失败（{lout}），退出登录后服务可能被停止；可让管理员执行 `sudo loginctl enable-linger {user}`"
            ));
        }
        return Ok(msg);
    }

    // No systemd user instance: crontab @reboot + start in the background right away
    let line = format!("@reboot {} serve >/dev/null 2>&1", exe.display());
    let (_, current) = run(Command::new("crontab").arg("-l"));
    if !current.contains(&line) {
        let new = format!("{}\n{line}\n", current.trim_end());
        let mut child = Command::new("crontab")
            .arg("-")
            .stdin(std::process::Stdio::piped())
            .spawn()?;
        use std::io::Write;
        child.stdin.take().unwrap().write_all(new.as_bytes())?;
        child.wait()?;
    }
    crate::spawn_detached()?;
    Ok(format!(
        "systemd --user 不可用（{out}），已改用 crontab @reboot 自启并在后台启动"
    ))
}

#[cfg(target_os = "linux")]
fn platform_uninstall() -> Result<String> {
    let _ = run(Command::new("systemctl").args(["--user", "disable", "--now", UNIT]));
    let unit = crate::paths::home().join(".config/systemd/user").join(UNIT);
    let _ = std::fs::remove_file(unit);
    let _ = run(Command::new("systemctl").args(["--user", "daemon-reload"]));
    let (_, current) = run(Command::new("crontab").arg("-l"));
    if current.contains("yi-agentd") && current.contains("@reboot") {
        let kept: Vec<&str> = current
            .lines()
            .filter(|l| !(l.contains("@reboot") && l.contains(" serve")))
            .collect();
        if let Ok(mut child) = Command::new("crontab")
            .arg("-")
            .stdin(std::process::Stdio::piped())
            .spawn()
        {
            use std::io::Write;
            let _ = child
                .stdin
                .take()
                .unwrap()
                .write_all(format!("{}\n", kept.join("\n")).as_bytes());
            let _ = child.wait();
        }
    }
    stop_running();
    Ok("已移除自启服务".into())
}

#[cfg(target_os = "macos")]
fn platform_install(exe: &Path) -> Result<String> {
    let path = std::env::var("PATH").unwrap_or_default();
    let dir = crate::paths::home().join("Library/LaunchAgents");
    std::fs::create_dir_all(&dir)?;
    let plist = dir.join(format!("{LABEL}.plist"));
    let log = crate::paths::base_dir().join("launchd.log");
    let content = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key><array><string>{}</string><string>serve</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>EnvironmentVariables</key><dict><key>PATH</key><string>{}</string></dict>
  <key>StandardErrorPath</key><string>{}</string>
</dict></plist>
"#,
        xml_escape(&exe.display().to_string()),
        xml_escape(&path),
        xml_escape(&log.display().to_string())
    );
    std::fs::write(&plist, content)?;
    let uid = unsafe { libc::getuid() };
    let target = format!("gui/{uid}");
    let _ = run(Command::new("launchctl").args(["bootout", &target, &plist.display().to_string()]));
    stop_running();
    let (ok, out) = run(Command::new("launchctl").args(["bootstrap", &target, &plist.display().to_string()]));
    if !ok {
        // SSH logins have no gui domain; fall back to the user domain
        let target = format!("user/{uid}");
        let (ok2, out2) =
            run(Command::new("launchctl").args(["bootstrap", &target, &plist.display().to_string()]));
        if !ok2 {
            return Err(anyhow!("launchctl bootstrap 失败: {out} / {out2}"));
        }
    }
    Ok(format!("已注册 LaunchAgent {LABEL}"))
}

#[cfg(target_os = "macos")]
fn platform_uninstall() -> Result<String> {
    let plist = crate::paths::home()
        .join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist"));
    let uid = unsafe { libc::getuid() };
    for d in ["gui", "user"] {
        let _ = run(Command::new("launchctl").args([
            "bootout",
            &format!("{d}/{uid}"),
            &plist.display().to_string(),
        ]));
    }
    let _ = std::fs::remove_file(plist);
    stop_running();
    Ok("已移除 LaunchAgent".into())
}

#[cfg(target_os = "macos")]
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

#[cfg(windows)]
const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

#[cfg(windows)]
fn platform_install(exe: &Path) -> Result<String> {
    let value = format!("\"{}\" serve", exe.display());
    let (ok, out) = run(Command::new("reg").args([
        "add", RUN_KEY, "/v", "YiConsole", "/t", "REG_SZ", "/d", &value, "/f",
    ]));
    if !ok {
        return Err(anyhow!("写注册表失败: {out}"));
    }
    crate::spawn_detached()?;
    Ok("已加入登录自启（HKCU Run），并已在后台启动".into())
}

#[cfg(windows)]
fn platform_uninstall() -> Result<String> {
    let _ = run(Command::new("reg").args(["delete", RUN_KEY, "/v", "YiConsole", "/f"]));
    stop_running();
    Ok("已移除登录自启".into())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn platform_install(_exe: &Path) -> Result<String> {
    Err(anyhow!("当前平台不支持安装服务，请手动运行 `yi-agentd serve`"))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn platform_uninstall() -> Result<String> {
    Ok(String::new())
}

/// Ask the running instance to exit (sends daemon.shutdown over IPC)
pub fn stop_running() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build();
    let Ok(rt) = rt else { return };
    rt.block_on(async {
        use tokio::io::AsyncWriteExt;
        if let Ok(mut c) = crate::transport::connect().await {
            let _ = c
                .write_all(b"{\"id\":1,\"method\":\"daemon.shutdown\",\"params\":{}}\n")
                .await;
            let _ = c.flush().await;
            tokio::time::sleep(std::time::Duration::from_millis(800)).await;
        }
    });
}

pub fn status() -> String {
    let running = {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async { crate::transport::connect().await.is_ok() })
    };
    let mut s = format!(
        "yi-agentd {}：{}\n数据目录：{}",
        yi_proto::DAEMON_VERSION,
        if running { "运行中" } else { "未运行" },
        crate::paths::base_dir().display()
    );
    #[cfg(target_os = "linux")]
    {
        let (_, out) = run(Command::new("systemctl").args(["--user", "is-enabled", UNIT]));
        s.push_str(&format!("\nsystemd 服务：{out}"));
    }
    let relay = crate::relay::load_config();
    if let Some(r) = relay.filter(|r| r.enabled) {
        s.push_str(&format!("\n中继：{}（设备 {}，已配对 {} 台）", r.url, r.device_id(), r.clients.len()));
    }
    s
}
