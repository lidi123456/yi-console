//! git operations for summarizing session changes: which files changed and their diffs, discarding changes, committing.

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::path::Path;
use std::process::{Command, Stdio};

/// Size limit for a single file's diff; larger ones just get a notice
const DIFF_MAX: usize = 200 * 1024;

fn git(cwd: &Path, args: &[&str]) -> Result<std::process::Output> {
    let mut cmd = Command::new("git");
    cmd.args(["-c", "core.quotepath=false"]).args(args).current_dir(cwd);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    cmd.stdin(Stdio::null())
        .output()
        .map_err(|e| anyhow!("运行 git 失败（这台机器装了 git 吗）: {e}"))
}

fn git_ok(cwd: &Path, args: &[&str]) -> Result<String> {
    let out = git(cwd, args)?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Repository root; None if not a git repo
fn repo_root(cwd: &Path) -> Option<std::path::PathBuf> {
    git_ok(cwd, &["rev-parse", "--show-toplevel"])
        .ok()
        .map(|s| std::path::PathBuf::from(s.trim()))
}

/// Paths must stay inside the repo, so things like ../ can't escape it
fn check_paths(paths: &[String]) -> Result<()> {
    for p in paths {
        if p.is_empty() || p.starts_with('-') || Path::new(p).is_absolute() || p.split(['/', '\\']).any(|c| c == "..") {
            bail!("路径无效: {p}");
        }
    }
    Ok(())
}

/// Working tree changes relative to HEAD. Paths are relative to the repo root
pub fn changes(cwd: &str) -> Result<Value> {
    changes_with(cwd, true, None)
}

/// with_diff=false only lists statuses without diffs (running git diff per file is slow in a big repo with many changes);
/// if only is given, return just those paths (relative to the repo root), used to fetch one file's diff
pub fn changes_with(cwd: &str, with_diff: bool, only: Option<&[String]>) -> Result<Value> {
    let dir = crate::paths::expand(cwd);
    let Some(root) = repo_root(&dir) else {
        return Ok(json!({ "repo": null, "files": [] }));
    };
    let branch = git_ok(&root, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_default();
    let status = git_ok(&root, &["status", "--porcelain=v1", "-z", "-uall"])?;
    let stats = numstat(&root);
    let mut files = vec![];
    let mut it = status.split('\0').filter(|s| !s.is_empty());
    while let Some(entry) = it.next() {
        if entry.len() < 4 {
            continue;
        }
        let (code, path) = entry.split_at(3);
        let code = code.trim().to_string();
        // Rename entries are followed by the original path
        if code.starts_with('R') || code.starts_with('C') {
            it.next();
        }
        if only.is_some_and(|o| !o.iter().any(|x| x == path)) {
            continue;
        }
        let untracked = code == "??";
        let diff = if !with_diff {
            String::new()
        } else if untracked {
            let full = root.join(path);
            match std::fs::read(&full) {
                Ok(b) if b.len() > DIFF_MAX => String::new(),
                Ok(b) if b.iter().take(8000).any(|&c| c == 0) => String::new(),
                Ok(b) => {
                    let text = String::from_utf8_lossy(&b);
                    let n = text.lines().count();
                    let mut d = format!("--- /dev/null\n+++ b/{path}\n@@ -0,0 +1,{n} @@\n");
                    for l in text.lines() {
                        d.push('+');
                        d.push_str(l);
                        d.push('\n');
                    }
                    d
                }
                Err(_) => String::new(),
            }
        } else {
            let d = git_ok(&root, &["diff", "HEAD", "--no-color", "--", path]).unwrap_or_default();
            if d.len() > DIFF_MAX {
                String::new()
            } else {
                d
            }
        };
        // Lines added / removed: null for binaries, files too large, and repos without HEAD
        let (added, deleted) = if untracked {
            (count_lines(&root.join(path)), Some(0))
        } else {
            stats.get(path).copied().unwrap_or((None, None))
        };
        files.push(json!({
            "path": path,
            "status": if untracked { "new".to_string() } else { code },
            "diff": diff,
            "added": added,
            "deleted": deleted,
        }));
    }
    Ok(json!({
        "repo": root.display().to_string(),
        "branch": branch.trim(),
        "files": files,
    }))
}

/// Lines added/removed relative to HEAD for tracked files, computed in one git diff --numstat (no per-file diff runs);
/// git reports "-" for binary files, recorded as None
fn numstat(root: &Path) -> std::collections::HashMap<String, (Option<u64>, Option<u64>)> {
    let mut m = std::collections::HashMap::new();
    let Ok(out) = git_ok(root, &["diff", "HEAD", "--numstat", "-z", "--no-renames"]) else {
        return m;
    };
    for rec in out.split('\0').filter(|s| !s.is_empty()) {
        let mut it = rec.splitn(3, '\t');
        let (Some(a), Some(d), Some(path)) = (it.next(), it.next(), it.next()) else {
            continue;
        };
        m.insert(path.to_string(), (a.parse().ok(), d.parse().ok()));
    }
    m
}

/// Line count of a new file; not counted for files that are too large or binary
fn count_lines(full: &Path) -> Option<u64> {
    let meta = std::fs::metadata(full).ok()?;
    if !meta.is_file() || meta.len() as usize > DIFF_MAX {
        return None;
    }
    let b = std::fs::read(full).ok()?;
    if b.iter().take(8000).any(|&c| c == 0) {
        return None;
    }
    let n = b.iter().filter(|&&c| c == b'\n').count() + usize::from(b.last().is_some_and(|&c| c != b'\n'));
    Some(n as u64)
}

/// Discard changes: tracked files are restored to HEAD, new files are deleted
pub fn discard(cwd: &str, paths: &[String]) -> Result<()> {
    check_paths(paths)?;
    let dir = crate::paths::expand(cwd);
    let root = repo_root(&dir).ok_or_else(|| anyhow!("不是 git 仓库"))?;
    for p in paths {
        let tracked = git(&root, &["ls-files", "--error-unmatch", "--", p])?.status.success();
        let in_head = git(&root, &["cat-file", "-e", &format!("HEAD:{p}")])?.status.success();
        if in_head {
            git_ok(&root, &["checkout", "HEAD", "--", p])?;
        } else {
            if tracked {
                git_ok(&root, &["rm", "--cached", "-q", "--", p])?;
            }
            let full = root.join(p);
            if full.is_file() {
                std::fs::remove_file(&full)?;
            }
        }
    }
    Ok(())
}

/// Commit only the selected files (leaving everything else in the index alone)
pub fn commit(cwd: &str, paths: &[String], message: &str) -> Result<Value> {
    check_paths(paths)?;
    if paths.is_empty() {
        bail!("没有选文件");
    }
    if message.trim().is_empty() {
        bail!("提交说明不能为空");
    }
    let dir = crate::paths::expand(cwd);
    let root = repo_root(&dir).ok_or_else(|| anyhow!("不是 git 仓库"))?;
    let mut add = vec!["add", "-A", "--"];
    add.extend(paths.iter().map(String::as_str));
    git_ok(&root, &add)?;
    let mut c = vec!["commit", "-m", message.trim(), "--"];
    c.extend(paths.iter().map(String::as_str));
    git_ok(&root, &c)?;
    let head = git_ok(&root, &["rev-parse", "--short", "HEAD"])?;
    Ok(json!({ "commit": head.trim() }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changes_discard_commit() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path();
        let run = |a: &[&str]| assert!(git(p, a).unwrap().status.success(), "{a:?}");
        run(&["init", "-q"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(p.join("a.txt"), "1\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-qm", "init"]);
        std::fs::write(p.join("a.txt"), "2\n").unwrap();
        std::fs::write(p.join("b.txt"), "new\n").unwrap();
        let cwd = p.display().to_string();
        let c = changes(&cwd).unwrap();
        let files = c["files"].as_array().unwrap();
        assert_eq!(files.len(), 2);
        assert!(files.iter().any(|f| f["path"] == "b.txt" && f["status"] == "new"));
        let fast = changes_with(&cwd, false, None).unwrap();
        let fast = fast["files"].as_array().unwrap();
        assert!(fast.iter().all(|f| f["diff"] == ""));
        // Line counts come back even without diffs: a.txt changed one line (+1 -1), b.txt is a new one-line file
        let a = fast.iter().find(|f| f["path"] == "a.txt").unwrap();
        assert_eq!((a["added"].as_u64(), a["deleted"].as_u64()), (Some(1), Some(1)));
        let b = fast.iter().find(|f| f["path"] == "b.txt").unwrap();
        assert_eq!((b["added"].as_u64(), b["deleted"].as_u64()), (Some(1), Some(0)));
        let one = changes_with(&cwd, true, Some(&["a.txt".to_string()])).unwrap();
        let one = one["files"].as_array().unwrap();
        assert_eq!(one.len(), 1);
        assert!(one[0]["diff"].as_str().unwrap().contains("+2"));
        assert!(files.iter().any(|f| f["path"] == "a.txt" && f["diff"].as_str().unwrap().contains("+2")));
        commit(&cwd, &["b.txt".into()], "add b").unwrap();
        discard(&cwd, &["a.txt".into()]).unwrap();
        assert_eq!(std::fs::read_to_string(p.join("a.txt")).unwrap(), "1\n");
        assert_eq!(changes(&cwd).unwrap()["files"].as_array().unwrap().len(), 0);
        assert!(discard(&cwd, &["../x".into()]).is_err());
    }
}
