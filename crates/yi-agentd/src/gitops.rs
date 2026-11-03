//! git operations for summarizing session changes: which files changed and their diffs, discarding changes, committing.

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::path::Path;
use std::process::{Command, Stdio};

/// Size limit for a single file's diff; larger ones just get a notice
const DIFF_MAX: usize = 200 * 1024;
/// At most this many changed files are returned (VS Code's git.statusLimit default): a repo without a .gitignore for
/// node_modules and the like can list hundreds of thousands, and the panel polls every 10 s
const STATUS_MAX: usize = 10_000;
/// New files beyond this many don't get their lines counted (that reads each file)
const COUNT_MAX: usize = 2_000;

fn git(cwd: &Path, args: &[&str]) -> Result<std::process::Output> {
    let mut cmd = Command::new("git");
    cmd.args(["-c", "core.quotepath=false"]).args(args).current_dir(cwd);
    // Read-only commands like status may otherwise take index.lock to refresh the index; the panel polls status, and an
    // agent's git commit landing at the same moment would fail with "index.lock exists" (VS Code sets the same)
    cmd.env("GIT_OPTIONAL_LOCKS", "0");
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
    // Current commit: the frontend refreshes the commit history when it changes (null in a repo with no commits yet)
    let head = git_ok(&root, &["rev-parse", "--verify", "-q", "HEAD"]).ok().map(|s| s.trim().to_string());
    let status = git_ok(&root, &["status", "--porcelain=v1", "-z", "-uall"])?;
    let stats = numstat(&root);
    let mut files = vec![];
    let (mut total, mut counted) = (0usize, 0usize);
    let mut it = status.split('\0').filter(|s| !s.is_empty());
    while let Some(entry) = it.next() {
        if entry.len() < 4 {
            continue;
        }
        total += 1;
        let (code, path) = entry.split_at(3);
        let code = code.trim().to_string();
        // Rename entries are followed by the original path
        if code.starts_with('R') || code.starts_with('C') {
            it.next();
        }
        if only.is_some_and(|o| !o.iter().any(|x| x == path)) || files.len() >= STATUS_MAX {
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
            counted += 1;
            (if counted <= COUNT_MAX { count_lines(&root.join(path)) } else { None }, Some(0))
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
        "head": head,
        // Total number of changed files, set only when more than STATUS_MAX were left out
        "total": if only.is_none() && total > files.len() { Some(total) } else { None },
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

/// Field / record separators for git log --format (control characters that can't appear in a commit subject)
const FS: char = '\x1f';
const RS: char = '\x1e';

/// Commit history of HEAD, newest first; skip/limit page through it, more says whether there are older commits
pub fn log(cwd: &str, skip: usize, limit: usize) -> Result<Value> {
    let dir = crate::paths::expand(cwd);
    let Some(root) = repo_root(&dir) else {
        return Ok(json!({ "repo": null, "commits": [], "more": false }));
    };
    let repo = root.display().to_string();
    // New repo with no commits yet: no HEAD
    if !git(&root, &["rev-parse", "--verify", "-q", "HEAD"])?.status.success() {
        return Ok(json!({ "repo": repo, "commits": [], "more": false }));
    }
    let fmt = format!("--format=%H{FS}%h{FS}%P{FS}%an{FS}%at{FS}%D{FS}%s{RS}");
    let (skip, n) = (format!("--skip={skip}"), format!("-n{}", limit + 1));
    let out = git_ok(&root, &["log", &fmt, &skip, &n, "HEAD", "--"])?;
    let mut commits: Vec<Value> = out
        .split(RS)
        .map(|r| r.trim_start_matches('\n'))
        .filter(|r| !r.is_empty())
        .filter_map(|r| {
            let f: Vec<&str> = r.splitn(7, FS).collect();
            let [hash, short, parents, author, time, refs, subject] = f[..] else {
                return None;
            };
            Some(json!({
                "hash": hash,
                "short": short,
                "parents": parents.split(' ').filter(|p| !p.is_empty()).collect::<Vec<_>>(),
                "author": author,
                "time": time.parse::<i64>().unwrap_or(0),
                "refs": refs.split(", ").filter(|x| !x.is_empty()).collect::<Vec<_>>(),
                "subject": subject,
            }))
        })
        .collect();
    let more = commits.len() > limit;
    commits.truncate(limit);
    Ok(json!({ "repo": repo, "commits": commits, "more": more }))
}

/// A commit's changes relative to its first parent (a merge shows what it brought into the current branch; the first commit is compared with the empty tree).
/// Without paths only statuses and line counts are listed; diffs are returned for the files in paths (relative to the repo root)
pub fn show(cwd: &str, hash: &str, only: Option<&[String]>) -> Result<Value> {
    if hash.len() < 4 || hash.len() > 64 || !hash.bytes().all(|c| c.is_ascii_hexdigit()) {
        bail!("提交号无效: {hash}");
    }
    if let Some(o) = only {
        check_paths(o)?;
    }
    let dir = crate::paths::expand(cwd);
    let root = repo_root(&dir).ok_or_else(|| anyhow!("不是 git 仓库"))?;
    let full = git_ok(&root, &["rev-parse", "--verify", &format!("{hash}^{{commit}}")])?.trim().to_string();
    let parent = git_ok(&root, &["rev-parse", "--verify", "-q", &format!("{full}^1")]).ok().map(|s| s.trim().to_string());
    // No parent: compare with the empty tree (hash-object computes it in the repo's own hash format)
    let base = match &parent {
        Some(p) => p.clone(),
        None => git_ok(&root, &["hash-object", "-t", "tree", "--stdin"])?.trim().to_string(),
    };
    let info = git_ok(&root, &["log", "-1", &format!("--format=%an{FS}%at{FS}%B"), &full, "--"])?;
    let mut it = info.splitn(3, FS);
    let (author, time, message) = (it.next().unwrap_or(""), it.next().unwrap_or("0"), it.next().unwrap_or(""));

    let mut stats = std::collections::HashMap::new();
    for rec in git_ok(&root, &["diff", "--numstat", "-z", "--no-renames", &base, &full])?.split('\0').filter(|s| !s.is_empty()) {
        let mut f = rec.splitn(3, '\t');
        if let (Some(a), Some(d), Some(path)) = (f.next(), f.next(), f.next()) {
            stats.insert(path.to_string(), (a.parse::<u64>().ok(), d.parse::<u64>().ok()));
        }
    }
    let names = git_ok(&root, &["diff", "--name-status", "-z", "--no-renames", &base, &full])?;
    let mut files = vec![];
    let mut parts = names.split('\0').filter(|s| !s.is_empty());
    while let (Some(code), Some(path)) = (parts.next(), parts.next()) {
        if only.is_some_and(|o| !o.iter().any(|x| x == path)) {
            continue;
        }
        let diff = if only.is_some() {
            let d = git_ok(&root, &["diff", "--no-color", &base, &full, "--", path]).unwrap_or_default();
            if d.len() > DIFF_MAX {
                String::new()
            } else {
                d
            }
        } else {
            String::new()
        };
        let (added, deleted) = stats.get(path).copied().unwrap_or((None, None));
        files.push(json!({ "path": path, "status": code, "added": added, "deleted": deleted, "diff": diff }));
    }
    Ok(json!({
        "repo": root.display().to_string(),
        "hash": full,
        "parent": parent,
        "author": author,
        "time": time.trim().parse::<i64>().unwrap_or(0),
        "message": message.trim_end(),
        "files": files,
    }))
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

    #[test]
    fn log_and_show() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path();
        let run = |a: &[&str]| assert!(git(p, a).unwrap().status.success(), "{a:?}");
        run(&["init", "-q"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        let cwd = p.display().to_string();
        // No commits yet: empty history, not an error
        assert_eq!(log(&cwd, 0, 10).unwrap()["commits"].as_array().unwrap().len(), 0);
        std::fs::write(p.join("a.txt"), "1\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-qm", "first"]);
        std::fs::write(p.join("a.txt"), "1\n2\n").unwrap();
        std::fs::write(p.join("b.txt"), "b\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-qm", "second\n\nbody line"]);

        let l = log(&cwd, 0, 1).unwrap();
        let c = l["commits"].as_array().unwrap();
        assert_eq!(c.len(), 1);
        assert_eq!(c[0]["subject"], "second");
        assert_eq!(l["more"], true);
        let l2 = log(&cwd, 1, 10).unwrap();
        assert_eq!(l2["commits"][0]["subject"], "first");
        assert_eq!(l2["more"], false);

        let head = c[0]["hash"].as_str().unwrap();
        let s = show(&cwd, head, None).unwrap();
        assert_eq!(s["message"], "second\n\nbody line");
        let files = s["files"].as_array().unwrap();
        assert_eq!(files.len(), 2);
        let a = files.iter().find(|f| f["path"] == "a.txt").unwrap();
        assert_eq!((a["status"].as_str(), a["added"].as_u64(), a["deleted"].as_u64()), (Some("M"), Some(1), Some(0)));
        assert!(files.iter().any(|f| f["path"] == "b.txt" && f["status"] == "A"));
        assert!(files.iter().all(|f| f["diff"] == ""));
        let one = show(&cwd, head, Some(&["a.txt".to_string()])).unwrap();
        assert_eq!(one["files"].as_array().unwrap().len(), 1);
        assert!(one["files"][0]["diff"].as_str().unwrap().contains("+2"));

        // First commit: compared with the empty tree
        let first = l2["commits"][0]["short"].as_str().unwrap();
        let s = show(&cwd, first, None).unwrap();
        assert!(s["parent"].is_null());
        assert_eq!(s["files"][0]["status"], "A");

        assert!(show(&cwd, "HEAD; rm", None).is_err());
        assert!(show(&cwd, head, Some(&["../x".to_string()])).is_err());
    }
}
