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
/// Separate cap for ignored paths (`git status --ignored=matching`) so ignored entries never crowd out changed files
const IGNORED_MAX: usize = 10_000;
/// New files beyond this many don't get their lines counted (that reads each file)
const COUNT_MAX: usize = 2_000;
/// Prefix for hidden per-turn checkpoint git refs (never appears in `git branch` or `git log`)
pub const CHECKPOINT_PREFIX: &str = "refs/yi/checkpoints/";
/// Skip capturing a checkpoint when untracked non-ignored files exceed these limits
const CHECKPOINT_MAX_UNTRACKED_FILES: usize = 2_000;
const CHECKPOINT_MAX_UNTRACKED_BYTES: u64 = 50 * 1024 * 1024;

fn git_cmd(cwd: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    cmd.args(["-c", "core.quotepath=false"]).args(args).current_dir(cwd);
    // Read-only commands like status may otherwise take index.lock to refresh the index; the panel polls status, and an
    // agent's git commit landing at the same moment would fail with "index.lock exists" (VS Code sets the same)
    cmd.env("GIT_OPTIONAL_LOCKS", "0");
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    cmd
}

fn git(cwd: &Path, args: &[&str]) -> Result<std::process::Output> {
    git_cmd(cwd, args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| anyhow!("运行 git 失败（这台机器装了 git 吗）: {e}"))
}

fn git_env(cwd: &Path, args: &[&str], envs: &[(&str, &str)]) -> Result<std::process::Output> {
    let mut cmd = git_cmd(cwd, args);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    cmd.stdin(Stdio::null())
        .output()
        .map_err(|e| anyhow!("运行 git 失败（这台机器装了 git 吗）: {e}"))
}

fn git_env_ok(cwd: &Path, args: &[&str], envs: &[(&str, &str)]) -> Result<String> {
    let out = git_env(cwd, args, envs)?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

fn git_stdin(cwd: &Path, args: &[&str], envs: &[(&str, &str)], input: &[u8]) -> Result<std::process::Output> {
    use std::io::Write;
    let mut cmd = git_cmd(cwd, args);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow!("运行 git 失败（这台机器装了 git 吗）: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(input);
    }
    child
        .wait_with_output()
        .map_err(|e| anyhow!("等待 git 执行失败: {e}"))
}

/// Validate a hidden checkpoint ref name (`refs/yi/checkpoints/<sid>/<name>`)
pub fn is_valid_checkpoint_ref(rev: &str) -> bool {
    let Some(rest) = rev.strip_prefix(CHECKPOINT_PREFIX) else {
        return false;
    };
    if rest.is_empty() || rest.len() > 160 || rest.contains("..") {
        return false;
    }
    let mut parts = rest.split('/');
    let Some(sid) = parts.next() else { return false };
    let Some(name) = parts.next() else { return false };
    if parts.next().is_some() {
        return false;
    }
    let ok_seg = |s: &str| {
        !s.is_empty()
            && !s.starts_with(['-', '.'])
            && !s.ends_with('.')
            && !s.ends_with(".lock")
            && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
    };
    ok_seg(sid) && ok_seg(name)
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
        return Ok(json!({ "repo": null, "files": [], "ignored": [] }));
    };
    let branch = git_ok(&root, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_default();
    // Current commit: the frontend refreshes the commit history when it changes (null in a repo with no commits yet)
    let head = git_ok(&root, &["rev-parse", "--verify", "-q", "HEAD"]).ok().map(|s| s.trim().to_string());
    // Include ignored entries only for full listings; --ignored=matching stops at ignored directory boundaries
    // (avoiding a walk into node_modules/target), falling back for very old git (< 2.16) that lacks it
    let status = if only.is_none() {
        git_ok(&root, &["status", "--porcelain=v1", "-z", "-uall", "--ignored=matching"])
            .or_else(|_| git_ok(&root, &["status", "--porcelain=v1", "-z", "-uall"]))?
    } else {
        git_ok(&root, &["status", "--porcelain=v1", "-z", "-uall"])?
    };
    let stats = numstat(&root);
    let mut files = vec![];
    let mut ignored = vec![];
    let (mut total, mut counted) = (0usize, 0usize);
    let mut it = status.split('\0').filter(|s| !s.is_empty());
    while let Some(entry) = it.next() {
        if entry.len() < 4 {
            continue;
        }
        let (code, path) = entry.split_at(3);
        let code = code.trim().to_string();
        if code == "!!" {
            if ignored.len() < IGNORED_MAX {
                ignored.push(path.to_string());
            }
            continue;
        }
        total += 1;
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
        // Ignored paths relative to the repo root (directories end with '/')
        "ignored": ignored,
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

/// Size limit for git.blob (same as fs.read's text limit): larger files fall back to the hunk-only diff
const BLOB_MAX: u64 = 1024 * 1024;

/// A file's full content at `rev` ("HEAD", a commit hash, a hash + "^" for its first parent, or a hidden checkpoint ref).
/// A path missing at that revision (added / deleted files, or a root commit's parent) gives exists=false
pub fn blob(cwd: &str, path: &str, rev: &str) -> Result<Value> {
    check_paths(&[path.to_string()])?;
    let hash = rev.strip_suffix('^').unwrap_or(rev);
    let is_hash = hash.len() >= 4 && hash.len() <= 64 && hash.bytes().all(|c| c.is_ascii_hexdigit());
    if rev != "HEAD" && !is_hash && !is_valid_checkpoint_ref(rev) {
        bail!("提交号无效: {rev}");
    }
    let dir = crate::paths::expand(cwd);
    let root = repo_root(&dir).ok_or_else(|| anyhow!("不是 git 仓库"))?;
    // Backslashes from Windows paths aren't separators in a tree path
    let spec = format!("{rev}:{}", path.replace('\\', "/"));
    let Some(oid) = git_ok(&root, &["rev-parse", "--verify", "-q", &spec]).ok().map(|s| s.trim().to_string()) else {
        return Ok(json!({ "exists": false, "binary": false, "tooLarge": false, "notUtf8": false }));
    };
    let size: u64 = git_ok(&root, &["cat-file", "-s", &oid])?.trim().parse().unwrap_or(0);
    if size > BLOB_MAX {
        return Ok(json!({ "exists": true, "binary": false, "tooLarge": true, "notUtf8": false, "size": size }));
    }
    let out = git(&root, &["cat-file", "blob", &oid])?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let b = out.stdout;
    if b.iter().take(8000).any(|&c| c == 0) {
        return Ok(json!({ "exists": true, "binary": true, "tooLarge": false, "notUtf8": false, "size": size }));
    }
    let (text, not_utf8) = match String::from_utf8(b) {
        Ok(s) => (s, false),
        Err(e) => (String::from_utf8_lossy(e.as_bytes()).into_owned(), true),
    };
    Ok(json!({ "exists": true, "text": text, "binary": false, "tooLarge": false, "notUtf8": not_utf8, "size": size }))
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

/// Where new worktrees go (`DaemonSettings::worktree_root`, default `~/.yi-console/worktrees`)
pub fn worktree_root(settings: &yi_proto::DaemonSettings) -> std::path::PathBuf {
    match settings.worktree_root.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(p) => crate::paths::expand(p),
        None => crate::paths::base_dir().join("worktrees"),
    }
}

fn safe_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && !s.starts_with(['-', '.'])
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

/// Add a worktree for the repo at `cwd`: `<root>/<repo name>-<hash>/<name>` on a new branch (default `yi/<name>`)
/// from `base` (default HEAD). The hash keeps repos with the same name apart
pub fn worktree_add(root: &Path, cwd: &str, name: &str, branch: Option<&str>, base: Option<&str>) -> Result<Value> {
    if !safe_name(name) {
        bail!("worktree 名称只能包含字母、数字、点、下划线和短横线: {name}");
    }
    let repo = repo_root(Path::new(cwd)).ok_or_else(|| anyhow!("不是 git 仓库: {cwd}"))?;
    let branch = branch.map(str::to_string).unwrap_or_else(|| format!("yi/{name}"));
    let base = base.unwrap_or("HEAD");
    if branch.starts_with('-') || base.starts_with('-') || git_ok(&repo, &["check-ref-format", "--branch", &branch]).is_err() {
        bail!("分支名无效: {branch}");
    }
    let repo_name = repo.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "repo".into());
    let repo_name: String = repo_name.chars().map(|c| if c.is_alphanumeric() || "._-".contains(c) { c } else { '_' }).collect();
    let hash = format!("{:x}", md5::compute(repo.to_string_lossy().as_bytes()));
    let dir = root.join(format!("{repo_name}-{}", &hash[..8])).join(name);
    if dir.exists() {
        bail!("worktree 目录已存在: {}", dir.display());
    }
    std::fs::create_dir_all(dir.parent().unwrap())?;
    let path = dir.to_string_lossy().to_string();
    git_ok(&repo, &["worktree", "add", "-b", &branch, &path, base])?;
    Ok(json!({ "path": path, "branch": branch, "repo": repo.to_string_lossy() }))
}

/// Remove a worktree made by `worktree_add`; only paths under the worktree root are accepted
pub fn worktree_remove(root: &Path, path: &str, force: bool, delete_branch: bool) -> Result<()> {
    let dir = Path::new(path).canonicalize().map_err(|_| anyhow!("worktree 不存在: {path}"))?;
    let root = root.canonicalize().map_err(|_| anyhow!("worktree 根目录不存在"))?;
    if !dir.starts_with(&root) || dir == root {
        bail!("只能删除 worktree 根目录下的 worktree: {path}");
    }
    let common = git_ok(&dir, &["rev-parse", "--path-format=absolute", "--git-common-dir"])?;
    let common = std::path::PathBuf::from(common.trim());
    let branch = git_ok(&dir, &["symbolic-ref", "--quiet", "--short", "HEAD"]).ok().map(|s| s.trim().to_string());
    // The path as given: git may not understand the verbatim (\\?\) form canonicalize returns on Windows
    let target = path.to_string();
    let mut args = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.push(&target);
    // Run from the shared git dir: git refuses to remove the worktree it is running in
    git_ok(&common, &args)?;
    if let (true, Some(b)) = (delete_branch, branch.filter(|b| !b.is_empty())) {
        git_ok(&common, &["branch", "-D", &b])?;
    }
    Ok(())
}

/// Normalize a path for equality checks across platforms (strip \\?\ verbatim prefix, unify separators, fold case on Windows)
fn norm_path_key(p: &str) -> String {
    let s = p.strip_prefix(r"\\?\").unwrap_or(p).replace('\\', "/");
    let s = s.trim_end_matches('/');
    if cfg!(windows) {
        s.to_ascii_lowercase()
    } else {
        s.to_string()
    }
}

/// Convert a git worktree path to a native display path (Windows uses backslashes; strip any \\?\ prefix)
fn native_worktree_path(raw: &str) -> String {
    let s = raw.strip_prefix(r"\\?\").unwrap_or(raw);
    if cfg!(windows) {
        s.replace('/', "\\")
    } else {
        s.to_string()
    }
}

/// List all worktrees belonging to the git repository containing `cwd` (including user- or AI-created worktrees anywhere on disk)
pub fn worktrees(cwd: &str) -> Result<Value> {
    let dir = crate::paths::expand(cwd);
    let Some(root) = repo_root(&dir) else {
        return Ok(json!({ "repo": null, "current": null, "worktrees": [] }));
    };
    let current_canon = root.canonicalize().unwrap_or_else(|_| root.clone());
    let current_key = norm_path_key(&current_canon.to_string_lossy());
    let out = git_ok(&root, &["worktree", "list", "--porcelain"])?;

    let mut items: Vec<Value> = Vec::new();
    let mut path: Option<String> = None;
    let mut head = String::new();
    let mut branch: Option<String> = None;
    let mut detached = false;
    let mut bare = false;
    let mut prunable = false;

    let flush = |items: &mut Vec<Value>,
                 path: &mut Option<String>,
                 head: &mut String,
                 branch: &mut Option<String>,
                 detached: &mut bool,
                 bare: &mut bool,
                 prunable: &mut bool| {
        if let Some(raw_path) = path.take() {
            let native = native_worktree_path(&raw_path);
            let item_canon = Path::new(&native)
                .canonicalize()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| native.clone());
            let is_current = norm_path_key(&item_canon) == current_key;
            let is_main = items.is_empty();
            items.push(json!({
                "path": native,
                "head": head.clone(),
                "branch": branch.take(),
                "detached": *detached,
                "isMain": is_main,
                "isCurrent": is_current,
                "bare": *bare,
                "prunable": *prunable,
            }));
        }
        head.clear();
        *branch = None;
        *detached = false;
        *bare = false;
        *prunable = false;
    };

    for line in out.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            flush(&mut items, &mut path, &mut head, &mut branch, &mut detached, &mut bare, &mut prunable);
            continue;
        }
        if let Some(v) = line.strip_prefix("worktree ") {
            flush(&mut items, &mut path, &mut head, &mut branch, &mut detached, &mut bare, &mut prunable);
            path = Some(v.to_string());
        } else if let Some(v) = line.strip_prefix("HEAD ") {
            head = v.to_string();
        } else if let Some(v) = line.strip_prefix("branch ") {
            branch = Some(v.strip_prefix("refs/heads/").unwrap_or(v).to_string());
        } else if line == "detached" {
            detached = true;
        } else if line == "bare" {
            bare = true;
        } else if line.starts_with("prunable") {
            prunable = true;
        }
    }
    flush(&mut items, &mut path, &mut head, &mut branch, &mut detached, &mut bare, &mut prunable);

    let main_path = items
        .first()
        .and_then(|v| v["path"].as_str())
        .map(str::to_string)
        .unwrap_or_else(|| native_worktree_path(&root.to_string_lossy()));

    Ok(json!({
        "repo": main_path,
        "current": native_worktree_path(&root.to_string_lossy()),
        "worktrees": items,
    }))
}

/// Detect the repository's default branch name (origin/HEAD -> local main/master -> origin/main|master -> current branch)
fn detect_default_branch(root: &Path) -> Option<String> {
    if let Ok(sym) = git_ok(root, &["symbolic-ref", "--quiet", "--short", "refs/remotes/origin/HEAD"]) {
        let sym = sym.trim();
        let short = sym.strip_prefix("origin/").unwrap_or(sym);
        if !short.is_empty() {
            if git(root, &["show-ref", "--verify", "--quiet", &format!("refs/heads/{short}")])
                .is_ok_and(|o| o.status.success())
            {
                return Some(short.to_string());
            }
            if git(root, &["show-ref", "--verify", "--quiet", &format!("refs/remotes/{sym}")])
                .is_ok_and(|o| o.status.success())
            {
                return Some(sym.to_string());
            }
        }
    }
    for cand in ["main", "master"] {
        if git(root, &["show-ref", "--verify", "--quiet", &format!("refs/heads/{cand}")])
            .is_ok_and(|o| o.status.success())
        {
            return Some(cand.to_string());
        }
    }
    for cand in ["origin/main", "origin/master"] {
        if git(root, &["show-ref", "--verify", "--quiet", &format!("refs/remotes/{cand}")])
            .is_ok_and(|o| o.status.success())
        {
            return Some(cand.to_string());
        }
    }
    git_ok(root, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Parse `[ahead 2, behind 1]` / `[ahead 3]` / `[behind 1]` from `%(upstream:track)`
fn parse_track(track: &str) -> (Option<u64>, Option<u64>) {
    let mut ahead = None;
    let mut behind = None;
    let inner = track.trim().trim_start_matches('[').trim_end_matches(']');
    for part in inner.split(',') {
        let p = part.trim();
        if let Some(n) = p.strip_prefix("ahead ") {
            ahead = n.trim().parse().ok();
        } else if let Some(n) = p.strip_prefix("behind ") {
            behind = n.trim().parse().ok();
        }
    }
    (ahead, behind)
}

/// List local branches, the current branch, and the detected default branch
pub fn branches(cwd: &str) -> Result<Value> {
    let dir = crate::paths::expand(cwd);
    let Some(root) = repo_root(&dir) else {
        return Ok(json!({ "repo": null, "current": null, "defaultBranch": null, "branches": [] }));
    };
    let current = git_ok(&root, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let default_branch = detect_default_branch(&root);
    let fmt = format!("--format=%(HEAD){FS}%(refname:short){FS}%(upstream:short){FS}%(upstream:track){RS}");
    let out = git_ok(&root, &["for-each-ref", "--sort=-committerdate", &fmt, "refs/heads"])?;
    let mut list = Vec::new();
    for rec in out.split(RS).map(|r| r.trim_start_matches('\n')).filter(|r| !r.is_empty()) {
        let f: Vec<&str> = rec.splitn(4, FS).collect();
        let [head_mark, name, upstream, track] = f[..] else {
            continue;
        };
        let (ahead, behind) = parse_track(track);
        list.push(json!({
            "name": name,
            "current": head_mark == "*",
            "upstream": if upstream.is_empty() { Value::Null } else { json!(upstream) },
            "ahead": ahead,
            "behind": behind,
        }));
    }
    Ok(json!({
        "repo": root.display().to_string(),
        "current": current,
        "defaultBranch": default_branch,
        "branches": list,
    }))
}

/// Switch to an existing branch or create and switch to a new branch
pub fn checkout(cwd: &str, branch: &str, create: bool, base: Option<&str>) -> Result<Value> {
    let branch = branch.trim();
    if branch.is_empty() || branch.starts_with('-') {
        bail!("分支名无效: {branch}");
    }
    let dir = crate::paths::expand(cwd);
    let root = repo_root(&dir).ok_or_else(|| anyhow!("不是 git 仓库"))?;
    if git_ok(&root, &["check-ref-format", "--branch", branch]).is_err() {
        bail!("分支名无效: {branch}");
    }
    if create {
        let mut args = vec!["checkout", "-b", branch];
        if let Some(b) = base.map(str::trim).filter(|s| !s.is_empty()) {
            if b.starts_with('-') {
                bail!("基准分支无效: {b}");
            }
            args.push(b);
        }
        git_ok(&root, &args)?;
    } else {
        git_ok(&root, &["checkout", branch])?;
    }
    Ok(json!({ "branch": branch }))
}

/// Push the current branch to its upstream or to `remote` (setting upstream when absent)
pub fn push(cwd: &str, remote: Option<&str>, branch: Option<&str>, set_upstream: bool) -> Result<Value> {
    let dir = crate::paths::expand(cwd);
    let root = repo_root(&dir).ok_or_else(|| anyhow!("不是 git 仓库"))?;
    let current_branch = match branch.map(str::trim).filter(|s| !s.is_empty()) {
        Some(b) => {
            if b.starts_with('-') || git_ok(&root, &["check-ref-format", "--branch", b]).is_err() {
                bail!("分支名无效: {b}");
            }
            b.to_string()
        }
        None => git_ok(&root, &["symbolic-ref", "--quiet", "--short", "HEAD"])
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("当前处于分离头指针状态，请先切换到分支再推送"))?,
    };
    let explicit_remote = remote.map(str::trim).filter(|s| !s.is_empty());
    if let Some(r) = explicit_remote {
        if r.starts_with('-') {
            bail!("远程仓库名无效: {r}");
        }
    }
    let has_upstream = git(&root, &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"])
        .is_ok_and(|o| o.status.success());

    let (chosen_remote, args): (String, Vec<String>) = if has_upstream && explicit_remote.is_none() && branch.is_none() {
        let up = git_ok(&root, &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"])
            .unwrap_or_default();
        let rem = up.trim().split('/').next().unwrap_or("origin").to_string();
        (rem, vec!["push".into()])
    } else {
        let rem = if let Some(r) = explicit_remote {
            r.to_string()
        } else {
            let remotes = git_ok(&root, &["remote"])?;
            let list: Vec<&str> = remotes.lines().map(str::trim).filter(|s| !s.is_empty()).collect();
            if list.is_empty() {
                bail!("未配置远程仓库（remote）");
            }
            if list.contains(&"origin") {
                "origin".to_string()
            } else {
                list[0].to_string()
            }
        };
        let mut a = vec!["push".to_string()];
        if set_upstream {
            a.push("-u".into());
        }
        a.push(rem.clone());
        a.push(current_branch.clone());
        (rem, a)
    };

    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = git(&root, &arg_refs)?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if !out.status.success() {
        let msg = if !stderr.is_empty() { stderr } else { stdout };
        bail!("{msg}");
    }
    let combined = [stdout, stderr].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n");
    Ok(json!({
        "branch": current_branch,
        "remote": chosen_remote,
        "output": combined,
    }))
}

/// Build a unified diff for an untracked file on disk (empty for binaries or oversized files)
fn untracked_diff(root: &Path, path: &str) -> String {
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
}

/// Working tree (including branch commits and untracked files) compared to `merge-base(base, HEAD)`
pub fn branch_diff(cwd: &str, base: Option<&str>, with_diff: bool, only: Option<&[String]>) -> Result<Value> {
    if let Some(o) = only {
        check_paths(o)?;
    }
    let dir = crate::paths::expand(cwd);
    let Some(root) = repo_root(&dir) else {
        return Ok(json!({ "repo": null, "branch": "", "base": null, "mergeBase": null, "files": [] }));
    };
    let branch = git_ok(&root, &["rev-parse", "--abbrev-ref", "HEAD"])
        .unwrap_or_default()
        .trim()
        .to_string();
    let Some(_) = git_ok(&root, &["rev-parse", "--verify", "-q", "HEAD"]).ok() else {
        return Ok(json!({ "repo": root.display().to_string(), "branch": branch, "base": null, "mergeBase": null, "files": [] }));
    };
    let chosen_base = match base.map(str::trim).filter(|s| !s.is_empty()) {
        Some(b) => {
            if b.starts_with('-') {
                bail!("基准分支无效: {b}");
            }
            b.to_string()
        }
        None => match detect_default_branch(&root) {
            Some(b) => b,
            None => return Ok(json!({ "repo": root.display().to_string(), "branch": branch, "base": null, "mergeBase": null, "files": [] })),
        },
    };
    let Ok(mb_raw) = git_ok(&root, &["merge-base", &chosen_base, "HEAD"]) else {
        return Ok(json!({ "repo": root.display().to_string(), "branch": branch, "base": chosen_base, "mergeBase": null, "files": [] }));
    };
    let merge_base = mb_raw.trim().to_string();

    let mut stats = std::collections::HashMap::new();
    if let Ok(out) = git_ok(&root, &["diff", "--numstat", "-z", "--no-renames", &merge_base]) {
        for rec in out.split('\0').filter(|s| !s.is_empty()) {
            let mut it = rec.splitn(3, '\t');
            if let (Some(a), Some(d), Some(path)) = (it.next(), it.next(), it.next()) {
                stats.insert(path.to_string(), (a.parse::<u64>().ok(), d.parse::<u64>().ok()));
            }
        }
    }

    let mut entries: Vec<(String, String, bool)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let names = git_ok(&root, &["diff", "--name-status", "-z", "--no-renames", &merge_base])?;
    let mut parts = names.split('\0').filter(|s| !s.is_empty());
    while let (Some(code), Some(path)) = (parts.next(), parts.next()) {
        if seen.insert(path.to_string()) {
            entries.push((path.to_string(), code.to_string(), false));
        }
    }
    if let Ok(untracked) = git_ok(&root, &["ls-files", "--others", "--exclude-standard", "-z"]) {
        for path in untracked.split('\0').filter(|s| !s.is_empty()) {
            if seen.insert(path.to_string()) {
                entries.push((path.to_string(), "new".to_string(), true));
            }
        }
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let total = entries.len();
    let mut files = Vec::new();
    let mut counted = 0usize;
    for (path, code, is_untracked) in entries {
        if only.is_some_and(|o| !o.iter().any(|x| x == &path)) || files.len() >= STATUS_MAX {
            continue;
        }
        let diff = if !with_diff {
            String::new()
        } else if is_untracked {
            untracked_diff(&root, &path)
        } else {
            let d = git_ok(&root, &["diff", &merge_base, "--no-color", "--", &path]).unwrap_or_default();
            if d.len() > DIFF_MAX { String::new() } else { d }
        };
        let (added, deleted) = if is_untracked {
            counted += 1;
            (if counted <= COUNT_MAX { count_lines(&root.join(&path)) } else { None }, Some(0))
        } else {
            stats.get(&path).copied().unwrap_or((None, None))
        };
        files.push(json!({
            "path": path,
            "status": code,
            "diff": diff,
            "added": added,
            "deleted": deleted,
        }));
    }

    Ok(json!({
        "repo": root.display().to_string(),
        "branch": branch,
        "base": chosen_base,
        "mergeBase": merge_base,
        "total": if only.is_none() && total > files.len() { Some(total) } else { None },
        "files": files,
    }))
}

/// RAII guard for a temporary `GIT_INDEX_FILE` so checkpoints never touch the repository's real `.git/index`
struct TempIndex(std::path::PathBuf);

impl TempIndex {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!("yi-ckpt-idx-{}", uuid::Uuid::new_v4().simple()));
        let _ = std::fs::remove_file(&p);
        Self(p)
    }

    fn as_str(&self) -> String {
        self.0.to_string_lossy().into_owned()
    }
}

impl Drop for TempIndex {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Build a git tree OID representing the current working tree (tracked + non-ignored untracked files)
/// without touching the repository's real `.git/index`. Returns `None` if untracked files exceed safety caps.
fn write_working_tree(root: &Path) -> Result<Option<String>> {
    if let Ok(untracked) = git_ok(root, &["ls-files", "--others", "--exclude-standard", "-z"]) {
        let mut count = 0usize;
        let mut bytes = 0u64;
        for rel in untracked.split('\0').filter(|s| !s.is_empty()) {
            count += 1;
            if count > CHECKPOINT_MAX_UNTRACKED_FILES {
                return Ok(None);
            }
            if let Ok(meta) = std::fs::symlink_metadata(root.join(rel)) {
                if meta.is_file() {
                    bytes = bytes.saturating_add(meta.len());
                    if bytes > CHECKPOINT_MAX_UNTRACKED_BYTES {
                        return Ok(None);
                    }
                }
            }
        }
    }
    let tmp = TempIndex::new();
    let idx = tmp.as_str();
    let env = [("GIT_INDEX_FILE", idx.as_str())];
    if git(root, &["rev-parse", "--verify", "-q", "HEAD"])?.status.success() {
        let _ = git_env(root, &["read-tree", "HEAD"], &env)?;
    }
    let add = git_env(root, &["add", "-A", "--", "."], &env)?;
    if !add.status.success() {
        bail!("{}", String::from_utf8_lossy(&add.stderr).trim());
    }
    let tree = git_env_ok(root, &["write-tree"], &env)?.trim().to_string();
    Ok(Some(tree))
}

/// Capture the current working tree (including non-ignored untracked files) into a hidden git ref
/// under `refs/yi/checkpoints/<sid>/<name>`. Returns `Ok(None)` for non-git directories or oversized untracked trees.
pub fn capture_checkpoint(cwd: &str, ref_name: &str) -> Result<Option<String>> {
    if !is_valid_checkpoint_ref(ref_name) {
        bail!("快照引用无效: {ref_name}");
    }
    let dir = crate::paths::expand(cwd);
    let Some(root) = repo_root(&dir) else {
        return Ok(None);
    };
    let Some(tree) = write_working_tree(&root)? else {
        return Ok(None);
    };
    let commit_env = [
        ("GIT_AUTHOR_NAME", "Yi Console"),
        ("GIT_AUTHOR_EMAIL", "checkpoint@yi-console.local"),
        ("GIT_COMMITTER_NAME", "Yi Console"),
        ("GIT_COMMITTER_EMAIL", "checkpoint@yi-console.local"),
    ];
    let commit = git_env_ok(&root, &["commit-tree", &tree, "-m", "yi-checkpoint"], &commit_env)?
        .trim()
        .to_string();
    git_ok(&root, &["update-ref", ref_name, &commit])?;
    Ok(Some(tree))
}

/// Exact `(files_changed, added_lines, deleted_lines)` between two checkpoint refs
pub fn diff_stat_refs(cwd: &str, base_ref: &str, end_ref: &str) -> Result<(u32, u64, u64)> {
    if !is_valid_checkpoint_ref(base_ref) || !is_valid_checkpoint_ref(end_ref) {
        bail!("快照引用无效");
    }
    let dir = crate::paths::expand(cwd);
    let Some(root) = repo_root(&dir) else {
        return Ok((0, 0, 0));
    };
    let out = git_ok(&root, &["diff", "--numstat", "-z", "--no-renames", base_ref, end_ref])?;
    let mut files = 0u32;
    let mut added = 0u64;
    let mut deleted = 0u64;
    for rec in out.split('\0').filter(|s| !s.is_empty()) {
        let mut it = rec.splitn(3, '\t');
        if let (Some(a), Some(d), Some(_path)) = (it.next(), it.next(), it.next()) {
            files = files.saturating_add(1);
            added = added.saturating_add(a.parse::<u64>().unwrap_or(0));
            deleted = deleted.saturating_add(d.parse::<u64>().unwrap_or(0));
        }
    }
    Ok((files, added, deleted))
}

/// Diff between two hidden checkpoint refs (`base_ref..end_ref`), used by the per-turn Diff view
pub fn ref_diff(cwd: &str, base_ref: &str, end_ref: &str, with_diff: bool, only: Option<&[String]>) -> Result<Value> {
    if let Some(o) = only {
        check_paths(o)?;
    }
    if !is_valid_checkpoint_ref(base_ref) || !is_valid_checkpoint_ref(end_ref) {
        bail!("快照引用无效");
    }
    let dir = crate::paths::expand(cwd);
    let Some(root) = repo_root(&dir) else {
        return Ok(json!({ "repo": null, "baseRef": base_ref, "endRef": end_ref, "files": [] }));
    };
    if !git(&root, &["rev-parse", "--verify", "-q", &format!("{base_ref}^{{commit}}")])?.status.success()
        || !git(&root, &["rev-parse", "--verify", "-q", &format!("{end_ref}^{{commit}}")])?.status.success()
    {
        return Ok(json!({ "repo": root.display().to_string(), "baseRef": base_ref, "endRef": end_ref, "missing": true, "files": [] }));
    }

    let mut stats = std::collections::HashMap::new();
    if let Ok(out) = git_ok(&root, &["diff", "--numstat", "-z", "--no-renames", base_ref, end_ref]) {
        for rec in out.split('\0').filter(|s| !s.is_empty()) {
            let mut it = rec.splitn(3, '\t');
            if let (Some(a), Some(d), Some(path)) = (it.next(), it.next(), it.next()) {
                stats.insert(path.to_string(), (a.parse::<u64>().ok(), d.parse::<u64>().ok()));
            }
        }
    }

    let mut entries: Vec<(String, String)> = Vec::new();
    let names = git_ok(&root, &["diff", "--name-status", "-z", "--no-renames", base_ref, end_ref])?;
    let mut parts = names.split('\0').filter(|s| !s.is_empty());
    while let (Some(code), Some(path)) = (parts.next(), parts.next()) {
        let mapped = if code == "A" { "new" } else { code };
        entries.push((path.to_string(), mapped.to_string()));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let total = entries.len();
    let mut files = Vec::new();
    for (path, code) in entries {
        if only.is_some_and(|o| !o.iter().any(|x| x == &path)) || files.len() >= STATUS_MAX {
            continue;
        }
        let diff = if !with_diff {
            String::new()
        } else {
            let d = git_ok(&root, &["diff", base_ref, end_ref, "--no-color", "--", &path]).unwrap_or_default();
            if d.len() > DIFF_MAX { String::new() } else { d }
        };
        let (added, deleted) = stats.get(&path).copied().unwrap_or((None, None));
        files.push(json!({
            "path": path,
            "status": code,
            "diff": diff,
            "added": added,
            "deleted": deleted,
        }));
    }

    Ok(json!({
        "repo": root.display().to_string(),
        "baseRef": base_ref,
        "endRef": end_ref,
        "total": if only.is_none() && total > files.len() { Some(total) } else { None },
        "files": files,
    }))
}

/// Delete a single hidden checkpoint ref if it exists
pub fn delete_ref(cwd: &str, ref_name: &str) {
    if !is_valid_checkpoint_ref(ref_name) {
        return;
    }
    let dir = crate::paths::expand(cwd);
    if let Some(root) = repo_root(&dir) {
        let _ = git(&root, &["update-ref", "-d", ref_name]);
    }
}

/// Delete a batch of hidden checkpoint refs in one `git update-ref --stdin` call
pub fn delete_refs(cwd: &str, ref_names: &[String]) {
    let valid: Vec<&str> = ref_names.iter().map(String::as_str).filter(|r| is_valid_checkpoint_ref(r)).collect();
    if valid.is_empty() {
        return;
    }
    let dir = crate::paths::expand(cwd);
    let Some(root) = repo_root(&dir) else {
        return;
    };
    let mut script = String::new();
    for r in valid {
        script.push_str("delete ");
        script.push_str(r);
        script.push('\n');
    }
    let _ = git_stdin(&root, &["update-ref", "--stdin"], &[], script.as_bytes());
}

/// Delete all hidden checkpoint refs belonging to a session (`refs/yi/checkpoints/<sid>/*`)
pub fn delete_session_checkpoints(cwd: &str, sid: &str) {
    if sid.is_empty() || !sid.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-')) {
        return;
    }
    let dir = crate::paths::expand(cwd);
    let Some(root) = repo_root(&dir) else {
        return;
    };
    let prefix = format!("{CHECKPOINT_PREFIX}{sid}/");
    let Ok(out) = git_ok(&root, &["for-each-ref", "--format=%(refname)", &prefix]) else {
        return;
    };
    let refs: Vec<String> = out.lines().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect();
    if !refs.is_empty() {
        delete_refs(cwd, &refs);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worktrees_live_under_the_root_and_only_there_can_be_removed() {
        let d = tempfile::tempdir().unwrap();
        let repo = d.path().join("my repo");
        std::fs::create_dir_all(&repo).unwrap();
        let run = |a: &[&str]| assert!(git(&repo, a).unwrap().status.success(), "{a:?}");
        run(&["init", "-q"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(repo.join("a.txt"), "1\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-qm", "init"]);
        let root = d.path().join("wt");
        let cwd = repo.to_string_lossy().to_string();
        assert!(worktree_add(&root, &cwd, "../x", None, None).is_err());
        assert!(worktree_add(&root, &cwd, "t1", Some("--bad"), None).is_err());
        let r = worktree_add(&root, &cwd, "t1", None, None).unwrap();
        let path = r["path"].as_str().unwrap().to_string();
        assert_eq!(r["branch"], "yi/t1");
        assert!(Path::new(&path).join("a.txt").exists());
        assert!(Path::new(&path).starts_with(&root));
        // The same name again is refused
        assert!(worktree_add(&root, &cwd, "t1", Some("yi/other"), None).is_err());
        // The repo itself is not under the root
        assert!(worktree_remove(&root, &cwd, false, false).is_err());
        worktree_remove(&root, &path, false, true).unwrap();
        assert!(!Path::new(&path).exists());
        assert!(git_ok(&repo, &["branch", "--list", "yi/t1"]).unwrap().trim().is_empty());
    }

    #[test]
    fn changes_discard_commit() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path();
        let run = |a: &[&str]| assert!(git(p, a).unwrap().status.success(), "{a:?}");
        run(&["init", "-q"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        run(&["config", "core.autocrlf", "false"]);
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
    fn blob_at_revisions() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path();
        let run = |a: &[&str]| assert!(git(p, a).unwrap().status.success(), "{a:?}");
        run(&["init", "-q"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        run(&["config", "core.autocrlf", "false"]);
        std::fs::create_dir(p.join("src")).unwrap();
        std::fs::write(p.join("src/a.txt"), "one\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-qm", "first"]);
        std::fs::write(p.join("src/a.txt"), "two\n").unwrap();
        std::fs::write(p.join("bin.dat"), [0u8, 1, 2]).unwrap();
        run(&["add", "."]);
        run(&["commit", "-qm", "second"]);
        let cwd = p.display().to_string();
        let head = git_ok(p, &["rev-parse", "HEAD"]).unwrap().trim().to_string();
        let first = git_ok(p, &["rev-parse", "HEAD^"]).unwrap().trim().to_string();

        assert_eq!(blob(&cwd, "src/a.txt", "HEAD").unwrap()["text"], "two\n");
        assert_eq!(blob(&cwd, "src/a.txt", &format!("{head}^")).unwrap()["text"], "one\n");
        // Windows-style separators are accepted
        assert_eq!(blob(&cwd, "src\\a.txt", &first).unwrap()["text"], "one\n");
        // Added in the second commit: missing in its parent; a root commit has no parent at all
        assert_eq!(blob(&cwd, "bin.dat", &format!("{head}^")).unwrap()["exists"], false);
        assert_eq!(blob(&cwd, "src/a.txt", &format!("{first}^")).unwrap()["exists"], false);
        assert_eq!(blob(&cwd, "bin.dat", "HEAD").unwrap()["binary"], true);
        assert!(blob(&cwd, "../x", "HEAD").is_err());
        assert!(blob(&cwd, "src/a.txt", "HEAD; rm").is_err());
        assert!(blob(&cwd, "src/a.txt", "--output=x").is_err());
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

    #[test]
    fn changes_reports_ignored_paths() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path();
        let run = |a: &[&str]| assert!(git(p, a).unwrap().status.success(), "{a:?}");
        run(&["init", "-q"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(p.join(".gitignore"), "dist/\nfoo/\nbar/*\n!bar/keep.txt\n*.log\n").unwrap();
        std::fs::create_dir_all(p.join("dist/sub")).unwrap();
        std::fs::write(p.join("dist/sub/out.js"), "console.log(1);\n").unwrap();
        std::fs::create_dir_all(p.join("foo")).unwrap();
        std::fs::write(p.join("foo/tracked.txt"), "ok\n").unwrap();
        std::fs::write(p.join("foo/ignored.txt"), "skip\n").unwrap();
        std::fs::create_dir_all(p.join("bar")).unwrap();
        std::fs::write(p.join("bar/keep.txt"), "keep\n").unwrap();
        std::fs::write(p.join("bar/drop.o"), "drop\n").unwrap();
        std::fs::write(p.join("app.log"), "log\n").unwrap();
        std::fs::write(p.join("tracked.log"), "tracked\n").unwrap();
        run(&["add", ".gitignore"]);
        run(&["add", "-f", "foo/tracked.txt", "tracked.log"]);
        run(&["commit", "-qm", "init"]);

        let cwd = p.display().to_string();
        let res = changes_with(&cwd, false, None).unwrap();
        let files = res["files"].as_array().unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0]["path"], "bar/keep.txt");
        assert!(res["total"].is_null());

        let ignored: Vec<&str> = res["ignored"].as_array().unwrap().iter().filter_map(|v| v.as_str()).collect();
        // Whole ignored dir is reported with trailing '/', without descending into it
        assert!(ignored.contains(&"dist/"), "{ignored:?}");
        assert!(!ignored.iter().any(|x| x.starts_with("dist/sub")), "{ignored:?}");
        // Ignored file matching pattern
        assert!(ignored.contains(&"app.log"), "{ignored:?}");
        // Force-tracked file matching .gitignore is not reported as ignored
        assert!(!ignored.contains(&"tracked.log"), "{ignored:?}");
        // Ignored dir containing a tracked file only marks untracked ignored siblings, not the dir itself
        assert!(!ignored.contains(&"foo/"), "{ignored:?}");
        assert!(ignored.contains(&"foo/ignored.txt"), "{ignored:?}");
        // Negated file is in `files`, while non-negated sibling is in `ignored`
        assert!(ignored.contains(&"bar/drop.o"), "{ignored:?}");
        assert!(!ignored.contains(&"bar/keep.txt"), "{ignored:?}");

        // Single-file diff fetch skips ignored collection
        let one = changes_with(&cwd, true, Some(&["bar/keep.txt".to_string()])).unwrap();
        assert!(one["ignored"].as_array().unwrap().is_empty());
    }

    #[test]
    fn worktrees_lists_main_and_added() {
        let d = tempfile::tempdir().unwrap();
        let repo = d.path().join("repo");
        let wt_dir = d.path().join("repo-feat");
        std::fs::create_dir_all(&repo).unwrap();
        let run = |a: &[&str]| assert!(git(&repo, a).unwrap().status.success(), "{a:?}");
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(repo.join("a.txt"), "1\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-qm", "init"]);
        run(&["worktree", "add", "-b", "feat/x", &wt_dir.to_string_lossy()]);

        let from_main = worktrees(&repo.to_string_lossy()).unwrap();
        let list = from_main["worktrees"].as_array().unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0]["isMain"], true);
        assert_eq!(list[0]["isCurrent"], true);
        assert_eq!(list[0]["branch"], "main");
        assert_eq!(list[1]["isMain"], false);
        assert_eq!(list[1]["isCurrent"], false);
        assert_eq!(list[1]["branch"], "feat/x");

        let from_wt = worktrees(&wt_dir.to_string_lossy()).unwrap();
        let list2 = from_wt["worktrees"].as_array().unwrap();
        assert_eq!(list2[0]["isCurrent"], false);
        assert_eq!(list2[1]["isCurrent"], true);
    }

    #[test]
    fn branches_checkout_push_and_branch_diff() {
        let d = tempfile::tempdir().unwrap();
        let remote = d.path().join("remote.git");
        let repo = d.path().join("repo");
        std::fs::create_dir_all(&remote).unwrap();
        std::fs::create_dir_all(&repo).unwrap();
        assert!(git(&remote, &["init", "--bare", "-q"]).unwrap().status.success());
        let run = |a: &[&str]| assert!(git(&repo, a).unwrap().status.success(), "{a:?}");
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        run(&["remote", "add", "origin", &remote.to_string_lossy()]);
        std::fs::write(repo.join("base.txt"), "v1\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-qm", "init"]);
        let cwd = repo.to_string_lossy().to_string();

        // Push initial main branch
        let p1 = push(&cwd, None, None, true).unwrap();
        assert_eq!(p1["branch"], "main");
        assert_eq!(p1["remote"], "origin");

        // Create and switch to feature branch
        assert!(checkout(&cwd, "--bad", true, None).is_err());
        let co = checkout(&cwd, "feat/demo", true, None).unwrap();
        assert_eq!(co["branch"], "feat/demo");

        let br = branches(&cwd).unwrap();
        assert_eq!(br["current"], "feat/demo");
        assert_eq!(br["defaultBranch"], "main");
        assert_eq!(br["branches"].as_array().unwrap().len(), 2);

        // Committed change + unstaged edit + untracked file
        std::fs::write(repo.join("base.txt"), "v2\n").unwrap();
        run(&["add", "base.txt"]);
        run(&["commit", "-qm", "update base"]);
        std::fs::write(repo.join("extra.txt"), "hello\nworld\n").unwrap();

        let bd = branch_diff(&cwd, None, false, None).unwrap();
        assert_eq!(bd["base"], "main");
        assert!(bd["mergeBase"].as_str().is_some());
        let files = bd["files"].as_array().unwrap();
        assert_eq!(files.len(), 2);
        let base_f = files.iter().find(|f| f["path"] == "base.txt").unwrap();
        assert_eq!((base_f["added"].as_u64(), base_f["deleted"].as_u64()), (Some(1), Some(1)));
        let extra_f = files.iter().find(|f| f["path"] == "extra.txt").unwrap();
        assert_eq!(extra_f["status"], "new");
        assert_eq!((extra_f["added"].as_u64(), extra_f["deleted"].as_u64()), (Some(2), Some(0)));

        let one = branch_diff(&cwd, Some("main"), true, Some(&["base.txt".into()])).unwrap();
        assert_eq!(one["files"].as_array().unwrap().len(), 1);
        assert!(one["files"][0]["diff"].as_str().unwrap().contains("+v2"));

        // Push feature branch
        let p2 = push(&cwd, None, None, true).unwrap();
        assert_eq!(p2["branch"], "feat/demo");
        assert!(branch_diff(&cwd, Some("--bad"), false, None).is_err());
    }

    #[test]
    fn checkpoint_capture_diff_and_preserves_real_index() {
        let d = tempfile::tempdir().unwrap();
        let repo = d.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let run = |a: &[&str]| assert!(git(&repo, a).unwrap().status.success(), "{a:?}");
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        run(&["config", "core.autocrlf", "false"]);
        std::fs::write(repo.join("a.txt"), "line1\nline2\n").unwrap();
        std::fs::write(repo.join("staged.txt"), "before\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-qm", "init"]);

        // Stage a change in the real .git/index so we verify checkpoints never disturb staged changes
        std::fs::write(repo.join("staged.txt"), "staged-v2\n").unwrap();
        run(&["add", "staged.txt"]);

        let cwd = repo.to_string_lossy().to_string();
        let r_base = "refs/yi/checkpoints/s1/t1-base";
        let r_end = "refs/yi/checkpoints/s1/t1-end";
        let t_base = capture_checkpoint(&cwd, r_base).unwrap().unwrap();

        // Simulate an AI turn editing a.txt and creating an untracked nested file
        std::fs::write(repo.join("a.txt"), "line1\nline2-edited\nline3\n").unwrap();
        std::fs::create_dir_all(repo.join("sub")).unwrap();
        std::fs::write(repo.join("sub/new.txt"), "hello\nworld\n").unwrap();

        let t_end = capture_checkpoint(&cwd, r_end).unwrap().unwrap();
        assert_ne!(t_base, t_end);

        // Real staged index must be untouched
        let cached = git_ok(&repo, &["diff", "--cached", "--name-only"]).unwrap();
        assert_eq!(cached.trim(), "staged.txt");

        // Exact turn diff stats & ref_diff
        let (fc, add, del) = diff_stat_refs(&cwd, r_base, r_end).unwrap();
        assert_eq!((fc, add, del), (2, 4, 1));
        let rd = ref_diff(&cwd, r_base, r_end, true, None).unwrap();
        let files = rd["files"].as_array().unwrap();
        assert_eq!(files.len(), 2);
        let nf = files.iter().find(|f| f["path"] == "sub/new.txt").unwrap();
        assert_eq!(nf["status"], "new");
        assert!(nf["diff"].as_str().unwrap().contains("+hello"));

        // git.blob reads hidden checkpoint refs cleanly
        let b_old = blob(&cwd, "a.txt", r_base).unwrap();
        let b_new = blob(&cwd, "a.txt", r_end).unwrap();
        assert_eq!(b_old["text"], "line1\nline2\n");
        assert_eq!(b_new["text"], "line1\nline2-edited\nline3\n");
        let b_missing = blob(&cwd, "sub/new.txt", r_base).unwrap();
        assert_eq!(b_missing["exists"], false);

        // Cleanup removes all refs for s1
        delete_session_checkpoints(&cwd, "s1");
        assert!(!git(&repo, &["rev-parse", "--verify", "-q", r_base]).unwrap().status.success());
        assert!(!git(&repo, &["rev-parse", "--verify", "-q", r_end]).unwrap().status.success());
    }
}
