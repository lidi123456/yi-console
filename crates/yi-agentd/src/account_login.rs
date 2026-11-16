//! In-app OAuth / device-code / interactive CLI login orchestrator.
//!
//! Spawns tool native login flows (`codex login --device-auth`, `agy acp --login`,
//! `claude auth login`, etc.), strips ANSI sequences, extracts OAuth URLs and device
//! codes for headless/remote users, accepts pasted callback URLs or codes via RPC,
//! and rolls back HostActive snapshots automatically on failure or cancellation.

use crate::accounts::{ensure_codex_profile_files, AccountManager, SnapshotBundle};
use anyhow::{anyhow, bail, Result};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use regex::Regex;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use yi_proto::{
    AccountAuthKind, AccountIsolation, AccountLoginUpdate, AccountProfileView, DaemonSettings,
    ToolKind,
};

pub struct StartLoginRequest {
    pub tool: ToolKind,
    pub mode: Option<String>,
    pub account_id: Option<String>,
    pub label: Option<String>,
}

pub type LoginEmitFn = Arc<dyn Fn(AccountLoginUpdate) + Send + Sync>;
pub type AccountsChangedFn = Arc<dyn Fn(ToolKind, Option<String>, bool) + Send + Sync>;

struct RunningHandles {
    cancel_fn: Option<Box<dyn FnOnce() + Send>>,
    stdin_tx: Option<mpsc::Sender<String>>,
    pty_master: Option<Box<dyn MasterPty + Send>>,
}

struct LoginJob {
    tool: ToolKind,
    target_account_id: Option<String>,
    staging_id: String,
    staging_dir: Option<PathBuf>,
    cleanup_dir_on_abort: bool,
    host_backup: Mutex<Option<SnapshotBundle>>,
    state: Mutex<AccountLoginUpdate>,
    handles: Mutex<RunningHandles>,
}

pub struct LoginManager {
    accounts: Arc<AccountManager>,
    jobs: Mutex<HashMap<String, Arc<LoginJob>>>,
}

impl LoginManager {
    pub fn new(accounts: Arc<AccountManager>) -> Self {
        Self {
            accounts,
            jobs: Mutex::new(HashMap::new()),
        }
    }

    pub fn active_jobs(&self, tool_filter: Option<ToolKind>) -> Vec<AccountLoginUpdate> {
        let g = self.jobs.lock().unwrap();
        g.values()
            .filter(|j| tool_filter.map_or(true, |t| t == j.tool))
            .map(|j| j.state.lock().unwrap().clone())
            .collect()
    }

    pub fn start(
        self: &Arc<Self>,
        req: StartLoginRequest,
        settings: &DaemonSettings,
        emit: LoginEmitFn,
        on_changed: AccountsChangedFn,
    ) -> Result<AccountLoginUpdate> {
        if !req.tool.supports_accounts() {
            bail!("该工具不支持账号登录");
        }
        let program = crate::tools::resolve(req.tool, settings)
            .ok_or_else(|| anyhow!("未找到 {} 可执行文件", req.tool.as_str()))?;
        let isolation = AccountManager::effective_isolation(req.tool, AccountAuthKind::Oauth);

        // HostActive tools swap live host credentials, so disallow overlapping logins per tool.
        if isolation == AccountIsolation::HostActive {
            let g = self.jobs.lock().unwrap();
            if g.values()
                .any(|j| j.tool == req.tool && j.state.lock().unwrap().running)
            {
                bail!("{} 已有正在进行的登录任务，请先完成或取消", req.tool.as_str());
            }
        }

        let mode = default_mode(req.tool, req.mode.as_deref());
        let job_id = format!("login_{}", AccountManager::new_account_id());
        let staging_id = req
            .account_id
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(AccountManager::new_account_id);
        let cleanup_dir_on_abort = req.account_id.is_none();

        let (staging_dir, host_backup) = if isolation == AccountIsolation::Isolated {
            let dir = self.accounts.profile_home(req.tool, &staging_id);
            std::fs::create_dir_all(&dir)?;
            if req.tool == ToolKind::Codex {
                ensure_codex_profile_files(&dir, None)?;
            }
            (Some(dir), None)
        } else {
            let snap = self.accounts.take_host_snapshot(req.tool)?;
            // Preserve live host auth in backup and clear live slots so CLI always starts a fresh login.
            self.accounts.clear_host_live(req.tool)?;
            (None, Some(snap))
        };

        let init_state = AccountLoginUpdate {
            job_id: job_id.clone(),
            tool: req.tool,
            account_id: req.account_id.clone(),
            mode: mode.clone(),
            running: true,
            ok: None,
            url: None,
            code: None,
            prompt: None,
            needs_input: false,
            output: Vec::new(),
            error: None,
        };

        let job = Arc::new(LoginJob {
            tool: req.tool,
            target_account_id: req.account_id.clone(),
            staging_id,
            staging_dir,
            cleanup_dir_on_abort,
            host_backup: Mutex::new(host_backup),
            state: Mutex::new(init_state.clone()),
            handles: Mutex::new(RunningHandles {
                cancel_fn: None,
                stdin_tx: None,
                pty_master: None,
            }),
        });

        self.jobs.lock().unwrap().insert(job_id.clone(), job.clone());
        emit(init_state.clone());

        let mgr = self.clone();
        let label = req.label.clone();
        std::thread::spawn(move || {
            mgr.run_job(job, program, mode, label, emit, on_changed);
        });

        Ok(init_state)
    }

    pub fn send_input(&self, job_id: &str, input: &str, emit: LoginEmitFn) -> Result<AccountLoginUpdate> {
        let job = self
            .jobs
            .lock()
            .unwrap()
            .get(job_id)
            .cloned()
            .ok_or_else(|| anyhow!("登录任务 {job_id} 不存在或已结束"))?;
        let tx = job
            .handles
            .lock()
            .unwrap()
            .stdin_tx
            .clone()
            .ok_or_else(|| anyhow!("该登录任务不支持文本输入或已关闭"))?;
        tx.send(input.to_string())
            .map_err(|_| anyhow!("登录进程已关闭输入通道"))?;
        let updated = {
            let mut st = job.state.lock().unwrap();
            st.needs_input = false;
            st.clone()
        };
        emit(updated.clone());
        Ok(updated)
    }

    pub fn cancel(&self, job_id: &str, emit: LoginEmitFn) -> Result<AccountLoginUpdate> {
        let job = self
            .jobs
            .lock()
            .unwrap()
            .get(job_id)
            .cloned()
            .ok_or_else(|| anyhow!("登录任务 {job_id} 不存在"))?;
        let cancel_fn = job.handles.lock().unwrap().cancel_fn.take();
        if let Some(f) = cancel_fn {
            f();
        }
        self.restore_on_abort(&job);
        let updated = {
            let mut st = job.state.lock().unwrap();
            if st.running {
                st.running = false;
                st.ok = Some(false);
                st.needs_input = false;
                st.error = Some("登录已取消".to_string());
            }
            st.clone()
        };
        emit(updated.clone());
        Ok(updated)
    }

    fn restore_on_abort(&self, job: &LoginJob) {
        if let Some(backup) = job.host_backup.lock().unwrap().take() {
            let _ = self.accounts.restore_host_snapshot(job.tool, &backup);
        }
        if job.cleanup_dir_on_abort {
            let dir = self.accounts.account_dir(job.tool, &job.staging_id);
            if dir.exists() {
                let _ = std::fs::remove_dir_all(&dir);
            }
        }
    }

    fn run_job(
        &self,
        job: Arc<LoginJob>,
        program: PathBuf,
        mode: String,
        label: Option<String>,
        emit: LoginEmitFn,
        on_changed: AccountsChangedFn,
    ) {
        let res = match (job.tool, mode.as_str()) {
            (ToolKind::Codex, "device_auth") => self.run_piped(
                &job,
                &program,
                &["login", "--device-auth"],
                &emit,
            ),
            (ToolKind::Antigravity, _) => self.run_piped(
                &job,
                &program,
                &["acp", "--login"],
                &emit,
            ),
            (ToolKind::Cursor, _) => self.run_piped(&job, &program, &["login"], &emit),
            (ToolKind::Codex, _) => self.run_pty(&job, &program, &["login"], &emit),
            (ToolKind::Claude, _) => self.run_pty(&job, &program, &["auth", "login"], &emit),
            _ => self.run_pty(&job, &program, &["login"], &emit),
        };

        // Ignore post-cancel completion races cleanly.
        if !job.state.lock().unwrap().running {
            return;
        }

        match res {
            Ok(()) => {
                match self.finalize_success(&job, label.as_deref()) {
                    Ok((view, host_switched)) => {
                        *job.host_backup.lock().unwrap() = None;
                        let acc_id = view.id.clone();
                        let updated = {
                            let mut st = job.state.lock().unwrap();
                            st.running = false;
                            st.ok = Some(true);
                            st.needs_input = false;
                            st.account_id = Some(acc_id.clone());
                            st.clone()
                        };
                        emit(updated);
                        on_changed(job.tool, Some(acc_id), host_switched);
                    }
                    Err(err) => {
                        self.restore_on_abort(&job);
                        let updated = {
                            let mut st = job.state.lock().unwrap();
                            st.running = false;
                            st.ok = Some(false);
                            st.needs_input = false;
                            st.error = Some(err.to_string());
                            st.clone()
                        };
                        emit(updated);
                    }
                }
            }
            Err(err) => {
                self.restore_on_abort(&job);
                let updated = {
                    let mut st = job.state.lock().unwrap();
                    if st.running {
                        st.running = false;
                        st.ok = Some(false);
                        st.needs_input = false;
                        st.error = Some(err.to_string());
                    }
                    st.clone()
                };
                emit(updated);
            }
        }
    }

    fn finalize_success(
        &self,
        job: &LoginJob,
        label: Option<&str>,
    ) -> Result<(AccountProfileView, bool)> {
        let isolation = AccountManager::effective_isolation(job.tool, AccountAuthKind::Oauth);
        if isolation == AccountIsolation::Isolated {
            let dir = job
                .staging_dir
                .as_ref()
                .ok_or_else(|| anyhow!("缺少账号凭据目录"))?;
            let has_creds = match job.tool {
                ToolKind::Codex => dir.join("auth.json").is_file(),
                ToolKind::Claude => dir.join(".credentials.json").is_file(),
                _ => true,
            };
            if !has_creds {
                bail!("登录进程结束，但未生成凭据文件");
            }
            let target_id = job
                .target_account_id
                .as_deref()
                .unwrap_or(job.staging_id.as_str());
            let view = self.accounts.save_oauth_from_dir(
                job.tool,
                Some(target_id),
                label,
                Some(dir),
                None,
                false,
            )?;
            Ok((view, false))
        } else {
            let snap = self.accounts.take_host_snapshot(job.tool)?;
            if snap.is_empty() {
                bail!("登录进程结束，但系统未保存任何新凭据");
            }
            if job.tool == ToolKind::Cursor && crate::accounts::cursor_snapshot_is_api_key(&snap) {
                bail!("登录进程未生成网页授权凭据，请重试");
            }
            let view = self.accounts.save_oauth_from_dir(
                job.tool,
                job.target_account_id.as_deref(),
                label,
                None,
                Some(snap),
                true,
            )?;
            Ok((view, true))
        }
    }

    fn resolve_env(&self, job: &LoginJob) -> (Vec<(&'static str, String)>, Vec<&'static str>) {
        let mut sets = Vec::new();
        let mut unsets = Vec::new();
        match job.tool {
            ToolKind::Codex => {
                if let Some(dir) = &job.staging_dir {
                    sets.push(("CODEX_HOME", dir.display().to_string()));
                }
                unsets.extend(["OPENAI_API_KEY", "OPENAI_BASE_URL"]);
            }
            ToolKind::Claude => {
                if let Some(dir) = &job.staging_dir {
                    sets.push(("CLAUDE_CONFIG_DIR", dir.display().to_string()));
                }
                unsets.extend(["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_BASE_URL"]);
            }
            ToolKind::Antigravity => {
                unsets.extend(["GEMINI_API_KEY", "GOOGLE_API_KEY"]);
            }
            ToolKind::Cursor => {
                sets.push(("NO_OPEN_BROWSER", "1".into()));
                unsets.extend([
                    "CURSOR_API_KEY",
                    "CURSOR_AUTH_TOKEN",
                    "AGENT_CLI_CREDENTIAL_STORE",
                ]);
            }
            _ => {}
        }
        (sets, unsets)
    }

    fn run_piped(
        &self,
        job: &Arc<LoginJob>,
        program: &PathBuf,
        args: &[&str],
        emit: &LoginEmitFn,
    ) -> Result<()> {
        let mut cmd = Command::new(program);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let (sets, unsets) = self.resolve_env(job);
        for (k, v) in sets {
            cmd.env(k, v);
        }
        for k in unsets {
            cmd.env_remove(k);
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            unsafe {
                cmd.pre_exec(|| {
                    libc::setpgid(0, 0);
                    Ok(())
                });
            }
        }
        let mut child = cmd.spawn()?;
        #[cfg(unix)]
        let pid = child.id();

        let (tx, rx) = mpsc::channel::<String>();
        let stdin = child.stdin.take();
        if let Some(mut sin) = stdin {
            std::thread::spawn(move || {
                while let Ok(line) = rx.recv() {
                    let trimmed = line.trim_end_matches(['\r', '\n']);
                    if sin.write_all(trimmed.as_bytes()).is_err()
                        || sin.write_all(b"\n").is_err()
                        || sin.flush().is_err()
                    {
                        break;
                    }
                }
            });
        }

        let child_arc: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        *child_arc.lock().unwrap() = Some(child);

        {
            let mut h = job.handles.lock().unwrap();
            h.stdin_tx = Some(tx);
            let ca = child_arc.clone();
            h.cancel_fn = Some(Box::new(move || {
                #[cfg(unix)]
                unsafe {
                    libc::killpg(pid as i32, libc::SIGKILL);
                }
                if let Some(ref mut c) = *ca.lock().unwrap() {
                    let _ = c.kill();
                }
            }));
        }

        let t1 = stdout.map(|r| {
            let j = job.clone();
            let e = emit.clone();
            std::thread::spawn(move || read_stream_chunks(r, &j, &e))
        });
        let t2 = stderr.map(|r| {
            let j = job.clone();
            let e = emit.clone();
            std::thread::spawn(move || read_stream_chunks(r, &j, &e))
        });

        let status = loop {
            let poll = {
                let mut g = child_arc.lock().unwrap();
                match g.as_mut() {
                    Some(c) => c.try_wait(),
                    None => break Err(anyhow!("登录进程已终止")),
                }
            };
            match poll {
                Ok(Some(st)) => break Ok(st),
                Ok(None) => std::thread::sleep(std::time::Duration::from_millis(80)),
                Err(e) => break Err(e.into()),
            }
        };

        if let Some(t) = t1 {
            let _ = t.join();
        }
        if let Some(t) = t2 {
            let _ = t.join();
        }

        let st = status?;
        if !st.success() {
            let hint = job
                .state
                .lock()
                .unwrap()
                .output
                .last()
                .cloned()
                .unwrap_or_else(|| format!("登录命令退出码 {}", st.code().unwrap_or(-1)));
            bail!("{hint}");
        }
        Ok(())
    }

    fn run_pty(
        &self,
        job: &Arc<LoginJob>,
        program: &PathBuf,
        args: &[&str],
        emit: &LoginEmitFn,
    ) -> Result<()> {
        let pty = native_pty_system();
        let pair = pty.openpty(PtySize {
            rows: 30,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let mut cmd = CommandBuilder::new(program);
        for a in args {
            cmd.arg(a);
        }
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        let (sets, unsets) = self.resolve_env(job);
        for (k, v) in sets {
            cmd.env(k, v);
        }
        for k in unsets {
            cmd.env_remove(k);
        }
        let mut child = pair.slave.spawn_command(cmd)?;
        drop(pair.slave);

        let reader = pair.master.try_clone_reader()?;
        let mut writer = pair.master.take_writer()?;
        let (tx, rx) = mpsc::channel::<String>();
        std::thread::spawn(move || {
            while let Ok(line) = rx.recv() {
                if writer.write_all(line.as_bytes()).is_err()
                    || writer.write_all(b"\r").is_err()
                    || writer.flush().is_err()
                {
                    break;
                }
            }
        });

        let killer = Arc::new(Mutex::new(Some(child.clone_killer())));
        {
            let mut h = job.handles.lock().unwrap();
            h.stdin_tx = Some(tx);
            h.pty_master = Some(pair.master);
            let k = killer.clone();
            h.cancel_fn = Some(Box::new(move || {
                if let Some(mut kill) = k.lock().unwrap().take() {
                    let _ = kill.kill();
                }
            }));
        }

        let j_r = job.clone();
        let e_r = emit.clone();
        let t = std::thread::spawn(move || read_stream_chunks(reader, &j_r, &e_r));

        let status = child.wait()?;
        drop(job.handles.lock().unwrap().pty_master.take());
        let _ = t.join();
        if !status.success() {
            let hint = job
                .state
                .lock()
                .unwrap()
                .output
                .last()
                .cloned()
                .unwrap_or_else(|| format!("登录命令退出码 {}", status.exit_code()));
            bail!("{hint}");
        }
        Ok(())
    }
}

fn default_mode(tool: ToolKind, req: Option<&str>) -> String {
    if let Some(m) = req.filter(|s| !s.trim().is_empty()) {
        return m.to_string();
    }
    match tool {
        ToolKind::Codex => "device_auth".into(),
        ToolKind::Antigravity => "oauth".into(),
        _ => "interactive".into(),
    }
}

fn read_stream_chunks<R: Read>(mut reader: R, job: &LoginJob, emit: &LoginEmitFn) {
    let mut buf = [0u8; 2048];
    let mut pending = String::new();
    loop {
        match reader.read(&mut buf) {
            Ok(0) => {
                if !pending.trim().is_empty() {
                    ingest_line(job, &pending, emit);
                }
                break;
            }
            Ok(n) => {
                let chunk = String::from_utf8_lossy(&buf[..n]);
                pending.push_str(&chunk);
                while let Some(pos) = pending.find('\n') {
                    let line = pending[..pos].to_string();
                    pending.drain(..=pos);
                    ingest_line(job, &line, emit);
                }
                // Flush prompt lines that wait for inline stdin without a trailing newline.
                let trimmed = pending.trim_end();
                if trimmed.ends_with(':') || trimmed.ends_with('?') {
                    let line = std::mem::take(&mut pending);
                    ingest_line(job, &line, emit);
                }
            }
            Err(_) => break,
        }
    }
}

fn ingest_line(job: &LoginJob, raw_line: &str, emit: &LoginEmitFn) {
    let clean = strip_ansi(raw_line).trim().to_string();
    if clean.is_empty() {
        return;
    }
    let updated = {
        let mut st = job.state.lock().unwrap();
        if st.output.len() >= 100 {
            st.output.remove(0);
        }
        st.output.push(clean.clone());
        if let Some(u) = extract_url(&clean) {
            st.url = Some(u);
        }
        if let Some(c) = extract_device_code(&clean) {
            st.code = Some(c);
        }
        let low = clean.to_ascii_lowercase();
        if low.contains("paste")
            || low.contains("enter code")
            || low.contains("authorization code")
            || low.contains("press enter")
        {
            st.needs_input = true;
            st.prompt = Some(clean);
        }
        st.clone()
    };
    emit(updated);
}

fn strip_ansi(s: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\r").unwrap()
    });
    re.replace_all(s, "").into_owned()
}

fn extract_url(line: &str) -> Option<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"https?://[^\s<>]+").unwrap());
    let m = re.find(line)?;
    Some(m.as_str().trim_end_matches([',', '.', ')', ']']).to_string())
}

fn extract_device_code(line: &str) -> Option<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    // Codex device-auth uses uppercase alphanumeric segments like ABCD-12345.
    let re = RE.get_or_init(|| Regex::new(r"\b([A-Z0-9]{4,5}-[A-Z0-9]{4,6})\b").unwrap());
    if line.contains("http://") || line.contains("https://") {
        return None;
    }
    for cap in re.captures_iter(line) {
        let token = &cap[1];
        if token.bytes().any(|b| b.is_ascii_uppercase()) {
            return Some(token.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_codex_device_url_and_code() {
        let sample = "\x1b[32m  1. Open this link in your browser:\x1b[0m\n     https://auth.openai.com/codex/device\n  2. Enter this one-time code:\n     W4K9-P2X8M\n";
        let mut url = None;
        let mut code = None;
        for line in sample.lines() {
            let clean = strip_ansi(line).trim().to_string();
            if url.is_none() {
                url = extract_url(&clean);
            }
            if code.is_none() {
                code = extract_device_code(&clean);
            }
        }
        assert_eq!(url.as_deref(), Some("https://auth.openai.com/codex/device"));
        assert_eq!(code.as_deref(), Some("W4K9-P2X8M"));
        // Plain dates must not be misidentified as device codes.
        assert_eq!(extract_device_code("2026-1115"), None);
    }

    #[test]
    fn extracts_agy_google_oauth_url() {
        let line = "  https://accounts.google.com/o/oauth2/auth?client_id=123&redirect_uri=https%3A%2F%2Fantigravity.google%2Foauth-callback";
        assert_eq!(
            extract_url(line).as_deref(),
            Some("https://accounts.google.com/o/oauth2/auth?client_id=123&redirect_uri=https%3A%2F%2Fantigravity.google%2Foauth-callback")
        );
    }
}
