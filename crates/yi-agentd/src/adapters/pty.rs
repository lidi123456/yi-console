//! Terminal mode: run any CLI in a PTY and pass the output as is to the client's xterm.js.

use super::{AdapterCmd, StartSpec};
use crate::hub::Emitter;
use yi_proto::{SessionStatus, ToolKind};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// Messages (`AdapterCmd::Send`) that arrive while the program is still starting are held until its output has been
/// quiet this long: a TUI typed into before it draws its input box drops or garbles the keys (the first message of a
/// new terminal-mode session, or a message that restarts an exited one)
const READY_QUIET: Duration = Duration::from_millis(1500);
/// Send anyway after this long, for programs that keep redrawing (spinners, clocks)
const READY_MAX: Duration = Duration::from_secs(20);
/// Pause between a bracketed paste and the Enter that submits it, so TUIs that time pastes don't take Enter as part of it
const PASTE_ENTER_DELAY: Duration = Duration::from_millis(150);

/// Whether the program is ready to be typed into, given the time since launch and since its last output
fn input_ready(since_start: Duration, since_output: Option<Duration>) -> bool {
    since_start >= READY_MAX || since_output.is_some_and(|q| q >= READY_QUIET && since_start >= READY_QUIET)
}

/// The bytes typing a message: as a bracketed paste when the program turned it on (multi-line text stays one message),
/// otherwise the raw text. Enter is sent separately
fn message_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    if !bracketed {
        return text.as_bytes().to_vec();
    }
    // A paste-end marker inside the text would end the paste early
    let clean = text.replace("\x1b[201~", "");
    let mut out = b"\x1b[200~".to_vec();
    out.extend_from_slice(clean.as_bytes());
    out.extend_from_slice(b"\x1b[201~");
    out
}

/// Tracks the latest bracketed-paste mode switch in the output (`ESC[?2004h` / `ESC[?2004l`); a switch split across
/// two reads is missed, which only costs the paste wrapping
fn scan_bracketed(data: &[u8], cur: bool) -> bool {
    const ON: &[u8] = b"\x1b[?2004h";
    const OFF: &[u8] = b"\x1b[?2004l";
    let last = |pat: &[u8]| data.windows(pat.len()).rposition(|w| w == pat);
    match (last(ON), last(OFF)) {
        (Some(a), Some(b)) => a > b,
        (Some(_), None) => true,
        (None, Some(_)) => false,
        (None, None) => cur,
    }
}

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
        ToolKind::Antigravity => {
            // No way to pin a conversation id on a new session; Hub::guess_terminal_session finds it afterwards
            if let Some(r) = &spec.resume {
                a.extend(["--conversation".into(), r.clone()]);
            }
            if let Some(m) = model {
                a.extend(["--model".into(), m.clone()]);
            }
            if let Some(e) = spec.effort.as_ref().filter(|e| !e.is_empty()) {
                a.extend(["--effort".into(), e.clone()]);
            }
            match spec.permission_mode.as_str() {
                "bypass" => a.push("--dangerously-skip-permissions".into()),
                "acceptEdits" => a.extend(["--mode".into(), "accept-edits".into()]),
                m @ ("auto" | "plan") => a.extend(["--mode".into(), m.into()]),
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
        let (p, a) = crate::tmux::wrap_env(
            t,
            name,
            &spec.cwd.to_string_lossy(),
            &program.to_string_lossy(),
            &args,
            spec.cols.max(20),
            spec.rows.max(5),
            &spec.env,
            &spec.env_unset,
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
    for k in &spec.env_unset {
        cmd.env_remove(k);
    }
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }

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

    // Kiro writes the pid of its `kiro-cli acp` process (a grandchild: kiro-cli chat -> bun tui.js -> kiro-cli acp) into
    // ~/.kiro/sessions/cli/<id>.lock; sampling the process tree lets the hub tell which Kiro session this process owns
    let sampling = Arc::new(AtomicBool::new(spec.tool == ToolKind::Kiro));
    if sampling.load(Ordering::Relaxed) {
        let (em_p, on) = (em.clone(), sampling.clone());
        let root = child.process_id();
        let pane = tmux.as_ref().map(|(_, n)| n.clone());
        std::thread::spawn(move || {
            let waits = [3u64, 3, 4, 10, 20, 40, 80];
            let mut i = 0;
            while on.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_secs(waits.get(i).copied().unwrap_or(120)));
                i += 1;
                if !on.load(Ordering::Relaxed) {
                    break;
                }
                let root = match &pane {
                    Some(n) => crate::tmux::pane_pid(n).map(|p| p as u32),
                    None => root,
                };
                if let Some(r) = root {
                    let mut pids = crate::proctree::descendants(r);
                    pids.push(r);
                    em_p.tool_pids(pids);
                }
            }
        });
    }

    // Reading output: blocking thread. It also records when output last came (ms since launch, 0 = none yet) and
    // whether the program has bracketed paste on, for sending messages
    let started = Instant::now();
    let last_out = Arc::new(AtomicU64::new(0));
    let bracketed = Arc::new(AtomicBool::new(false));
    let (last_out_r, bracketed_r) = (last_out.clone(), bracketed.clone());
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
                    last_out_r.store((started.elapsed().as_millis() as u64).max(1), Ordering::Relaxed);
                    let b = bracketed_r.load(Ordering::Relaxed);
                    bracketed_r.store(scan_bracketed(&buf[..n], b), Ordering::Relaxed);
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
        sampling.store(false, Ordering::Relaxed);
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
        // Messages waiting for the program to be ready (see READY_QUIET); once ready it stays ready
        let mut pending: VecDeque<String> = VecDeque::new();
        let mut ready = false;
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        loop {
            if !ready {
                let out = last_out.load(Ordering::Relaxed);
                let since = started.elapsed();
                ready = input_ready(since, (out > 0).then(|| since.saturating_sub(Duration::from_millis(out))));
            }
            if ready {
                while let Some(text) = pending.pop_front() {
                    let paste = bracketed.load(Ordering::Relaxed);
                    let _ = writer.write_all(&message_bytes(&text, paste));
                    let _ = writer.flush();
                    if paste {
                        tokio::time::sleep(PASTE_ENTER_DELAY).await;
                    }
                    let _ = writer.write_all(b"\r");
                    let _ = writer.flush();
                }
            }
            tokio::select! {
                _ = &mut done_rx => break,
                _ = tick.tick(), if !ready && !pending.is_empty() => {}
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
                    // Typed at the top of the loop, once the program is ready
                    Some(AdapterCmd::Send(p)) => pending.push_back(p.text),
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
                    Some(AdapterCmd::Permission { .. } | AdapterCmd::SetModel(_) | AdapterCmd::SetEffort(_) | AdapterCmd::SetModelConfig { .. } | AdapterCmd::SetPermissionMode(_) | AdapterCmd::StopTask(_)) => {}
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
    use super::{input_ready, message_bytes, scan_bracketed, strip_first_dsr};
    use std::time::Duration;

    #[test]
    fn ready_after_quiet_output_or_timeout() {
        let s = Duration::from_secs;
        let ms = Duration::from_millis;
        assert!(!input_ready(ms(500), None));
        assert!(!input_ready(s(5), None), "no output yet: keep waiting");
        assert!(!input_ready(s(5), Some(ms(300))), "still drawing");
        assert!(input_ready(s(5), Some(s(2))));
        assert!(!input_ready(ms(1000), Some(ms(1000))), "too soon after launch");
        assert!(input_ready(s(21), Some(ms(10))), "keeps redrawing: send anyway");
    }

    #[test]
    fn bracketed_paste() {
        assert_eq!(message_bytes("a\nb", false), b"a\nb");
        assert_eq!(message_bytes("a\nb", true), b"\x1b[200~a\nb\x1b[201~");
        assert_eq!(message_bytes("x\x1b[201~y", true), b"\x1b[200~xy\x1b[201~");
        assert!(scan_bracketed(b"..\x1b[?2004h..", false));
        assert!(!scan_bracketed(b"\x1b[?2004h\x1b[?2004l", true));
        assert!(scan_bracketed(b"\x1b[?2004l\x1b[?2004h", false));
        assert!(scan_bracketed(b"plain", true));
    }

    #[test]
    fn strips_only_first_dsr() {
        assert_eq!(strip_first_dsr(b"\x1b[6n"), Some(vec![]));
        assert_eq!(strip_first_dsr(b"a\x1b[6nb\x1b[6n").as_deref(), Some(&b"ab\x1b[6n"[..]));
        assert_eq!(strip_first_dsr(b"plain"), None);
    }
}
