//! Terminal mode: run any CLI in a PTY and pass the output as is to the client's xterm.js.

use super::{AdapterCmd, StartSpec};
use crate::hub::Emitter;
use yi_proto::{SessionStatus, ToolKind};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::io::{Read, Write};
use tokio::sync::mpsc;

/// Launch arguments for each tool's interactive mode (including resume)
pub fn interactive_args(spec: &StartSpec) -> Vec<String> {
    let mut a: Vec<String> = vec![];
    let model = spec.model.as_ref().filter(|m| !m.is_empty());
    match spec.tool {
        ToolKind::Claude => {
            if let Some(r) = &spec.resume {
                a.extend(["--resume".into(), r.clone()]);
            } else if let Some(id) = &spec.session_id {
                a.extend(["--session-id".into(), id.clone()]);
            }
            if let Some(m) = model {
                a.extend(["--model".into(), m.clone()]);
            }
            if let Some(e) = spec.effort.as_ref().filter(|e| !e.is_empty()) {
                a.extend(["--effort".into(), e.clone()]);
            }
            match spec.permission_mode.as_str() {
                "" | "default" => {}
                "bypass" => a.push("--dangerously-skip-permissions".into()),
                m => a.extend(["--permission-mode".into(), m.into()]),
            }
        }
        ToolKind::Codex => {
            if let Some(r) = &spec.resume {
                a.extend(["resume".into(), r.clone()]);
            }
            if let Some(m) = model {
                a.extend(["--model".into(), m.clone()]);
            }
            match spec.permission_mode.as_str() {
                "bypass" => a.push("--dangerously-bypass-approvals-and-sandbox".into()),
                "auto" => a.push("--approve-for-me".into()),
                "plan" => a.extend(["--sandbox".into(), "read-only".into(), "--ask-for-approval".into(), "untrusted".into()]),
                _ => {}
            }
            if let Some(e) = spec.effort.as_ref().filter(|e| !e.is_empty()) {
                a.extend(["-c".into(), format!("model_reasoning_effort={e}")]);
            }
        }
        ToolKind::Cursor => {
            if let Some(r) = &spec.resume {
                a.extend(["--resume".into(), r.clone()]);
            }
            // Don't pass --model: chat-mode (ACP) model ids carry parameters (grok-4.7[reasoning_effort=high,fast=true]),
            // and the interactive UI often reports "Cannot use this model" on Windows. Without it, Cursor uses its own last choice
            // (selectedModel in ~/.cursor/cli-config.json, shared with ACP), and /model still works in the terminal
            if spec.permission_mode == "bypass" {
                a.push("--force".into());
            }
            if spec.permission_mode == "plan" {
                a.push("--plan".into());
            }
        }
        ToolKind::Kiro => {
            a.push("chat".into());
            if let Some(r) = &spec.resume {
                a.extend(["--resume-id".into(), r.clone()]);
            }
            if let Some(m) = model {
                a.extend(["--model".into(), m.clone()]);
            }
            if spec.permission_mode == "bypass" {
                a.push("--trust-all-tools".into());
            }
        }
        ToolKind::Qoder | ToolKind::Codebuddy => {
            if let Some(r) = &spec.resume {
                a.extend(["--resume".into(), r.clone()]);
            } else if let Some(id) = &spec.session_id {
                a.extend(["--session-id".into(), id.clone()]);
            }
            if let Some(m) = model {
                a.extend(["--model".into(), m.clone()]);
            }
            let qoder = spec.tool == ToolKind::Qoder;
            match spec.permission_mode.as_str() {
                "bypass" if qoder => a.push("--dangerously-skip-permissions".into()),
                "bypass" => a.extend(["--permission-mode".into(), "bypassPermissions".into()]),
                "acceptEdits" if qoder => a.extend(["--permission-mode".into(), "accept_edits".into()]),
                m @ ("acceptEdits" | "plan") => a.extend(["--permission-mode".into(), m.into()]),
                _ => {}
            }
        }
        ToolKind::Shell => {
            if let Some(c) = spec.command.as_ref().filter(|c| !c.trim().is_empty()) {
                if cfg!(windows) {
                    a.extend(["-NoLogo".into(), "-Command".into(), c.clone()]);
                } else {
                    a.extend(["-lc".into(), c.clone()]);
                }
            } else if !cfg!(windows) {
                a.push("-l".into());
            }
        }
        // Never launched (Hub::launch refuses); here only to keep the match exhaustive
        ToolKind::Files => {}
    }
    a.extend(spec.extra_args.iter().cloned());
    a
}

pub fn start(spec: StartSpec, em: Emitter, mut rx: mpsc::UnboundedReceiver<AdapterCmd>) {
    let pty = native_pty_system();
    let pair = match pty.openpty(PtySize {
        rows: spec.rows.max(5),
        cols: spec.cols.max(20),
        pixel_width: 0,
        pixel_height: 0,
    }) {
        Ok(p) => p,
        Err(e) => return em.exited(Some(format!("创建 PTY 失败: {e}"))),
    };

    let args = interactive_args(&spec);
    let program = spec.program.clone();
    let is_script = program
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("cmd") || e.eq_ignore_ascii_case("bat"))
        .unwrap_or(false);
    let tmux = spec.tmux.as_ref().and_then(|n| crate::tmux::program().map(|t| (t, n.clone())));
    let mut cmd = if let Some((t, name)) = &tmux {
        let (p, a) = crate::tmux::wrap(
            t,
            name,
            &spec.cwd.to_string_lossy(),
            &program.to_string_lossy(),
            &args,
            spec.cols.max(20),
            spec.rows.max(5),
        );
        let mut c = CommandBuilder::new(p);
        c.args(a);
        c
    } else if cfg!(windows) && is_script {
        let mut c = CommandBuilder::new("cmd");
        c.arg("/c");
        c.arg(&program);
        c
    } else {
        CommandBuilder::new(&program)
    };
    if tmux.is_none() {
        for a in &args {
            cmd.arg(a);
        }
    }
    cmd.cwd(&spec.cwd);
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");

    let mut child = match pair.slave.spawn_command(cmd) {
        Ok(c) => c,
        Err(e) => return em.exited(Some(format!("启动 {} 失败: {e}", program.display()))),
    };
    drop(pair.slave);
    let master = pair.master;
    let mut reader = match master.try_clone_reader() {
        Ok(r) => r,
        Err(e) => return em.exited(Some(format!("PTY 读取失败: {e}"))),
    };
    let mut writer = match master.take_writer() {
        Ok(w) => w,
        Err(e) => return em.exited(Some(format!("PTY 写入失败: {e}"))),
    };
    let mut killer = child.clone_killer();
    em.status(SessionStatus::Running, None);

    // Reading output: blocking thread
    let em_r = em.clone();
    let (reply_tx, mut reply_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buf = [0u8; 16 * 1024];
        // Windows ConPTY sends ESC[6n at startup to query the cursor position and doesn't continue until it gets an answer.
        // With no client watching it would hang forever; and the query left in the terminal buffer gets answered again by xterm on reconnect replay,
        // typing the answer into the shell as keyboard input. So the first one is answered here and not forwarded to clients.
        let mut startup_dsr = cfg!(windows);
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if startup_dsr {
                        if let Some(rest) = strip_first_dsr(&buf[..n]) {
                            startup_dsr = false;
                            let _ = reply_tx.send(b"\x1b[1;1R".to_vec());
                            if !rest.is_empty() {
                                em_r.pty_output(&rest);
                            }
                            continue;
                        }
                    }
                    em_r.pty_output(&buf[..n]);
                }
            }
        }
    });

    // Waiting for exit: blocking thread
    let em_w = em.clone();
    let (done_tx, mut done_rx) = tokio::sync::oneshot::channel::<()>();
    std::thread::spawn(move || {
        let status = child.wait();
        let msg = match status {
            Ok(s) if s.success() => None,
            Ok(s) => Some(format!("进程退出，退出码 {}", s.exit_code())),
            Err(e) => Some(format!("等待进程失败: {e}")),
        };
        // Give the reader thread a moment to push out the last output
        std::thread::sleep(std::time::Duration::from_millis(200));
        em_w.exited(msg);
        let _ = done_tx.send(());
    });

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut done_rx => break,
                Some(r) = reply_rx.recv() => {
                    let _ = writer.write_all(&r);
                    let _ = writer.flush();
                }
                cmd = rx.recv() => match cmd {
                    Some(AdapterCmd::PtyInput(data)) => {
                        let _ = writer.write_all(&data);
                        let _ = writer.flush();
                    }
                    Some(AdapterCmd::PtyResize { cols, rows }) => {
                        let _ = master.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
                    }
                    Some(AdapterCmd::Send(p)) => {
                        let _ = writer.write_all(p.text.as_bytes());
                        let _ = writer.write_all(b"\r");
                        let _ = writer.flush();
                    }
                    Some(AdapterCmd::Interrupt) => {
                        let _ = writer.write_all(&[3]);
                        let _ = writer.flush();
                    }
                    Some(AdapterCmd::Kill) => {
                        // Ended by the user: the program in tmux ends too
                        if let Some((_, name)) = &tmux {
                            crate::tmux::kill(name);
                        }
                        let _ = killer.kill();
                        break;
                    }
                    None => {
                        let _ = killer.kill();
                        break;
                    }
                    Some(AdapterCmd::Permission { .. } | AdapterCmd::SetModel(_) | AdapterCmd::SetEffort(_) | AdapterCmd::SetPermissionMode(_) | AdapterCmd::StopTask(_)) => {}
                }
            }
        }
    });
}

/// Strip the first cursor position query (ESC[6n) from the data; None if there isn't one
fn strip_first_dsr(data: &[u8]) -> Option<Vec<u8>> {
    const DSR: &[u8] = b"\x1b[6n";
    let i = data.windows(DSR.len()).position(|w| w == DSR)?;
    let mut out = data[..i].to_vec();
    out.extend_from_slice(&data[i + DSR.len()..]);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::strip_first_dsr;

    #[test]
    fn strips_only_first_dsr() {
        assert_eq!(strip_first_dsr(b"\x1b[6n"), Some(vec![]));
        assert_eq!(strip_first_dsr(b"a\x1b[6nb\x1b[6n").as_deref(), Some(&b"ab\x1b[6n"[..]));
        assert_eq!(strip_first_dsr(b"plain"), None);
    }
}
