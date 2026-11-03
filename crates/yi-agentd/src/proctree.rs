//! Kill a tool's whole process tree, not just the direct child.
//!
//! On Windows npm-installed tools are `.cmd` shims started as `cmd /c tool.cmd`, so the direct child is cmd.exe and the
//! real tool is a node.exe under it; `TerminateProcess` on cmd.exe leaves node.exe running as an orphan. Each child goes
//! into its own Job Object with KILL_ON_JOB_CLOSE: killing terminates the job, and closing the handle (drop, or the
//! daemon dying) takes the whole tree down too.
//! On Unix the child leads its own process group and we signal the group.

use tokio::process::{Child, Command};

/// Call before spawning
pub fn prepare(cmd: &mut Command) {
    #[cfg(unix)]
    cmd.process_group(0);
    #[cfg(not(unix))]
    let _ = cmd;
}

/// Owns the process tree of one child. Dropping it kills whatever is still running in the tree
pub struct Tree {
    #[cfg(windows)]
    job: isize,
    #[cfg(unix)]
    pgid: Option<i32>,
}

impl Tree {
    /// Call right after spawning
    pub fn attach(child: &Child) -> Tree {
        #[cfg(windows)]
        {
            Tree { job: win::job_for(child) }
        }
        #[cfg(unix)]
        {
            Tree { pgid: child.id().map(|p| p as i32) }
        }
    }

    /// Kill every process in the tree (the direct child included)
    pub fn kill(&self) {
        #[cfg(windows)]
        if self.job != 0 {
            unsafe { windows_sys::Win32::System::JobObjects::TerminateJobObject(self.job as _, 1) };
        }
        #[cfg(unix)]
        if let Some(g) = self.pgid {
            unsafe { libc::killpg(g, libc::SIGKILL) };
        }
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        #[cfg(windows)]
        if self.job != 0 {
            // KILL_ON_JOB_CLOSE: closing our only handle kills what's left in the job
            unsafe { windows_sys::Win32::Foundation::CloseHandle(self.job as _) };
        }
        #[cfg(unix)]
        self.kill();
    }
}

#[cfg(windows)]
mod win {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    /// Create a kill-on-close job holding the child; 0 if that fails (we then fall back to killing the child only)
    pub fn job_for(child: &tokio::process::Child) -> isize {
        let Some(proc) = child.raw_handle() else { return 0 };
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return 0;
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok = SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const _,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ) != 0
                && AssignProcessToJobObject(job, proc as _) != 0;
            if !ok {
                crate::warn!("进程无法加入作业对象，停止时可能残留子进程: {}", std::io::Error::last_os_error());
                CloseHandle(job);
                return 0;
            }
            job as isize
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;
    use tokio::io::{AsyncBufReadExt, BufReader};

    fn alive(pid: u32) -> bool {
        #[cfg(windows)]
        {
            let out = std::process::Command::new("tasklist")
                .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).contains(&format!("\"{pid}\""))
        }
        #[cfg(unix)]
        {
            // Reaped by init once its parent is gone; a zombie counts as dead
            std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .map(|s| !s.rsplit(')').next().unwrap_or("").trim_start().starts_with('Z'))
                .unwrap_or(false)
        }
    }

    /// The `cmd /c tool.cmd` -> node.exe shape: killing the tree must take the grandchild too
    #[tokio::test]
    async fn kill_takes_grandchildren() {
        #[cfg(windows)]
        let mut cmd = {
            let mut c = Command::new("cmd");
            // raw_arg: Rust's quoting (\") isn't understood by cmd
            c.raw_arg("/c powershell -NoProfile -Command \"$PID; Start-Sleep 60\"");
            c
        };
        #[cfg(unix)]
        let mut cmd = {
            let mut c = Command::new("sh");
            // The trailing `:` keeps the outer sh from exec'ing, so the sleeper really is a grandchild
            c.args(["-c", "sh -c 'echo $$; exec sleep 60'; :"]);
            c
        };
        cmd.stdin(Stdio::null()).stdout(Stdio::piped()).kill_on_drop(true);
        prepare(&mut cmd);
        let mut child = cmd.spawn().unwrap();
        let tree = Tree::attach(&child);
        let mut out = BufReader::new(child.stdout.take().unwrap()).lines();
        let gc: u32 = out.next_line().await.unwrap().unwrap().trim().parse().unwrap();
        assert_ne!(Some(gc), child.id());
        assert!(alive(gc));
        drop(tree);
        let _ = child.wait().await;
        for _ in 0..50 {
            if !alive(gc) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        panic!("grandchild {gc} survived");
    }
}
