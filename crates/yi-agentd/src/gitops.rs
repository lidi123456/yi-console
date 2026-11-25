//! git operations for summarizing session changes: which files changed and their diffs, discarding changes, committing.

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};

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
    // Clear inherited pathspec env vars so per-path `:(literal)` stays exact and case-sensitive,
    // commands that reject pathspec magic (`ls-tree`, `check-ignore`) never fail, and hooks never inherit them.
    cmd.env_remove("GIT_LITERAL_PATHSPECS");
    cmd.env_remove("GIT_ICASE_PATHSPECS");
    cmd.env_remove("GIT_GLOB_PATHSPECS");
    cmd.env_remove("GIT_NOGLOB_PATHSPECS");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    cmd
}

/// Prefix a repo-relative path with `:(literal)` so Git treats glob characters (`*`, `?`, `[...]`)
/// and leading `:` literally without leaking `GIT_LITERAL_PATHSPECS=1` into git hooks.
fn literal_pathspec(path: &str) -> String {
    format!(":(literal){path}")
}

pub(crate) fn git(cwd: &Path, args: &[&str]) -> Result<std::process::Output> {
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
    // Write stdin on a scoped thread while wait_with_output drains stdout/stderr so large payloads cannot deadlock.
    let stdin = child.stdin.take();
    std::thread::scope(|s| {
        let writer = s.spawn(move || {
            if let Some(mut stdin) = stdin {
                let _ = stdin.write_all(input);
            }
        });
        let out = child
            .wait_with_output()
            .map_err(|e| anyhow!("等待 git 执行失败: {e}"));
        let _ = writer.join();
        out
    })
}

fn git_stdin_ok(cwd: &Path, args: &[&str], envs: &[(&str, &str)], input: &[u8]) -> Result<String> {
    let out = git_stdin(cwd, args, envs, input)?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let msg = if !stderr.is_empty() { stderr } else { stdout };
        bail!("{msg}");
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
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

pub(crate) fn git_ok(cwd: &Path, args: &[&str]) -> Result<String> {
    let out = git(cwd, args)?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Repository root; None if not a git repo
pub(crate) fn repo_root(cwd: &Path) -> Option<std::path::PathBuf> {
    git_ok(cwd, &["rev-parse", "--show-toplevel"])
        .ok()
        .map(|s| std::path::PathBuf::from(s.trim()))
}

/// Main working tree root of the repository containing `cwd` (resolves linked worktrees back to the primary checkout)
pub fn main_repo_root(cwd: &Path) -> Option<std::path::PathBuf> {
    let root = repo_root(cwd)?;
    if let Ok(out) = git_ok(&root, &["worktree", "list", "--porcelain"]) {
        for line in out.lines() {
            if let Some(raw) = line.strip_prefix("worktree ") {
                let p = std::path::PathBuf::from(native_worktree_path(raw.trim()));
                if p.is_dir() {
                    return Some(p);
                }
                break;
            }
        }
    }
    Some(root)
}

/// Normalize a tool-reported file path (relative to `cwd` or absolute) into a repo-relative forward-slash path
pub fn normalize_repo_rel(cwd: &str, raw: &str) -> Option<String> {
    let dir = crate::paths::expand(cwd);
    let root = repo_root(&dir)?;
    let root_canon = root.canonicalize().unwrap_or_else(|_| root.clone());
    let root_key = norm_path_key(&root_canon.to_string_lossy());
    let raw_trimmed = raw.trim();
    if raw_trimmed.is_empty() {
        return None;
    }
    let p = Path::new(raw_trimmed);
    let joined = if p.is_absolute() {
        // Canonicalize the deepest existing ancestor so symlinked roots (e.g. /tmp -> /private/tmp) match `root_canon`.
        let mut cur = p.to_path_buf();
        let mut tail: Vec<std::ffi::OsString> = Vec::new();
        while !cur.exists() {
            let Some(name) = cur.file_name() else { break };
            tail.push(name.to_os_string());
            if !cur.pop() {
                break;
            }
        }
        let mut base = cur.canonicalize().unwrap_or_else(|_| p.to_path_buf());
        if cur.exists() {
            for seg in tail.into_iter().rev() {
                base.push(seg);
            }
            base
        } else {
            p.to_path_buf()
        }
    } else {
        let dir_canon = dir.canonicalize().unwrap_or(dir);
        dir_canon.join(p)
    };
    let mut comps: Vec<String> = Vec::new();
    let s = joined.to_string_lossy();
    let s = s.strip_prefix(r"\\?\").unwrap_or(&s).replace('\\', "/");
    let has_leading_slash = s.starts_with('/');
    for seg in s.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                if comps.pop().is_none() {
                    return None;
                }
            }
            other => comps.push(other.to_string()),
        }
    }
    let cleaned = if has_leading_slash {
        format!("/{}", comps.join("/"))
    } else {
        comps.join("/")
    };
    let cleaned_key = if cfg!(windows) {
        cleaned.to_ascii_lowercase()
    } else {
        cleaned.clone()
    };
    let prefix = format!("{root_key}/");
    if !cleaned_key.starts_with(&prefix) {
        return None;
    }
    let rel = cleaned[prefix.len()..].to_string();
    if rel.is_empty() || check_paths(std::slice::from_ref(&rel)).is_err() {
        return None;
    }
    Some(rel)
}

/// Paths must stay inside the repo, so things like ../ or absolute paths can't escape it.
/// Leading `-` filenames (e.g. `-draft.txt`) are allowed and always passed after `--` with `:(literal)` pathspecs.
pub(crate) fn check_paths(paths: &[String]) -> Result<()> {
    use std::path::Component;
    for p in paths {
        if p.is_empty()
            || p.contains('\0')
            || p.starts_with(['/', '\\'])
            || Path::new(p).is_absolute()
            || Path::new(p).components().any(|c| matches!(c, Component::Prefix(_) | Component::RootDir | Component::ParentDir))
            || p.split(['/', '\\']).any(|c| c == "..")
        {
            bail!("路径无效: {p}");
        }
    }
    Ok(())
}

/// Working tree changes relative to HEAD. Paths are relative to the repo root
pub fn changes(cwd: &str) -> Result<Value> {
    changes_with(cwd, true, None)
}

/// Expand requested paths with any staged rename/copy counterpart so `git status -- <paths>` still detects `R`/`C`.
fn expand_staged_rename_paths(root: &Path, paths: &[String]) -> Vec<String> {
    let mut out: Vec<String> = paths.to_vec();
    let Ok(diff) = git_ok(root, &["diff", "--cached", "--diff-filter=RC", "--name-status", "-M", "-z"]) else {
        return out;
    };
    let mut it = diff.split('\0').filter(|s| !s.is_empty());
    while let Some(status) = it.next() {
        if !(status.starts_with('R') || status.starts_with('C')) {
            continue;
        }
        let Some(old_path) = it.next() else { break };
        let Some(new_path) = it.next() else { break };
        if paths.iter().any(|p| p == old_path || p == new_path) {
            if !out.iter().any(|p| p == old_path) {
                out.push(old_path.to_string());
            }
            if !out.iter().any(|p| p == new_path) {
                out.push(new_path.to_string());
            }
        }
    }
    out
}

/// with_diff=false only lists statuses without diffs (running git diff per file is slow in a big repo with many changes);
/// if only is given, return just those paths (relative to the repo root), used to fetch one file's diff
pub fn changes_with(cwd: &str, with_diff: bool, only: Option<&[String]>) -> Result<Value> {
    if let Some(o) = only {
        check_paths(o)?;
    }
    let dir = crate::paths::expand(cwd);
    let Some(root) = repo_root(&dir) else {
        return Ok(json!({ "repo": null, "files": [], "ignored": [] }));
    };
    let branch = git_ok(&root, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_default();
    // Current commit: the frontend refreshes the commit history when it changes (null in a repo with no commits yet)
    let head = git_ok(&root, &["rev-parse", "--verify", "-q", "HEAD"]).ok().map(|s| s.trim().to_string());
    if only.is_some_and(|o| o.is_empty()) {
        return Ok(json!({
            "repo": root.display().to_string(),
            "branch": branch.trim(),
            "head": head,
            "total": Value::Null,
            "files": [],
            "ignored": [],
        }));
    }
    let expanded_only: Option<Vec<String>> = only
        .filter(|p| !p.is_empty())
        .map(|p| expand_staged_rename_paths(&root, p));
    let effective_only = expanded_only.as_deref().or(only);
    // Include ignored entries only for full listings; --ignored=matching stops at ignored directory boundaries
    // (avoiding a walk into node_modules/target), falling back for very old git (< 2.16) that lacks it
    let status = match effective_only {
        None => git_ok(&root, &["status", "--porcelain=v1", "-b", "-z", "-uall", "--ignored=matching"])
            .or_else(|_| git_ok(&root, &["status", "--porcelain=v1", "-b", "-z", "-uall"]))?,
        Some(paths) => {
            let lit: Vec<String> = paths.iter().map(|p| literal_pathspec(p)).collect();
            let mut args = vec!["status", "--porcelain=v1", "-z", "-uall", "--"];
            args.extend(lit.iter().map(String::as_str));
            git_ok(&root, &args)?
        }
    };
    let stats = numstat(&root, effective_only);
    let root_canon = root.canonicalize().ok();
    let mut files = vec![];
    let mut ignored = vec![];
    let mut ahead: Option<u64> = None;
    let mut behind: Option<u64> = None;
    let (mut total, mut counted) = (0usize, 0usize);
    let mut it = status.split('\0').filter(|s| !s.is_empty());
    while let Some(entry) = it.next() {
        if entry.starts_with("## ") {
            let (a, b) = parse_status_branch_track(entry);
            ahead = a;
            behind = b;
            continue;
        }
        if entry.len() < 4 {
            continue;
        }
        let (xy_raw, path) = entry.split_at(3);
        let xy = &xy_raw[..2];
        let code = xy.trim().to_string();
        if code == "!!" {
            if ignored.len() < IGNORED_MAX {
                ignored.push(path.to_string());
            }
            continue;
        }
        total += 1;
        let xb = xy.as_bytes()[0];
        let yb = xy.as_bytes()[1];
        let is_rename = xb == b'R' || yb == b'R';
        let is_copy = xb == b'C' || yb == b'C';
        // Rename/copy entries are followed by the original path; only renames carry oldPath so copies never delete their source.
        let old_path: Option<String> = if is_rename || is_copy {
            let orig = it.next();
            if is_rename {
                orig.map(str::to_string)
            } else {
                None
            }
        } else {
            None
        };
        if only.is_some_and(|o| !o.iter().any(|x| x == path || old_path.as_deref() == Some(x.as_str())))
            || files.len() >= STATUS_MAX
        {
            continue;
        }
        let untracked = code == "??";
        let diff = if !with_diff {
            String::new()
        } else if untracked {
            untracked_diff(&root, root_canon.as_deref(), path)
        } else {
            let d = if let Some(ref old) = old_path {
                let lit_old = literal_pathspec(old);
                let lit_path = literal_pathspec(path);
                git_ok(&root, &["diff", "HEAD", "-M", "--no-color", "--", &lit_old, &lit_path]).unwrap_or_default()
            } else {
                let lit_path = literal_pathspec(path);
                git_ok(&root, &["diff", "HEAD", "--no-color", "--", &lit_path]).unwrap_or_default()
            };
            if d.len() > DIFF_MAX {
                String::new()
            } else {
                d
            }
        };
        // Lines added / removed: null for binaries, files too large, and repos without HEAD
        let (added, deleted) = if untracked {
            counted += 1;
            (if counted <= COUNT_MAX { count_lines(&root, root_canon.as_deref(), path) } else { None }, Some(0))
        } else {
            stats.get(path).copied().unwrap_or((None, None))
        };
        let mut item = json!({
            "path": path,
            "status": if untracked { "new".to_string() } else { code },
            "diff": diff,
            "added": added,
            "deleted": deleted,
        });
        if let Some(old) = old_path {
            item["oldPath"] = json!(old);
        }
        files.push(item);
    }
    let mut res = json!({
        "repo": root.display().to_string(),
        "branch": branch.trim(),
        "head": head,
        // Total number of changed files, set only when more than STATUS_MAX were left out
        "total": if only.is_none() && total > files.len() { Some(total) } else { None },
        "files": files,
        // Ignored paths relative to the repo root (directories end with '/')
        "ignored": ignored,
    });
    if only.is_none() {
        res["ahead"] = json!(ahead);
        res["behind"] = json!(behind);
    }
    Ok(res)
}

/// Lines added/removed relative to HEAD for tracked files, computed in one git diff --numstat (no per-file diff runs);
/// git reports "-" for binary files, recorded as None
fn numstat(root: &Path, only: Option<&[String]>) -> std::collections::HashMap<String, (Option<u64>, Option<u64>)> {
    let mut m = std::collections::HashMap::new();
    let mut args = vec!["diff", "HEAD", "--numstat", "-z", "--no-renames"];
    let lit: Vec<String> = only
        .filter(|p| !p.is_empty())
        .map(|paths| paths.iter().map(|p| literal_pathspec(p)).collect())
        .unwrap_or_default();
    if !lit.is_empty() {
        args.push("--");
        args.extend(lit.iter().map(String::as_str));
    }
    let Ok(out) = git_ok(root, &args) else {
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

/// Line count of a new file; symlinks count as 1 without following; not counted for files too large, binary, or outside the repo
fn count_lines(root: &Path, can_root: Option<&Path>, rel: &str) -> Option<u64> {
    let full = root.join(rel);
    let meta = std::fs::symlink_metadata(&full).ok()?;
    if meta.file_type().is_symlink() {
        return Some(1);
    }
    if !meta.is_file() || meta.len() as usize > DIFF_MAX {
        return None;
    }
    let owned_root;
    let can_root = match can_root {
        Some(r) => r,
        None => {
            owned_root = root.canonicalize().ok()?;
            &owned_root
        }
    };
    let can_full = full.canonicalize().ok()?;
    if !can_full.starts_with(can_root) {
        return None;
    }
    let b = std::fs::read(&can_full).ok()?;
    if b.iter().take(8000).any(|&c| c == 0) {
        return None;
    }
    let n = b.iter().filter(|&&c| c == b'\n').count() + usize::from(b.last().is_some_and(|&c| c != b'\n'));
    Some(n as u64)
}

/// Field / record separators for git log --format (control characters that can't appear in a commit subject)
const FS: char = '\x1f';
const RS: char = '\x1e';

fn parse_log_records(out: &str) -> Vec<Value> {
    out.split(RS)
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
        .collect()
}

/// Commit history of HEAD, newest first; skip/limit page through it, more says whether there are older commits
pub fn log(cwd: &str, skip: usize, limit: usize) -> Result<Value> {
    log_with(cwd, skip, limit, None)
}

/// Commit history of HEAD with optional case-insensitive search across subject, author, hash, and refs
pub fn log_with(cwd: &str, skip: usize, limit: usize, query: Option<&str>) -> Result<Value> {
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
    let q = query.map(str::trim).filter(|s| !s.is_empty());
    let Some(q) = q else {
        let (skip_arg, n_arg) = (format!("--skip={skip}"), format!("-n{}", limit + 1));
        let out = git_ok(&root, &["log", &fmt, &skip_arg, &n_arg, "HEAD", "--"])?;
        let mut commits = parse_log_records(&out);
        let more = commits.len() > limit;
        commits.truncate(limit);
        return Ok(json!({ "repo": repo, "commits": commits, "more": more }));
    };

    let q_lower = q.to_lowercase();
    let out = git_ok(&root, &["log", &fmt, "-n500", "HEAD", "--"])?;
    let mut matched: Vec<Value> = parse_log_records(&out)
        .into_iter()
        .filter(|c| {
            let hit = |k: &str| c[k].as_str().is_some_and(|s| s.to_lowercase().contains(&q_lower));
            hit("subject")
                || hit("author")
                || hit("hash")
                || hit("short")
                || c["refs"]
                    .as_array()
                    .is_some_and(|arr| arr.iter().any(|r| r.as_str().is_some_and(|s| s.to_lowercase().contains(&q_lower))))
        })
        .collect();

    // If the user pasted a valid commit hash outside the latest 500 commits, resolve and prepend it
    if q.len() >= 4 && q.len() <= 64 && q.bytes().all(|b| b.is_ascii_hexdigit()) {
        if let Ok(full) = git_ok(&root, &["rev-parse", "--verify", "-q", &format!("{q}^{{commit}}")]) {
            let full = full.trim();
            if !full.is_empty() && !matched.iter().any(|c| c["hash"].as_str() == Some(full)) {
                if let Ok(one) = git_ok(&root, &["log", "-1", &fmt, full, "--"]) {
                    if let Some(item) = parse_log_records(&one).into_iter().next() {
                        matched.insert(0, item);
                    }
                }
            }
        }
    }

    let more = matched.len() > skip.saturating_add(limit);
    let commits: Vec<Value> = matched.into_iter().skip(skip).take(limit).collect();
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

    let mut files = vec![];
    if !only.is_some_and(|o| o.is_empty()) {
        let mut num_args = vec!["diff", "--numstat", "-z", "--no-renames", &base, &full];
        let mut name_args = vec!["diff", "--name-status", "-z", "--no-renames", &base, &full];
        let lit: Vec<String> = only
            .filter(|p| !p.is_empty())
            .map(|paths| paths.iter().map(|p| literal_pathspec(p)).collect())
            .unwrap_or_default();
        if !lit.is_empty() {
            num_args.push("--");
            num_args.extend(lit.iter().map(String::as_str));
            name_args.push("--");
            name_args.extend(lit.iter().map(String::as_str));
        }
        let mut stats = std::collections::HashMap::new();
        for rec in git_ok(&root, &num_args)?.split('\0').filter(|s| !s.is_empty()) {
            let mut f = rec.splitn(3, '\t');
            if let (Some(a), Some(d), Some(path)) = (f.next(), f.next(), f.next()) {
                stats.insert(path.to_string(), (a.parse::<u64>().ok(), d.parse::<u64>().ok()));
            }
        }
        let names = git_ok(&root, &name_args)?;
        let mut parts = names.split('\0').filter(|s| !s.is_empty());
        while let (Some(code), Some(path)) = (parts.next(), parts.next()) {
            if only.is_some_and(|o| !o.iter().any(|x| x == path)) {
                continue;
            }
            let diff = if only.is_some() {
                let lit_path = literal_pathspec(path);
                let d = git_ok(&root, &["diff", "--no-color", &base, &full, "--", &lit_path]).unwrap_or_default();
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

fn is_symlink_or_reparse(m: &std::fs::Metadata) -> bool {
    if m.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        if m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return true;
        }
    }
    false
}

fn remove_file_readonly_retry(p: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(p) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            if let Ok(m) = std::fs::symlink_metadata(p) {
                let mut perms = m.permissions();
                if perms.readonly() {
                    perms.set_readonly(false);
                    let _ = std::fs::set_permissions(p, perms);
                    return std::fs::remove_file(p);
                }
            }
            Err(e)
        }
        Err(e) => Err(e),
    }
}

/// Split path lists by both item count and byte length so git never hits Windows CreateProcess command-line limits
fn chunk_paths<'a>(paths: &'a [&'a str]) -> Vec<&'a [&'a str]> {
    const MAX_COUNT: usize = 200;
    const MAX_BYTES: usize = 16_000;
    let mut chunks = Vec::new();
    let mut start = 0;
    let mut bytes = 0usize;
    for (i, p) in paths.iter().enumerate() {
        let cost = p.len() + 3;
        if i > start && (i - start >= MAX_COUNT || bytes + cost > MAX_BYTES) {
            chunks.push(&paths[start..i]);
            start = i;
            bytes = 0;
        }
        bytes += cost;
    }
    if start < paths.len() {
        chunks.push(&paths[start..]);
    }
    chunks
}

/// Discard changes: tracked files are restored to HEAD, new files are deleted and emptied untracked dirs are pruned
pub fn discard(cwd: &str, paths: &[String]) -> Result<()> {
    check_paths(paths)?;
    let mut seen = HashSet::new();
    let mut norm_paths = Vec::with_capacity(paths.len());
    for raw in paths {
        let p = raw.replace('\\', "/");
        let p = p.trim_end_matches('/').to_string();
        if p.is_empty() {
            continue;
        }
        if p.split('/').any(|seg| seg == ".git") {
            bail!("路径无效: {raw}");
        }
        if seen.insert(p.clone()) {
            norm_paths.push(p);
        }
    }
    let dir = crate::paths::expand(cwd);
    let root = repo_root(&dir).ok_or_else(|| anyhow!("不是 git 仓库"))?;
    if norm_paths.is_empty() {
        return Ok(());
    }
    let lit_paths: Vec<String> = norm_paths.iter().map(|p| literal_pathspec(p)).collect();
    let refs: Vec<&str> = lit_paths.iter().map(String::as_str).collect();
    let has_head = git(&root, &["rev-parse", "--verify", "-q", "HEAD"])?.status.success();

    let mut in_head = HashSet::new();
    if has_head {
        for chunk in chunk_paths(&refs) {
            let mut args = vec!["ls-tree", "-r", "--name-only", "-z", "HEAD", "--"];
            args.extend_from_slice(chunk);
            let out = git_ok(&root, &args)?;
            for item in out.split('\0').filter(|s| !s.is_empty()) {
                in_head.insert(item.to_string());
            }
        }
    }

    let mut tracked = HashSet::new();
    for chunk in chunk_paths(&refs) {
        let mut args = vec!["ls-files", "-z", "--"];
        args.extend_from_slice(chunk);
        let out = git_ok(&root, &args)?;
        for item in out.split('\0').filter(|s| !s.is_empty()) {
            tracked.insert(item.to_string());
        }
    }

    let mut restore_head: Vec<&str> = Vec::new();
    let mut unstage_new: Vec<&str> = Vec::new();
    let mut remove_disk: Vec<&str> = Vec::new();
    for (p, lit) in norm_paths.iter().zip(lit_paths.iter()) {
        if in_head.contains(p) {
            restore_head.push(lit.as_str());
        } else {
            if tracked.contains(p) {
                unstage_new.push(lit.as_str());
            }
            remove_disk.push(p.as_str());
        }
    }

    for chunk in chunk_paths(&unstage_new) {
        let mut args = vec!["rm", "--cached", "-f", "-q", "--"];
        args.extend_from_slice(chunk);
        git_ok(&root, &args)?;
    }

    let mut candidate_dirs: HashSet<String> = HashSet::new();
    for p in remove_disk {
        let segs: Vec<&str> = p.split('/').collect();
        let mut cur = root.clone();
        let mut symlink_parent = false;
        for seg in &segs[..segs.len().saturating_sub(1)] {
            cur.push(seg);
            match std::fs::symlink_metadata(&cur) {
                Ok(m) if is_symlink_or_reparse(&m) => {
                    symlink_parent = true;
                    break;
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        if symlink_parent {
            continue;
        }
        let full = root.join(p);
        if let Ok(m) = std::fs::symlink_metadata(&full) {
            if is_symlink_or_reparse(&m) {
                std::fs::remove_file(&full).or_else(|_| std::fs::remove_dir(&full))?;
            } else if m.is_file() {
                remove_file_readonly_retry(&full)?;
            }
        }
        let mut rel = p;
        while let Some(idx) = rel.rfind('/') {
            rel = &rel[..idx];
            if !rel.is_empty() {
                candidate_dirs.insert(rel.to_string());
            }
        }
    }

    let mut dirs_sorted: Vec<String> = candidate_dirs.into_iter().collect();
    dirs_sorted.sort_by(|a, b| {
        b.matches('/')
            .count()
            .cmp(&a.matches('/').count())
            .then_with(|| b.len().cmp(&a.len()))
    });
    for rel_dir in dirs_sorted {
        if rel_dir.is_empty() || rel_dir.split('/').any(|seg| seg == ".git") {
            continue;
        }
        let d = root.join(&rel_dir);
        if let Ok(m) = std::fs::symlink_metadata(&d) {
            if m.is_dir() && !is_symlink_or_reparse(&m) {
                let _ = std::fs::remove_dir(&d);
            }
        }
    }

    for chunk in chunk_paths(&restore_head) {
        let mut args = vec!["checkout", "HEAD", "--"];
        args.extend_from_slice(chunk);
        git_ok(&root, &args)?;
    }
    Ok(())
}

const WINDOWS_CMDLINE_MAX: usize = 32_767;

fn parse_git_version(raw: &str) -> Option<(u32, u32)> {
    let token = raw
        .split_whitespace()
        .find(|t| t.as_bytes().first().is_some_and(u8::is_ascii_digit))?;
    let mut parts = token.split('.');
    let major: u32 = parts.next()?.parse().ok()?;
    let minor: u32 = parts.next()?.parse().ok()?;
    Some((major, minor))
}

fn supports_pathspec_from_file(cwd: &Path) -> bool {
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        git_ok(cwd, &["--version"])
            .ok()
            .and_then(|s| parse_git_version(&s))
            .is_some_and(|v| v >= (2, 26))
    })
}

fn nul_pathspec_payload(lit_pathspecs: &[&str]) -> Vec<u8> {
    let cap: usize = lit_pathspecs.iter().map(|p| p.len() + 1).sum();
    let mut out = Vec::with_capacity(cap);
    for p in lit_pathspecs {
        out.extend_from_slice(p.as_bytes());
        out.push(0);
    }
    out
}

fn estimate_cmdline_utf16(args: &[&str]) -> usize {
    // Headroom for `"git" -c core.quotepath=false ` and trailing NUL in Windows CreateProcessW.
    let mut total = 64usize;
    for arg in args {
        total = total.saturating_add(
            arg.encode_utf16()
                .count()
                .saturating_add(arg.bytes().filter(|&b| b == b'"' || b == b'\\').count())
                .saturating_add(3),
        );
    }
    total
}

fn check_commit_cmdline_limits(add_args: Option<&[&str]>, commit_args: &[&str], limit: usize) -> Result<()> {
    let add_over = add_args.is_some_and(|args| estimate_cmdline_utf16(args) > limit);
    let commit_over = estimate_cmdline_utf16(commit_args) > limit;
    if add_over || commit_over {
        bail!("选中文件过多，超出当前 Git 版本的命令行长度限制，请分批提交或升级 Git");
    }
    Ok(())
}

/// Commit only the selected files (leaving everything else in the index alone)
pub fn commit(cwd: &str, paths: &[String], message: &str) -> Result<Value> {
    commit_with_options(
        cwd,
        paths,
        message,
        None,
        if cfg!(windows) { Some(WINDOWS_CMDLINE_MAX) } else { None },
    )
}

fn commit_with_options(
    cwd: &str,
    paths: &[String],
    message: &str,
    force_pathspec_from_file: Option<bool>,
    fallback_cmdline_limit: Option<usize>,
) -> Result<Value> {
    check_paths(paths)?;
    if paths.is_empty() {
        bail!("没有选文件");
    }
    let msg = message.trim();
    if msg.is_empty() {
        bail!("提交说明不能为空");
    }
    let dir = crate::paths::expand(cwd);
    let root = repo_root(&dir).ok_or_else(|| anyhow!("不是 git 仓库"))?;
    let lit_paths: Vec<String> = paths.iter().map(|p| literal_pathspec(p)).collect();
    let lit_refs: Vec<&str> = lit_paths.iter().map(String::as_str).collect();
    let mut to_add: Vec<&str> = Vec::new();
    for (p, lit) in paths.iter().zip(lit_refs.iter().copied()) {
        // Staged deletions and rename `oldPath` entries are already removed from both disk and the index;
        // passing them to `git add -A -- <path>` fails with `did not match any files`.
        let exists = std::fs::symlink_metadata(root.join(p)).is_ok();
        let in_index = !exists && git(&root, &["ls-files", "--error-unmatch", "--", lit])?.status.success();
        if exists || in_index {
            to_add.push(lit);
        }
    }
    let use_from_file = force_pathspec_from_file.unwrap_or_else(|| supports_pathspec_from_file(&root));
    if use_from_file {
        if !to_add.is_empty() {
            let payload = nul_pathspec_payload(&to_add);
            git_stdin_ok(
                &root,
                &["add", "-A", "--pathspec-from-file=-", "--pathspec-file-nul"],
                &[],
                &payload,
            )?;
        }
        let payload = nul_pathspec_payload(&lit_refs);
        git_stdin_ok(
            &root,
            &["commit", "-m", msg, "--pathspec-from-file=-", "--pathspec-file-nul"],
            &[],
            &payload,
        )?;
    } else {
        let add_args = if to_add.is_empty() {
            None
        } else {
            let mut args = vec!["add", "-A", "--"];
            args.extend_from_slice(&to_add);
            Some(args)
        };
        let mut commit_args = vec!["commit", "-m", msg, "--"];
        commit_args.extend_from_slice(&lit_refs);
        if let Some(limit) = fallback_cmdline_limit {
            check_commit_cmdline_limits(add_args.as_deref(), &commit_args, limit)?;
        }
        if let Some(args) = add_args {
            git_ok(&root, &args)?;
        }
        git_ok(&root, &commit_args)?;
    }
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

/// Process-wide lock serializing `git branch -m` writes to shared `.git/config` across worktrees
static GIT_BRANCH_RENAME_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

/// Sanitize a user-configured branch prefix (e.g. `"yi/"` -> `"yi"`, `"feat/team/"` -> `"feat/team"`, `""` -> `""`)
pub fn sanitize_branch_prefix(raw: &str) -> String {
    raw.split(['/', '\\'])
        .filter_map(|seg| {
            let mut out = String::new();
            let mut last_dash = false;
            for ch in seg.trim().chars() {
                if ch.is_ascii_alphanumeric() || ch == '_' {
                    out.push(ch);
                    last_dash = false;
                } else if !out.is_empty() && !last_dash {
                    out.push('-');
                    last_dash = true;
                }
            }
            let trimmed = out.trim_matches(['-', '.']);
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Effective branch prefix from `DaemonSettings` (`None` -> `"yi"`, `Some("")` -> `""`)
pub fn effective_worktree_branch_prefix(settings: &yi_proto::DaemonSettings) -> String {
    match settings.worktree_branch_prefix.as_deref() {
        None => "yi".to_string(),
        Some(raw) => sanitize_branch_prefix(raw),
    }
}

/// Combine an optional sanitized prefix and a branch slug without duplicating an existing prefix
pub fn format_worktree_branch(prefix: &str, slug: &str) -> String {
    let clean_prefix = sanitize_branch_prefix(prefix);
    let clean_slug = slug.trim().trim_matches('/');
    if clean_prefix.is_empty() || clean_slug.starts_with(&format!("{clean_prefix}/")) {
        clean_slug.to_string()
    } else if clean_slug.is_empty() {
        clean_prefix
    } else {
        format!("{clean_prefix}/{clean_slug}")
    }
}

fn is_cjk_char(c: char) -> bool {
    matches!(c, '\u{4e00}'..='\u{9fff}' | '\u{3040}'..='\u{30ff}' | '\u{ac00}'..='\u{d7af}')
}

fn join_slug_tokens(tokens: &[String], max_tokens: usize, max_len: usize) -> Option<String> {
    let mut out = String::new();
    for tok in tokens.iter().take(max_tokens) {
        if out.is_empty() {
            if tok.len() > max_len {
                out.push_str(&tok[..max_len]);
                break;
            }
            out.push_str(tok);
        } else if out.len() + 1 + tok.len() <= max_len {
            out.push('-');
            out.push_str(tok);
        } else {
            break;
        }
    }
    let out = out.trim_matches('-').to_string();
    (out.len() >= 3).then_some(out)
}

/// Normalize free text or an LLM branch candidate into a concise lowercase ASCII kebab-case slug
pub fn sanitize_branch_slug(raw: &str) -> Option<String> {
    const FILLERS: &[&str] = &[
        "a", "an", "the", "in", "on", "at", "to", "for", "of", "by", "with", "and", "or", "is",
        "are", "be", "this", "that", "it", "when", "after", "before", "from", "into", "please",
        "can", "you", "help", "me", "my", "our", "we", "i",
    ];
    const STOP_SINGLE: &[&str] = &[
        "the", "fix", "add", "test", "tests", "new", "ok", "hi", "hello", "ai", "pr", "help",
        "please", "update", "check", "bug", "code", "file", "files", "issue",
    ];

    let line = raw
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with("```"))?;
    let mut tokens: Vec<String> = Vec::new();
    for word in line.split_whitespace() {
        let low_raw = word.trim_matches(|c: char| "`'\"()[]{}<>,:;!?".contains(c));
        if low_raw.starts_with("http://") || low_raw.starts_with("https://") {
            continue;
        }
        let mut cur = String::new();
        for ch in low_raw.chars() {
            if ch.is_ascii_alphanumeric() {
                cur.push(ch.to_ascii_lowercase());
            } else if !cur.is_empty() {
                tokens.push(std::mem::take(&mut cur));
            }
        }
        if !cur.is_empty() {
            tokens.push(cur);
        }
    }
    if tokens.len() > 3 {
        let filtered: Vec<String> = tokens
            .iter()
            .filter(|t| !FILLERS.contains(&t.as_str()))
            .cloned()
            .collect();
        if filtered.len() >= 2 {
            tokens = filtered;
        }
    }
    let slug = join_slug_tokens(&tokens, 5, 36)?;
    if !slug.contains('-') && STOP_SINGLE.contains(&slug.as_str()) {
        return None;
    }
    Some(slug)
}

/// Deterministic semantic slug extraction from an initial prompt or session title
pub fn derive_semantic_slug(text: &str) -> Option<String> {
    let base = sanitize_branch_slug(text)?;
    let has_action = base.starts_with("fix-") || base.starts_with("feat-") || base.starts_with("refactor-");
    if has_action {
        return Some(base);
    }
    let action = if ["修复", "排查", "解决", "崩溃", "白屏", "报错", "异常"]
        .iter()
        .any(|k| text.contains(k))
    {
        Some("fix")
    } else if ["新增", "添加", "支持", "实现", "增加"].iter().any(|k| text.contains(k)) {
        Some("feat")
    } else if ["重构", "优化", "改进"].iter().any(|k| text.contains(k)) {
        Some("refactor")
    } else {
        None
    };
    if let Some(act) = action {
        if base != act {
            let parts: Vec<String> = std::iter::once(act.to_string())
                .chain(base.split('-').map(str::to_string))
                .collect();
            if let Some(combined) = join_slug_tokens(&parts, 5, 36) {
                return Some(combined);
            }
        }
    }
    Some(base)
}

/// True when the prompt/title is primarily non-CJK and yielded an informative slug without needing an LLM call
pub fn is_strong_ascii_slug(source: &str, slug: &str) -> bool {
    let cjk_count = source.chars().filter(|&c| is_cjk_char(c)).count();
    if cjk_count >= 2 {
        return false;
    }
    slug.contains('-') || slug.len() >= 6
}

/// Extract a clean lowercase branch slug from CLI text generation output (stripping fences and accidental prefixes)
pub fn parse_generated_branch_slug(raw: &str) -> Option<String> {
    let candidate = raw
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("```"))
        .last()?;
    let cleaned = candidate
        .trim_matches(|c: char| "`'\"()[]{}.,:;!?".contains(c))
        .trim();
    let tail = cleaned.rsplit('/').next().unwrap_or(cleaned).trim();
    sanitize_branch_slug(tail)
}

/// Read the checked-out short branch name for `cwd`, or None when detached / not a git repo
pub fn current_branch(cwd: &Path) -> Option<String> {
    let root = repo_root(cwd)?;
    git_ok(&root, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// True when the checked-out branch in `cwd` tracks a remote upstream branch
pub fn has_upstream(cwd: &Path) -> bool {
    let Some(root) = repo_root(cwd) else {
        return false;
    };
    git(&root, &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"])
        .is_ok_and(|o| o.status.success())
}

fn list_local_branches(repo: &Path) -> Result<Vec<String>> {
    let out = git_ok(repo, &["for-each-ref", "--format=%(refname:short)", "refs/heads"])?;
    Ok(out
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect())
}

fn resolve_available_branch_name_excluding(
    repo: &Path,
    desired: &str,
    exclude_exact: Option<&str>,
) -> Result<String> {
    let all = list_local_branches(repo)?;
    let existing: HashSet<String> = all
        .into_iter()
        .filter(|b| exclude_exact != Some(b.as_str()))
        .map(|b| b.to_ascii_lowercase())
        .collect();
    let mut blocked = existing.clone();
    for br in &existing {
        let segs: Vec<&str> = br.split('/').collect();
        for idx in 1..segs.len() {
            blocked.insert(segs[..idx].join("/"));
        }
    }
    let segs: Vec<&str> = desired.split('/').collect();
    let blocked_by_parent = (1..segs.len()).any(|idx| existing.contains(&segs[..idx].join("/").to_ascii_lowercase()));
    let base = if blocked_by_parent { segs.join("-") } else { desired.to_string() };
    if !blocked.contains(&base.to_ascii_lowercase()) {
        return Ok(base);
    }
    for n in 2..=100 {
        let cand = format!("{base}-{n}");
        if !blocked.contains(&cand.to_ascii_lowercase()) {
            return Ok(cand);
        }
    }
    bail!("无法为分支找到可用名称: {desired}")
}

/// Pick `desired` if unused (and free of slash-ref hierarchy conflicts), else append `-2`..`-100`
pub fn resolve_available_branch_name(repo: &Path, desired: &str) -> Result<String> {
    resolve_available_branch_name_excluding(repo, desired, None)
}

/// Rename a local branch via `git branch -m` (optionally resolving collisions for background auto-renames)
pub fn rename_branch(
    cwd: &str,
    old_branch: Option<&str>,
    new_branch: &str,
    resolve_collision: bool,
) -> Result<Value> {
    let dir = crate::paths::expand(cwd);
    let root = repo_root(&dir).ok_or_else(|| anyhow!("不是 git 仓库"))?;
    let new_branch = new_branch.trim();
    if new_branch.is_empty()
        || new_branch.starts_with('-')
        || git_ok(&root, &["check-ref-format", "--branch", new_branch]).is_err()
    {
        bail!("分支名无效: {new_branch}");
    }
    let old = match old_branch.map(str::trim).filter(|s| !s.is_empty()) {
        Some(o) => {
            if o.starts_with('-') {
                bail!("分支名无效: {o}");
            }
            if !git(&root, &["show-ref", "--verify", "-q", &format!("refs/heads/{o}")])
                .is_ok_and(|out| out.status.success())
            {
                bail!("找不到分支: {o}");
            }
            o.to_string()
        }
        None => current_branch(&root)
            .ok_or_else(|| anyhow!("当前处于分离头指针状态，无法重命名分支"))?,
    };
    if old == new_branch {
        return Ok(json!({ "oldBranch": old, "branch": new_branch, "changed": false }));
    }

    let _guard = GIT_BRANCH_RENAME_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
    let target = if resolve_collision {
        resolve_available_branch_name_excluding(&root, new_branch, Some(&old))?
    } else {
        let branches = list_local_branches(&root)?;
        let others: Vec<&str> = branches
            .iter()
            .map(String::as_str)
            .filter(|b| *b != old)
            .collect();
        let new_low = new_branch.to_ascii_lowercase();
        let same_ci_self = old.eq_ignore_ascii_case(new_branch);
        if others
            .iter()
            .any(|b| *b == new_branch || (!same_ci_self && b.eq_ignore_ascii_case(new_branch)))
        {
            bail!("目标分支已存在: {new_branch}");
        }
        let new_slash = format!("{new_low}/");
        if others.iter().any(|b| {
            let b_low = b.to_ascii_lowercase();
            b_low.starts_with(&new_slash) || new_low.starts_with(&format!("{b_low}/"))
        }) {
            bail!("分支名与已有分支层级冲突: {new_branch}");
        }
        new_branch.to_string()
    };

    if let Err(err) = git_ok(&root, &["branch", "-m", "--", &old, &target]) {
        // Git can move the ref before failing on a locked `.git/config`; confirm actual ref state
        let old_exists = git(&root, &["show-ref", "--verify", "-q", &format!("refs/heads/{old}")])
            .is_ok_and(|o| o.status.success());
        let new_exists = git(&root, &["show-ref", "--verify", "-q", &format!("refs/heads/{target}")])
            .is_ok_and(|o| o.status.success());
        if old_exists || !new_exists {
            return Err(err);
        }
    }
    Ok(json!({ "oldBranch": old, "branch": target, "changed": true }))
}

/// Add a worktree for the repo at `cwd`: `<root>/<repo name>-<hash>/<name>` on a new branch (default `yi/<name>`)
/// from `base` (default HEAD). The hash keeps repos with the same name apart
pub fn worktree_add(root: &Path, cwd: &str, name: &str, branch: Option<&str>, base: Option<&str>) -> Result<Value> {
    worktree_add_with(root, cwd, name, branch, base, false, false)
}

/// Add a worktree either on a new branch (`existing_branch = false`) or checking out an existing local/remote branch (`existing_branch = true`),
/// optionally copying `.env*` and linking ignored dependencies (`prepare_env = true`).
pub fn worktree_add_with(
    root: &Path,
    cwd: &str,
    name: &str,
    branch: Option<&str>,
    base: Option<&str>,
    existing_branch: bool,
    prepare_env: bool,
) -> Result<Value> {
    if !safe_name(name) {
        bail!("worktree 名称只能包含字母、数字、点、下划线和短横线: {name}");
    }
    let dir_input = crate::paths::expand(cwd);
    let repo = main_repo_root(&dir_input)
        .or_else(|| repo_root(&dir_input))
        .ok_or_else(|| anyhow!("不是 git 仓库: {cwd}"))?;
    let repo_name = repo.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "repo".into());
    let repo_name: String = repo_name.chars().map(|c| if c.is_alphanumeric() || "._-".contains(c) { c } else { '_' }).collect();
    let hash = format!("{:x}", md5::compute(repo.to_string_lossy().as_bytes()));
    let dir = root.join(format!("{repo_name}-{}", &hash[..8])).join(name);
    if dir.exists() {
        bail!("worktree 目录已存在: {}", dir.display());
    }
    std::fs::create_dir_all(dir.parent().unwrap())?;
    let path = dir.to_string_lossy().to_string();

    let created_branch = if existing_branch {
        let raw_branch = branch
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("请指定分支名"))?;
        if !is_safe_ref_token(raw_branch) {
            bail!("分支名无效: {raw_branch}");
        }
        if git(&repo, &["show-ref", "--verify", "--quiet", &format!("refs/heads/{raw_branch}")])
            .is_ok_and(|o| o.status.success())
        {
            git_ok(&repo, &["worktree", "add", &path, raw_branch])?;
            raw_branch.to_string()
        } else if git(&repo, &["show-ref", "--verify", "--quiet", &format!("refs/remotes/{raw_branch}")])
            .is_ok_and(|o| o.status.success())
        {
            let local = raw_branch
                .split_once('/')
                .map(|(_, rest)| rest)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| anyhow!("远程分支名无效: {raw_branch}"))?;
            if git_ok(&repo, &["check-ref-format", "--branch", local]).is_err() {
                bail!("分支名无效: {local}");
            }
            if git(&repo, &["show-ref", "--verify", "--quiet", &format!("refs/heads/{local}")])
                .is_ok_and(|o| o.status.success())
            {
                git_ok(&repo, &["worktree", "add", &path, local])?;
            } else {
                git_ok(&repo, &["worktree", "add", "--track", "-b", local, &path, raw_branch])?;
            }
            local.to_string()
        } else {
            bail!("找不到分支: {raw_branch}");
        }
    } else {
        let branch_name = branch.map(str::to_string).unwrap_or_else(|| format!("yi/{name}"));
        let base_ref = base.unwrap_or("HEAD");
        if branch_name.starts_with('-')
            || base_ref.starts_with('-')
            || git_ok(&repo, &["check-ref-format", "--branch", &branch_name]).is_err()
        {
            bail!("分支名无效: {branch_name}");
        }
        git_ok(&repo, &["worktree", "add", "-b", &branch_name, &path, base_ref])?;
        branch_name
    };

    if prepare_env {
        prepare_worktree_env(&repo, &dir);
    }
    Ok(json!({ "path": path, "branch": created_branch, "repo": repo.to_string_lossy() }))
}

/// Unlink symlinked/junctioned dependency directories inside a worktree before removal so git never trips on them
fn unlink_worktree_deps(wt_dir: &Path) {
    for rel in ["node_modules", "app/node_modules", ".venv", "venv"] {
        let p = wt_dir.join(rel);
        let Ok(meta) = std::fs::symlink_metadata(&p) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            let _ = std::fs::remove_dir(&p).or_else(|_| std::fs::remove_file(&p));
        } else if cfg!(windows) && meta.is_dir() {
            // On Windows, `cmd /C rmdir` or `fs::remove_dir` removes a directory junction without touching its target
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
                if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                    let _ = std::fs::remove_dir(&p);
                }
            }
        }
    }
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
    unlink_worktree_deps(&dir);
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

/// Remove any linked (non-main) worktree belonging to `cwd`'s repository, including sibling worktrees and prunable entries
pub fn worktree_remove_in_repo(cwd: &str, path: &str, force: bool, delete_branch: bool) -> Result<()> {
    let dir_input = crate::paths::expand(cwd);
    let root = repo_root(&dir_input).ok_or_else(|| anyhow!("不是 git 仓库"))?;
    let listed = worktrees_with(cwd, false)?;
    let default_branch = listed["defaultBranch"].as_str().map(str::to_string);
    let req_canon_key = Path::new(path)
        .canonicalize()
        .ok()
        .map(|p| norm_path_key(&p.to_string_lossy()));
    let req_raw_key = norm_path_key(path);

    let entry = listed["worktrees"]
        .as_array()
        .and_then(|arr| {
            arr.iter().find(|w| {
                let Some(wp) = w["path"].as_str() else { return false };
                let wp_raw = norm_path_key(wp);
                if wp_raw == req_raw_key {
                    return true;
                }
                if let Some(ref ck) = req_canon_key {
                    let wp_canon = Path::new(wp)
                        .canonicalize()
                        .ok()
                        .map(|p| norm_path_key(&p.to_string_lossy()));
                    if wp_canon.as_deref() == Some(ck.as_str()) {
                        return true;
                    }
                }
                false
            })
        })
        .ok_or_else(|| anyhow!("未找到对应的 worktree: {path}"))?
        .clone();

    if entry["isMain"].as_bool().unwrap_or(false) {
        bail!("不能删除主仓库目录");
    }
    if entry["bare"].as_bool().unwrap_or(false) {
        bail!("不能删除 bare 仓库");
    }
    let wt_path = entry["path"]
        .as_str()
        .ok_or_else(|| anyhow!("worktree 路径无效"))?
        .to_string();
    let branch = entry["branch"].as_str().map(str::to_string);
    let prunable = entry["prunable"].as_bool().unwrap_or(false);

    let common = git_ok(&root, &["rev-parse", "--path-format=absolute", "--git-common-dir"])?;
    let common = std::path::PathBuf::from(common.trim());

    if prunable || !Path::new(&wt_path).exists() {
        git_ok(&common, &["worktree", "prune"])?;
    } else {
        unlink_worktree_deps(Path::new(&wt_path));
        let mut args = vec!["worktree", "remove"];
        if force {
            args.push("--force");
        }
        args.push(&wt_path);
        git_ok(&common, &args)?;
    }

    if delete_branch {
        if let Some(b) = branch.filter(|b| !b.is_empty() && default_branch.as_deref() != Some(b.as_str())) {
            if git(&common, &["show-ref", "--verify", "--quiet", &format!("refs/heads/{b}")])
                .is_ok_and(|o| o.status.success())
            {
                let flag = if force { "-D" } else { "-d" };
                git_ok(&common, &["branch", flag, &b])?;
            }
        }
    }
    Ok(())
}

/// Safely remove clean, merged linked worktrees in the repository (skipping current, dirty, or unmerged ones)
pub fn worktree_prune(cwd: &str, paths: Option<&[String]>, delete_branches: bool) -> Result<Value> {
    let listed = worktrees_with(cwd, true)?;
    let filter_keys: Option<std::collections::HashSet<String>> = paths.map(|ps| {
        ps.iter()
            .map(|p| {
                Path::new(p)
                    .canonicalize()
                    .ok()
                    .map(|c| norm_path_key(&c.to_string_lossy()))
                    .unwrap_or_else(|| norm_path_key(p))
            })
            .collect()
    });

    let mut removed: Vec<String> = Vec::new();
    let mut skipped: Vec<Value> = Vec::new();

    if let Some(arr) = listed["worktrees"].as_array() {
        for w in arr {
            if w["isMain"].as_bool().unwrap_or(false) || w["bare"].as_bool().unwrap_or(false) {
                continue;
            }
            let Some(wt_path) = w["path"].as_str() else { continue };
            if let Some(ref keys) = filter_keys {
                let canon_k = Path::new(wt_path)
                    .canonicalize()
                    .ok()
                    .map(|c| norm_path_key(&c.to_string_lossy()));
                let raw_k = norm_path_key(wt_path);
                if !keys.contains(&raw_k) && !canon_k.as_ref().is_some_and(|ck| keys.contains(ck)) {
                    continue;
                }
            }
            let is_current = w["isCurrent"].as_bool().unwrap_or(false);
            let prunable = w["prunable"].as_bool().unwrap_or(false);
            let dirty_count = w["dirtyCount"].as_u64().unwrap_or(0);
            let merged = w["merged"].as_bool().unwrap_or(false);

            if is_current {
                skipped.push(json!({ "path": wt_path, "reason": "当前正在浏览此 Worktree" }));
                continue;
            }
            if !prunable && dirty_count > 0 {
                skipped.push(json!({ "path": wt_path, "reason": "存在未提交改动" }));
                continue;
            }
            if !prunable && !merged {
                skipped.push(json!({ "path": wt_path, "reason": "尚未合并到默认分支" }));
                continue;
            }
            match worktree_remove_in_repo(cwd, wt_path, false, delete_branches) {
                Ok(()) => removed.push(wt_path.to_string()),
                Err(e) => skipped.push(json!({ "path": wt_path, "reason": e.to_string() })),
            }
        }
    }

    Ok(json!({ "removed": removed, "skipped": skipped }))
}

/// Best-effort synchronous removal with brief retries for Windows file-handle release
pub fn worktree_remove_best_effort(root: &Path, path: &str, force: bool, delete_branch: bool) -> bool {
    if worktree_remove(root, path, force, delete_branch).is_ok() {
        return true;
    }
    for ms in [150u64, 300, 600] {
        std::thread::sleep(std::time::Duration::from_millis(ms));
        if worktree_remove(root, path, force, delete_branch).is_ok() {
            return true;
        }
    }
    false
}

fn is_git_ignored(repo: &Path, rel: &str) -> bool {
    git(repo, &["check-ignore", "-q", "--", rel])
        .is_ok_and(|o| o.status.success())
}

fn link_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(src, dst)
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let dst_s = dst.to_string_lossy().to_string();
        let src_s = src.to_string_lossy().to_string();
        let st = Command::new("cmd")
            .args(["/C", "mklink", "/J", &dst_s, &src_s])
            .creation_flags(0x0800_0000)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        if st.success() {
            Ok(())
        } else {
            Err(std::io::Error::new(std::io::ErrorKind::Other, "mklink failed"))
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (src, dst);
        Ok(())
    }
}

/// Trailing-slash rules like `node_modules/` ignore directories in the main repo but do not match
/// Unix symlinks; add `/<rel>` to shared `.git/info/exclude` when a linked dependency is unignored
fn ensure_worktree_link_ignored(wt_dir: &Path, rel: &str) {
    if is_git_ignored(wt_dir, rel) {
        return;
    }
    let Ok(common) = git_ok(wt_dir, &["rev-parse", "--path-format=absolute", "--git-common-dir"]) else {
        return;
    };
    let exclude = Path::new(common.trim()).join("info").join("exclude");
    if let Some(parent) = exclude.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let entry = format!("/{rel}");
    let existing = std::fs::read_to_string(&exclude).unwrap_or_default();
    if existing.lines().any(|l| l.trim() == entry) {
        return;
    }
    let mut updated = existing;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(&entry);
    updated.push('\n');
    let _ = std::fs::write(&exclude, updated);
}

/// Validate that `rel` stays strictly inside `wt_dir` without traversing or overwriting symlinks.
fn safe_worktree_dest(wt_dir: &Path, wt_canon: &Path, rel: &str) -> Option<std::path::PathBuf> {
    let rel = rel.trim_matches('/');
    if rel.is_empty() || Path::new(rel).is_absolute() {
        return None;
    }
    let parts: Vec<&str> = rel.split('/').collect();
    let mut cur = wt_dir.to_path_buf();
    for seg in &parts[..parts.len().saturating_sub(1)] {
        if seg.is_empty() || *seg == "." || *seg == ".." {
            return None;
        }
        cur.push(seg);
        if let Ok(meta) = std::fs::symlink_metadata(&cur) {
            if meta.file_type().is_symlink() || !meta.is_dir() {
                return None;
            }
        }
    }
    let leaf = parts.last()?;
    if leaf.is_empty() || *leaf == "." || *leaf == ".." {
        return None;
    }
    let dst = wt_dir.join(rel);
    // Reject existing files and dangling symlinks at the destination path.
    if std::fs::symlink_metadata(&dst).is_ok() {
        return None;
    }
    if let Some(parent) = dst.parent() {
        if !parent.exists() {
            std::fs::create_dir_all(parent).ok()?;
        }
        let parent_canon = parent.canonicalize().ok()?;
        if !parent_canon.starts_with(wt_canon) {
            return None;
        }
    }
    Some(dst)
}

/// Prepare a newly created worktree so tests and builds work immediately:
/// copy gitignored `.env*` files (root + 1st-level subdirs, only when ignored on both branches) and symlink/junction heavy ignored dependency folders
fn prepare_worktree_env(main_repo: &Path, wt_dir: &Path) {
    const ENV_MAX_BYTES: u64 = 1024 * 1024;
    let Ok(wt_canon) = wt_dir.canonicalize() else {
        return;
    };
    let copy_env_in = |sub: Option<&str>| {
        if let Some(s) = sub {
            let wt_sub = wt_dir.join(s);
            if !std::fs::symlink_metadata(&wt_sub).is_ok_and(|m| !m.file_type().is_symlink() && m.is_dir()) {
                return;
            }
        }
        let src_dir = sub.map(|s| main_repo.join(s)).unwrap_or_else(|| main_repo.to_path_buf());
        let Ok(entries) = std::fs::read_dir(&src_dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_s = name.to_string_lossy();
            if !name_s.starts_with(".env") {
                continue;
            }
            if !entry.file_type().is_ok_and(|ft| ft.is_file()) {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            if meta.len() > ENV_MAX_BYTES {
                continue;
            }
            let rel = match sub {
                Some(s) => format!("{s}/{name_s}"),
                None => name_s.to_string(),
            };
            if !is_git_ignored(main_repo, &rel) || !is_git_ignored(wt_dir, &rel) {
                continue;
            }
            let Some(dst) = safe_worktree_dest(wt_dir, &wt_canon, &rel) else {
                continue;
            };
            let _ = std::fs::copy(entry.path(), &dst);
        }
    };
    copy_env_in(None);
    if let Ok(entries) = std::fs::read_dir(main_repo) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_s = name.to_string_lossy();
            if matches!(name_s.as_ref(), ".git" | "node_modules" | "target" | ".venv" | "venv") {
                continue;
            }
            if entry.file_type().is_ok_and(|ft| ft.is_dir()) {
                copy_env_in(Some(&name_s));
            }
        }
    }
    for rel in ["node_modules", "app/node_modules", ".venv", "venv"] {
        let src = main_repo.join(rel);
        if !src.is_dir() || !is_git_ignored(main_repo, rel) {
            continue;
        }
        if let Some((parent, _)) = rel.split_once('/') {
            let wt_parent = wt_dir.join(parent);
            if !std::fs::symlink_metadata(&wt_parent).is_ok_and(|m| !m.file_type().is_symlink() && m.is_dir()) {
                continue;
            }
        }
        let Some(dst) = safe_worktree_dest(wt_dir, &wt_canon, rel) else {
            continue;
        };
        if link_dir(&src, &dst).is_ok() {
            ensure_worktree_link_ignored(wt_dir, rel);
        }
    }
}

/// Create a managed Git worktree for a user session (`worktree_add` + `.env` / dependency linking)
pub fn worktree_add_session(
    root: &Path,
    cwd: &str,
    folder_name: &str,
    branch: Option<&str>,
    auto_slug: Option<&str>,
    base: Option<&str>,
    default_prefix: &str,
    temporary_branch: bool,
) -> Result<(String, yi_proto::SessionWorktree)> {
    let dir = crate::paths::expand(cwd);
    let main_repo = main_repo_root(&dir).ok_or_else(|| anyhow!("不是 git 仓库: {cwd}"))?;
    let base_arg = base.map(str::trim).filter(|s| !s.is_empty());
    if base_arg.is_some_and(|b| b.starts_with('-')) {
        bail!("基准分支无效");
    }
    let base_branch = match base_arg {
        Some(b) => {
            if git(&main_repo, &["show-ref", "--verify", "-q", &format!("refs/heads/{b}")])
                .is_ok_and(|o| o.status.success())
            {
                Some(b.to_string())
            } else {
                None
            }
        }
        None => git_ok(&dir, &["symbolic-ref", "--quiet", "--short", "HEAD"])
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| {
                git_ok(&main_repo, &["symbolic-ref", "--quiet", "--short", "HEAD"])
                    .ok()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
            }),
    };
    let resolve_ref = base_arg.unwrap_or("HEAD");
    let base_commit = git_ok(&dir, &["rev-parse", "--verify", "-q", &format!("{resolve_ref}^{{commit}}")])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let base_for_add = base_arg.or(base_commit.as_deref());
    let main_cwd = native_worktree_path(&main_repo.to_string_lossy());
    let (chosen_branch, is_temporary) = match branch.map(str::trim).filter(|s| !s.is_empty()) {
        Some(explicit) => (explicit.to_string(), false),
        None => {
            let raw_slug = auto_slug
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(folder_name);
            let desired = format_worktree_branch(default_prefix, raw_slug);
            (resolve_available_branch_name(&main_repo, &desired)?, temporary_branch)
        }
    };
    let added = worktree_add(root, &main_cwd, folder_name, Some(&chosen_branch), base_for_add)?;
    let wt_path = added["path"]
        .as_str()
        .ok_or_else(|| anyhow!("worktree 创建结果缺少路径"))?
        .to_string();
    let wt_branch = added["branch"]
        .as_str()
        .ok_or_else(|| anyhow!("worktree 创建结果缺少分支"))?
        .to_string();
    prepare_worktree_env(&main_repo, Path::new(&wt_path));
    Ok((
        wt_path,
        yi_proto::SessionWorktree {
            repo_root: main_cwd,
            branch: wt_branch,
            base_branch,
            base_commit,
            temporary_branch: is_temporary,
            attached: false,
        },
    ))
}

/// Detect whether `cwd` is an existing linked git worktree distinct from its primary repository checkout
pub fn detect_existing_worktree(cwd: &str) -> Option<yi_proto::SessionWorktree> {
    let dir = crate::paths::expand(cwd);
    let wt_root = repo_root(&dir)?;
    let main_repo = main_repo_root(&dir)?;
    let wt_canon = wt_root.canonicalize().unwrap_or_else(|_| wt_root.clone());
    let main_canon = main_repo.canonicalize().unwrap_or_else(|_| main_repo.clone());
    if norm_path_key(&wt_canon.to_string_lossy()) == norm_path_key(&main_canon.to_string_lossy()) {
        return None;
    }
    let branch = git_ok(&wt_root, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            git_ok(&wt_root, &["rev-parse", "--short", "HEAD"])
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "HEAD".to_string());
    let base_branch = git_ok(&main_repo, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| detect_default_branch(&main_repo));
    let base_commit = base_branch
        .as_deref()
        .and_then(|b| git_ok(&wt_root, &["merge-base", b, "HEAD"]).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    Some(yi_proto::SessionWorktree {
        repo_root: native_worktree_path(&main_repo.to_string_lossy()),
        branch,
        base_branch,
        base_commit,
        temporary_branch: false,
        attached: true,
    })
}

/// True only when a session-bound worktree has no uncommitted changes and 0 commits ahead of its base branch/commit
pub fn worktree_is_clean_and_merged(wt_cwd: &str, wt: &yi_proto::SessionWorktree) -> bool {
    let dir = crate::paths::expand(wt_cwd);
    let Some(wt_root) = repo_root(&dir) else {
        return false;
    };
    let Ok(st) = git_ok(&wt_root, &["status", "--porcelain"]) else {
        return false;
    };
    if !st.trim().is_empty() {
        return false;
    }
    let Some(base_ref) = wt.base_branch.as_deref().or(wt.base_commit.as_deref()) else {
        return false;
    };
    git_ok(&wt_root, &["rev-list", "--count", &format!("{base_ref}..HEAD")])
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .is_some_and(|ahead| ahead == 0)
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
    worktrees_with(cwd, false)
}

/// List all worktrees of the repo, optionally enriching each with dirty count, ahead/behind, and merged status
pub fn worktrees_with(cwd: &str, details: bool) -> Result<Value> {
    let dir = crate::paths::expand(cwd);
    let Some(root) = repo_root(&dir) else {
        return Ok(json!({ "repo": null, "current": null, "defaultBranch": null, "worktrees": [] }));
    };
    let current_canon = root.canonicalize().unwrap_or_else(|_| root.clone());
    let current_key = norm_path_key(&current_canon.to_string_lossy());
    let default_branch = detect_default_branch(&root);
    let out = git_ok(&root, &["worktree", "list", "--porcelain"])?;

    #[derive(Clone)]
    struct RawWt {
        path: String,
        head: String,
        branch: Option<String>,
        detached: bool,
        is_main: bool,
        is_current: bool,
        bare: bool,
        prunable: bool,
    }

    let mut raw_items: Vec<RawWt> = Vec::new();
    let mut path: Option<String> = None;
    let mut head = String::new();
    let mut branch: Option<String> = None;
    let mut detached = false;
    let mut bare = false;
    let mut prunable = false;

    let flush = |raw_items: &mut Vec<RawWt>,
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
            let is_main = raw_items.is_empty();
            raw_items.push(RawWt {
                path: native,
                head: head.clone(),
                branch: branch.take(),
                detached: *detached,
                is_main,
                is_current,
                bare: *bare,
                prunable: *prunable,
            });
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
            flush(&mut raw_items, &mut path, &mut head, &mut branch, &mut detached, &mut bare, &mut prunable);
            continue;
        }
        if let Some(v) = line.strip_prefix("worktree ") {
            flush(&mut raw_items, &mut path, &mut head, &mut branch, &mut detached, &mut bare, &mut prunable);
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
    flush(&mut raw_items, &mut path, &mut head, &mut branch, &mut detached, &mut bare, &mut prunable);

    let mut branch_track: std::collections::HashMap<String, (Option<String>, Option<u64>, Option<u64>)> =
        std::collections::HashMap::new();
    let mut merged_set: std::collections::HashSet<String> = std::collections::HashSet::new();

    if details {
        let fmt = format!("--format=%(refname:short){FS}%(upstream:short){FS}%(upstream:track){RS}");
        if let Ok(br_out) = git_ok(&root, &["for-each-ref", &fmt, "refs/heads"]) {
            for rec in br_out.split(RS).map(|r| r.trim_start_matches('\n')).filter(|r| !r.is_empty()) {
                let f: Vec<&str> = rec.splitn(3, FS).collect();
                if let [name, upstream, track] = f[..] {
                    let (ahead, behind) = parse_track(track);
                    branch_track.insert(
                        name.to_string(),
                        (if upstream.is_empty() { None } else { Some(upstream.to_string()) }, ahead, behind),
                    );
                }
            }
        }
        if let Some(ref def) = default_branch {
            if let Ok(m_out) = git_ok(&root, &["for-each-ref", &format!("--merged={def}"), "--format=%(refname:short)", "refs/heads"]) {
                for line in m_out.lines().map(str::trim).filter(|s| !s.is_empty()) {
                    merged_set.insert(line.to_string());
                }
            }
        }
    }

    let mut detail_checked = 0usize;
    let items: Vec<Value> = raw_items
        .iter()
        .map(|w| {
            let mut obj = json!({
                "path": w.path,
                "head": w.head,
                "branch": w.branch,
                "detached": w.detached,
                "isMain": w.is_main,
                "isCurrent": w.is_current,
                "bare": w.bare,
                "prunable": w.prunable,
            });
            if details && !w.bare && detail_checked < 60 {
                detail_checked += 1;
                let (upstream, ahead, behind) = w
                    .branch
                    .as_deref()
                    .and_then(|b| branch_track.get(b))
                    .cloned()
                    .unwrap_or((None, None, None));

                let dirty_count = if !w.prunable && Path::new(&w.path).is_dir() {
                    git_ok(Path::new(&w.path), &["status", "--porcelain"])
                        .map(|s| s.lines().filter(|l| !l.trim().is_empty()).count() as u64)
                        .unwrap_or(0)
                } else {
                    0
                };

                let is_default_branch = w.branch.as_deref().is_some_and(|b| {
                    default_branch.as_deref() == Some(b)
                        || default_branch
                            .as_deref()
                            .and_then(|d| d.strip_prefix("origin/"))
                            == Some(b)
                });
                let merged = if w.is_main || w.prunable || is_default_branch {
                    false
                } else if let Some(ref br) = w.branch {
                    merged_set.contains(br)
                } else if !w.head.is_empty() {
                    default_branch.as_deref().is_some_and(|def| {
                        git(&root, &["merge-base", "--is-ancestor", &w.head, def])
                            .is_ok_and(|o| o.status.success())
                    })
                } else {
                    false
                };

                let ahead_base = if !w.is_main && !w.prunable && !is_default_branch {
                    if merged {
                        Some(0u64)
                    } else if let Some(ref def) = default_branch {
                        let rev = w.branch.as_deref().or(if !w.head.is_empty() { Some(w.head.as_str()) } else { None });
                        rev.and_then(|r| {
                            git_ok(&root, &["rev-list", "--count", &format!("{def}..{r}")])
                                .ok()
                                .and_then(|s| s.trim().parse::<u64>().ok())
                        })
                    } else {
                        None
                    }
                } else {
                    None
                };

                if let Some(map) = obj.as_object_mut() {
                    map.insert("dirtyCount".into(), json!(dirty_count));
                    map.insert("upstream".into(), json!(upstream));
                    map.insert("ahead".into(), json!(ahead));
                    map.insert("behind".into(), json!(behind));
                    map.insert("aheadBase".into(), json!(ahead_base));
                    map.insert("merged".into(), json!(merged));
                }
            }
            obj
        })
        .collect();

    let main_path = items
        .first()
        .and_then(|v| v["path"].as_str())
        .map(str::to_string)
        .unwrap_or_else(|| native_worktree_path(&root.to_string_lossy()));

    Ok(json!({
        "repo": main_path,
        "current": native_worktree_path(&root.to_string_lossy()),
        "defaultBranch": default_branch,
        "worktrees": items,
    }))
}

/// Detect the repository's default branch name (origin/HEAD -> local main/master -> origin/main|master -> current branch)
fn detect_default_branch(root: &Path) -> Option<String> {
    let mut candidates: Vec<String> = Vec::with_capacity(5);
    let sym = git_ok(root, &["symbolic-ref", "--quiet", "--short", "refs/remotes/origin/HEAD"])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    if let Some(ref sym) = sym {
        let short = sym.strip_prefix("origin/").unwrap_or(sym);
        if !short.is_empty() {
            candidates.push(format!("refs/heads/{short}"));
        }
        candidates.push(format!("refs/remotes/{sym}"));
    }
    for cand in ["refs/heads/main", "refs/heads/master", "refs/remotes/origin/main", "refs/remotes/origin/master"] {
        if !candidates.iter().any(|c| c == cand) {
            candidates.push(cand.to_string());
        }
    }
    let mut args = vec!["for-each-ref", "--format=%(refname)"];
    args.extend(candidates.iter().map(String::as_str));
    if let Ok(out) = git_ok(root, &args) {
        let present: std::collections::HashSet<&str> = out.lines().map(str::trim).filter(|s| !s.is_empty()).collect();
        for full_ref in &candidates {
            if present.contains(full_ref.as_str()) {
                return Some(
                    full_ref
                        .strip_prefix("refs/heads/")
                        .or_else(|| full_ref.strip_prefix("refs/remotes/"))
                        .unwrap_or(full_ref)
                        .to_string(),
                );
            }
        }
    }
    git_ok(root, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Parse ahead/behind counts from a `git status --porcelain=v1 -b -z` leading `## ...` record.
/// Returns `(None, None)` when there is no upstream or upstream is `[gone]`, and `(Some(ahead), Some(behind))` when tracked.
fn parse_status_branch_track(header: &str) -> (Option<u64>, Option<u64>) {
    let Some(rest) = header.strip_prefix("## ") else {
        return (None, None);
    };
    let Some((_, after_dots)) = rest.split_once("...") else {
        return (None, None);
    };
    if let Some((_, bracket)) = after_dots.split_once(" [") {
        let trimmed = bracket.trim_end_matches(']');
        if trimmed.split(',').any(|p| p.trim() == "gone") {
            return (None, None);
        }
        let (ahead, behind) = parse_track(trimmed);
        return (Some(ahead.unwrap_or(0)), Some(behind.unwrap_or(0)));
    }
    (Some(0), Some(0))
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

/// List local branches, remote branches, the current branch, and the detected default branch
pub fn branches(cwd: &str) -> Result<Value> {
    let dir = crate::paths::expand(cwd);
    let Some(root) = repo_root(&dir) else {
        return Ok(json!({ "repo": null, "current": null, "defaultBranch": null, "branches": [], "remoteBranches": [] }));
    };
    let current = git_ok(&root, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let default_branch = detect_default_branch(&root);
    let mut merged_set: std::collections::HashSet<String> = std::collections::HashSet::new();
    if let Some(ref def) = default_branch {
        if let Ok(m_out) = git_ok(&root, &["for-each-ref", &format!("--merged={def}"), "--format=%(refname:short)", "refs/heads"]) {
            for line in m_out.lines().map(str::trim).filter(|s| !s.is_empty()) {
                merged_set.insert(line.to_string());
            }
        }
    }
    let fmt = format!("--format=%(HEAD){FS}%(refname:short){FS}%(upstream:short){FS}%(upstream:track){RS}");
    let out = git_ok(&root, &["for-each-ref", "--sort=-committerdate", &fmt, "refs/heads"])?;
    let mut list = Vec::new();
    for rec in out.split(RS).map(|r| r.trim_start_matches('\n')).filter(|r| !r.is_empty()) {
        let f: Vec<&str> = rec.splitn(4, FS).collect();
        let [head_mark, name, upstream, track] = f[..] else {
            continue;
        };
        let (ahead, behind) = parse_track(track);
        let merged = default_branch.as_deref() != Some(name) && merged_set.contains(name);
        list.push(json!({
            "name": name,
            "current": head_mark == "*",
            "upstream": if upstream.is_empty() { Value::Null } else { json!(upstream) },
            "ahead": ahead,
            "behind": behind,
            "merged": merged,
        }));
    }
    let remote_branches: Vec<String> = git_ok(
        &root,
        &["for-each-ref", "--sort=-committerdate", "--format=%(refname:short)", "refs/remotes"],
    )
    .unwrap_or_default()
    .lines()
    .map(str::trim)
    .filter(|s| !s.is_empty() && s.contains('/') && !s.ends_with("/HEAD"))
    .map(str::to_string)
    .collect();
    Ok(json!({
        "repo": root.display().to_string(),
        "current": current,
        "defaultBranch": default_branch,
        "branches": list,
        "remoteBranches": remote_branches,
    }))
}

/// Switch to an existing branch (or remote tracking branch) or create and switch to a new branch
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
        Ok(json!({ "branch": branch }))
    } else if git(&root, &["show-ref", "--verify", "--quiet", &format!("refs/heads/{branch}")])
        .is_ok_and(|o| o.status.success())
    {
        git_ok(&root, &["checkout", branch])?;
        Ok(json!({ "branch": branch }))
    } else if git(&root, &["show-ref", "--verify", "--quiet", &format!("refs/remotes/{branch}")])
        .is_ok_and(|o| o.status.success())
    {
        let local = branch
            .split_once('/')
            .map(|(_, rest)| rest)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("远程分支名无效: {branch}"))?;
        if git_ok(&root, &["check-ref-format", "--branch", local]).is_err() {
            bail!("分支名无效: {local}");
        }
        if git(&root, &["show-ref", "--verify", "--quiet", &format!("refs/heads/{local}")])
            .is_ok_and(|o| o.status.success())
        {
            git_ok(&root, &["checkout", local])?;
        } else {
            git_ok(&root, &["checkout", "--track", "-b", local, branch])?;
        }
        Ok(json!({ "branch": local }))
    } else {
        git_ok(&root, &["checkout", branch])?;
        Ok(json!({ "branch": branch }))
    }
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

fn combine_git_output(out: &std::process::Output) -> String {
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    [stdout, stderr].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n")
}

/// Fetch updates from remote(s)
pub fn fetch(cwd: &str, remote: Option<&str>, prune: bool) -> Result<Value> {
    let dir = crate::paths::expand(cwd);
    let root = repo_root(&dir).ok_or_else(|| anyhow!("不是 git 仓库"))?;
    let explicit_remote = remote.map(str::trim).filter(|s| !s.is_empty());
    if let Some(r) = explicit_remote {
        if r.starts_with('-') || !is_safe_ref_token(r) {
            bail!("远程仓库名无效: {r}");
        }
    }
    let mut args = vec!["fetch"];
    if prune {
        args.push("--prune");
    }
    if let Some(r) = explicit_remote {
        args.push(r);
    } else {
        args.push("--all");
    }
    let out = git(&root, &args)?;
    let combined = combine_git_output(&out);
    if !out.status.success() {
        bail!("{}", if combined.is_empty() { "拉取远程更新失败".into() } else { combined });
    }
    Ok(json!({ "output": combined }))
}

/// Pull remote updates into the current branch
pub fn pull(cwd: &str, rebase: bool) -> Result<Value> {
    let dir = crate::paths::expand(cwd);
    let root = repo_root(&dir).ok_or_else(|| anyhow!("不是 git 仓库"))?;
    let current_branch = git_ok(&root, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("当前处于分离头指针状态，请先切换到分支再拉取"))?;
    let mut args = vec!["pull"];
    if rebase {
        args.push("--rebase");
    }
    let out = git(&root, &args)?;
    let combined = combine_git_output(&out);
    if !out.status.success() {
        if rebase {
            let _ = git(&root, &["rebase", "--abort"]);
        }
        bail!("{}", if combined.is_empty() { "拉取更新失败".into() } else { combined });
    }
    Ok(json!({ "branch": current_branch, "output": combined }))
}

/// Merge `source` into `target` (or current HEAD branch when `target` is omitted).
/// Automatically resolves the worktree directory holding `target` if it is checked out elsewhere, and aborts cleanly on conflicts.
pub fn merge(cwd: &str, source: &str, target: Option<&str>, squash: bool, message: Option<&str>) -> Result<Value> {
    let source = source.trim();
    if source.is_empty() || !is_safe_ref_token(source) {
        bail!("分支名无效: {source}");
    }
    let dir = crate::paths::expand(cwd);
    let root = repo_root(&dir).ok_or_else(|| anyhow!("不是 git 仓库"))?;
    if !git(&root, &["rev-parse", "--verify", "-q", &format!("{source}^{{commit}}")])
        .is_ok_and(|o| o.status.success())
    {
        bail!("找不到分支或提交: {source}");
    }

    let (target_dir, target_branch) = match target.map(str::trim).filter(|s| !s.is_empty()) {
        None => {
            let cur = git_ok(&root, &["symbolic-ref", "--quiet", "--short", "HEAD"])
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| anyhow!("当前处于分离头指针状态，请先切换到分支再合并"))?;
            (root.clone(), cur)
        }
        Some(t) => {
            if !is_safe_ref_token(t) {
                bail!("目标分支无效: {t}");
            }
            if !git(&root, &["show-ref", "--verify", "--quiet", &format!("refs/heads/{t}")])
                .is_ok_and(|o| o.status.success())
            {
                bail!("找不到目标分支: {t}");
            }
            let listed = worktrees_with(cwd, false)?;
            let matched_wt = listed["worktrees"]
                .as_array()
                .and_then(|arr| {
                    arr.iter().find(|w| {
                        !w["bare"].as_bool().unwrap_or(false)
                            && !w["prunable"].as_bool().unwrap_or(false)
                            && w["branch"].as_str() == Some(t)
                    })
                })
                .and_then(|w| w["path"].as_str())
                .map(std::path::PathBuf::from);
            if let Some(wt_dir) = matched_wt {
                (wt_dir, t.to_string())
            } else {
                let main_dir = main_repo_root(&dir).unwrap_or_else(|| root.clone());
                let st = git_ok(&main_dir, &["status", "--porcelain"])?;
                if !st.trim().is_empty() {
                    bail!("目标分支未检出且主仓库有未提交改动，请先提交或清理");
                }
                git_ok(&main_dir, &["checkout", t])?;
                (main_dir, t.to_string())
            }
        }
    };

    if source == target_branch {
        bail!("源分支与目标分支相同");
    }
    let st = git_ok(&target_dir, &["status", "--porcelain"])?;
    if !st.trim().is_empty() {
        bail!("目标分支目录有未提交改动，请先提交或清理后再合并");
    }

    if squash {
        let out = git(&target_dir, &["merge", "--squash", source])?;
        if !out.status.success() {
            let _ = git(&target_dir, &["reset", "--merge"]);
            bail!("合并存在冲突，已自动中止，请在终端手动解决冲突");
        }
        let staged = git(&target_dir, &["diff", "--cached", "--quiet"])?;
        if staged.status.success() {
            return Ok(json!({
                "source": source,
                "target": target_branch,
                "output": "已经是最新，无需合并",
                "alreadyUpToDate": true,
            }));
        }
        let msg = message
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("squash: merge {source} into {target_branch}"));
        let commit_out = git(&target_dir, &["commit", "-m", &msg])?;
        let combined = combine_git_output(&commit_out);
        if !commit_out.status.success() {
            let _ = git(&target_dir, &["reset", "--merge"]);
            bail!("{}", if combined.is_empty() { "提交合并结果失败".into() } else { combined });
        }
        Ok(json!({
            "source": source,
            "target": target_branch,
            "output": combined,
        }))
    } else {
        let out = git(&target_dir, &["merge", "--no-edit", source])?;
        let combined = combine_git_output(&out);
        if !out.status.success() {
            let _ = git(&target_dir, &["merge", "--abort"]);
            bail!("合并存在冲突，已自动中止，请在终端手动解决冲突");
        }
        let lower = combined.to_ascii_lowercase();
        let already_up_to_date =
            lower.contains("already up to date") || lower.contains("already up-to-date");
        Ok(json!({
            "source": source,
            "target": target_branch,
            "output": combined,
            "alreadyUpToDate": already_up_to_date,
        }))
    }
}

/// Rebase the current branch onto `onto`, aborting cleanly if conflicts occur
pub fn rebase(cwd: &str, onto: &str) -> Result<Value> {
    let onto = onto.trim();
    if onto.is_empty() || !is_safe_ref_token(onto) {
        bail!("基准分支无效: {onto}");
    }
    let dir = crate::paths::expand(cwd);
    let root = repo_root(&dir).ok_or_else(|| anyhow!("不是 git 仓库"))?;
    let current_branch = git_ok(&root, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("当前处于分离头指针状态，无法变基"))?;
    if !git(&root, &["rev-parse", "--verify", "-q", &format!("{onto}^{{commit}}")])
        .is_ok_and(|o| o.status.success())
    {
        bail!("找不到分支或提交: {onto}");
    }
    let st = git_ok(&root, &["status", "--porcelain"])?;
    if !st.trim().is_empty() {
        bail!("当前工作区有未提交改动，请先提交或放弃后再变基");
    }
    let out = git(&root, &["rebase", onto])?;
    let combined = combine_git_output(&out);
    if !out.status.success() {
        let _ = git(&root, &["rebase", "--abort"]);
        bail!("变基存在冲突，已自动中止，请在终端手动处理");
    }
    Ok(json!({
        "branch": current_branch,
        "onto": onto,
        "output": combined,
    }))
}

/// Delete a local branch (refusing if currently checked out in any worktree)
pub fn branch_delete(cwd: &str, branch: &str, force: bool) -> Result<Value> {
    let branch = branch.trim();
    if branch.is_empty() || branch.starts_with('-') {
        bail!("分支名无效: {branch}");
    }
    let dir = crate::paths::expand(cwd);
    let root = repo_root(&dir).ok_or_else(|| anyhow!("不是 git 仓库"))?;
    if git_ok(&root, &["check-ref-format", "--branch", branch]).is_err() {
        bail!("分支名无效: {branch}");
    }
    let listed = worktrees_with(cwd, false)?;
    if listed["worktrees"]
        .as_array()
        .is_some_and(|arr| arr.iter().any(|w| w["branch"].as_str() == Some(branch)))
    {
        bail!("分支 {branch} 正被 Worktree 占用，请先切换或删除对应 Worktree");
    }
    let flag = if force { "-D" } else { "-d" };
    git_ok(&root, &["branch", flag, branch])?;
    Ok(json!({ "branch": branch }))
}

/// Build a unified diff for an untracked file on disk (link target text for symlinks; empty for outside paths, binaries, or oversized files)
fn untracked_diff(root: &Path, can_root: Option<&Path>, path: &str) -> String {
    let full = root.join(path);
    let Ok(meta) = std::fs::symlink_metadata(&full) else {
        return String::new();
    };
    if meta.file_type().is_symlink() {
        let Ok(target) = std::fs::read_link(&full) else {
            return String::new();
        };
        let target = target.to_string_lossy();
        if target.len() > DIFF_MAX {
            return String::new();
        }
        return format!("--- /dev/null\n+++ b/{path}\n@@ -0,0 +1,1 @@\n+{target}\n");
    }
    if !meta.is_file() || meta.len() as usize > DIFF_MAX {
        return String::new();
    }
    let owned_root;
    let can_root = match can_root {
        Some(r) => r,
        None => {
            let Ok(r) = root.canonicalize() else {
                return String::new();
            };
            owned_root = r;
            &owned_root
        }
    };
    let Ok(can_full) = full.canonicalize() else {
        return String::new();
    };
    if !can_full.starts_with(can_root) {
        return String::new();
    }
    match std::fs::read(&can_full) {
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
    if only.is_some_and(|o| o.is_empty()) {
        return Ok(json!({
            "repo": root.display().to_string(),
            "branch": branch,
            "base": chosen_base,
            "mergeBase": merge_base,
            "total": Value::Null,
            "files": [],
        }));
    }

    let mut num_args = vec!["diff", "--numstat", "-z", "--no-renames", &merge_base];
    let mut name_args = vec!["diff", "--name-status", "-z", "--no-renames", &merge_base];
    let mut ls_args = vec!["ls-files", "--others", "--exclude-standard", "-z"];
    let lit: Vec<String> = only
        .filter(|p| !p.is_empty())
        .map(|paths| paths.iter().map(|p| literal_pathspec(p)).collect())
        .unwrap_or_default();
    if !lit.is_empty() {
        num_args.push("--");
        num_args.extend(lit.iter().map(String::as_str));
        name_args.push("--");
        name_args.extend(lit.iter().map(String::as_str));
        ls_args.push("--");
        ls_args.extend(lit.iter().map(String::as_str));
    }

    let mut stats = std::collections::HashMap::new();
    if let Ok(out) = git_ok(&root, &num_args) {
        for rec in out.split('\0').filter(|s| !s.is_empty()) {
            let mut it = rec.splitn(3, '\t');
            if let (Some(a), Some(d), Some(path)) = (it.next(), it.next(), it.next()) {
                stats.insert(path.to_string(), (a.parse::<u64>().ok(), d.parse::<u64>().ok()));
            }
        }
    }

    let mut entries: Vec<(String, String, bool)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let names = git_ok(&root, &name_args)?;
    let mut parts = names.split('\0').filter(|s| !s.is_empty());
    while let (Some(code), Some(path)) = (parts.next(), parts.next()) {
        if seen.insert(path.to_string()) {
            entries.push((path.to_string(), code.to_string(), false));
        }
    }
    if let Ok(untracked) = git_ok(&root, &ls_args) {
        for path in untracked.split('\0').filter(|s| !s.is_empty()) {
            if seen.insert(path.to_string()) {
                entries.push((path.to_string(), "new".to_string(), true));
            }
        }
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let root_canon = root.canonicalize().ok();
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
            untracked_diff(&root, root_canon.as_deref(), &path)
        } else {
            let lit_path = literal_pathspec(&path);
            let d = git_ok(&root, &["diff", &merge_base, "--no-color", "--", &lit_path]).unwrap_or_default();
            if d.len() > DIFF_MAX { String::new() } else { d }
        };
        let (added, deleted) = if is_untracked {
            counted += 1;
            (if counted <= COUNT_MAX { count_lines(&root, root_canon.as_deref(), &path) } else { None }, Some(0))
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

pub(crate) fn is_safe_ref_token(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 200
        && !s.starts_with('-')
        && !s.contains("..")
        && !s.bytes().any(|b| {
            b < 0x20
                || b == 0x7f
                || b.is_ascii_whitespace()
                || matches!(
                    b,
                    b':' | b';' | b'|' | b'&' | b'$' | b'`' | b'(' | b')' | b'<' | b'>' | b'\\' | b'\'' | b'"' | b'*' | b'?' | b'['
                )
        })
}

fn parse_numstat_with_binary(out: &str) -> std::collections::HashMap<String, (Option<u64>, Option<u64>, bool)> {
    let mut m = std::collections::HashMap::new();
    for rec in out.split('\0').filter(|s| !s.is_empty()) {
        let mut it = rec.splitn(3, '\t');
        let (Some(a), Some(d), Some(path)) = (it.next(), it.next(), it.next()) else {
            continue;
        };
        if a == "-" && d == "-" {
            m.insert(path.to_string(), (None, None, true));
        } else {
            m.insert(path.to_string(), (a.parse::<u64>().ok(), d.parse::<u64>().ok(), false));
        }
    }
    m
}

fn inspect_untracked_file(root: &Path, rel: &str) -> (Option<u64>, Option<u64>, bool) {
    use std::io::Read;
    let full = root.join(rel);
    let Ok(meta) = std::fs::symlink_metadata(&full) else {
        return (None, None, false);
    };
    if meta.file_type().is_symlink() {
        let len = std::fs::read_link(&full)
            .ok()
            .map(|t| t.to_string_lossy().len() as u64);
        return (Some(1), len, false);
    }
    if !meta.is_file() {
        return (None, None, false);
    }
    let (Ok(can_root), Ok(can_full)) = (root.canonicalize(), full.canonicalize()) else {
        return (None, None, false);
    };
    if !can_full.starts_with(&can_root) {
        return (None, None, false);
    }
    let size = meta.len();
    if size as usize > DIFF_MAX {
        let is_bin = std::fs::File::open(&can_full)
            .ok()
            .map(|f| {
                let mut buf = Vec::with_capacity(8000);
                let _ = f.take(8000).read_to_end(&mut buf);
                buf.contains(&0)
            })
            .unwrap_or(false);
        return (None, Some(size), is_bin);
    }
    let Ok(b) = std::fs::read(&can_full) else {
        return (None, Some(size), false);
    };
    if b.iter().take(8000).any(|&c| c == 0) {
        return (None, Some(size), true);
    }
    let n = b.iter().filter(|&&c| c == b'\n').count() + usize::from(b.last().is_some_and(|&c| c != b'\n'));
    (Some(n as u64), Some(size), false)
}

fn empty_tree_oid(root: &Path) -> Result<String> {
    Ok(git_ok(root, &["hash-object", "-t", "tree", "--stdin"])?.trim().to_string())
}

fn diff_two_revs(
    root: &Path,
    base_oid: &str,
    end_oid: &str,
    with_diff: bool,
    only: Option<&[String]>,
) -> Result<(Option<usize>, Vec<Value>)> {
    let stats = git_ok(root, &["diff", "--numstat", "-z", "--no-renames", base_oid, end_oid])
        .map(|out| parse_numstat_with_binary(&out))
        .unwrap_or_default();
    let names = git_ok(root, &["diff", "--name-status", "-z", "--no-renames", base_oid, end_oid])?;
    let mut entries: Vec<(String, String)> = Vec::new();
    let mut parts = names.split('\0').filter(|s| !s.is_empty());
    while let (Some(code), Some(path)) = (parts.next(), parts.next()) {
        entries.push((path.to_string(), code.to_string()));
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
            let lit_path = literal_pathspec(&path);
            let d = git_ok(root, &["diff", "--no-color", base_oid, end_oid, "--", &lit_path]).unwrap_or_default();
            if d.len() > DIFF_MAX { String::new() } else { d }
        };
        let (added, deleted, binary) = stats.get(&path).copied().unwrap_or((None, None, false));
        files.push(json!({
            "path": path,
            "status": code,
            "diff": diff,
            "added": added,
            "deleted": deleted,
            "binary": binary,
            "size": Value::Null,
            "untracked": false,
            "staged": false,
        }));
    }
    let total_out = if only.is_none() && total > files.len() { Some(total) } else { None };
    Ok((total_out, files))
}

fn rev_commit_count(root: &Path, base_oid: &str, end_oid: &str) -> Option<u64> {
    git_ok(root, &["rev-list", "--count", &format!("{base_oid}..{end_oid}")])
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
}

/// Resolve and list changed files/stats for cross-tool code review scopes (`uncommitted`, `branch`, `commits`, `range`)
pub fn review_scope(cwd: &str, params: &Value) -> Result<Value> {
    let only: Option<Vec<String>> = params["paths"]
        .as_array()
        .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect());
    if let Some(o) = only.as_deref() {
        check_paths(o)?;
    }
    let with_diff = params["diff"].as_bool().unwrap_or(false);
    let mode = params["mode"].as_str().unwrap_or("uncommitted");
    let dir = crate::paths::expand(cwd);
    let Some(root) = repo_root(&dir) else {
        return Ok(json!({ "repo": null, "mode": mode, "files": [] }));
    };
    let repo = root.display().to_string();
    let branch = git_ok(&root, &["rev-parse", "--abbrev-ref", "HEAD"])
        .unwrap_or_default()
        .trim()
        .to_string();

    match mode {
        "uncommitted" => {
            let filter = match params["uncommittedFilter"].as_str().unwrap_or("all") {
                "tracked" => "tracked",
                "staged" => "staged",
                _ => "all",
            };
            let status = git_ok(&root, &["status", "--porcelain=v1", "-z", "-uall"])?;
            let mut all_count = 0usize;
            let mut tracked_count = 0usize;
            let mut staged_count = 0usize;
            let mut entries: Vec<(String, Option<String>, String, bool, bool)> = Vec::new();

            let mut it = status.split('\0').filter(|s| !s.is_empty());
            while let Some(entry) = it.next() {
                if entry.len() < 4 {
                    continue;
                }
                let (xy, path) = entry.split_at(3);
                let xy = &xy[..2];
                if xy == "!!" {
                    continue;
                }
                let xb = xy.as_bytes()[0];
                let yb = xy.as_bytes()[1];
                let is_rename = xb == b'R' || yb == b'R';
                let is_copy = xb == b'C' || yb == b'C';
                let old_path = if is_rename || is_copy {
                    let orig = it.next();
                    if is_rename { orig.map(str::to_string) } else { None }
                } else {
                    None
                };
                let is_untracked = xy == "??";
                let is_staged = !is_untracked && xb != b' ';
                all_count += 1;
                if !is_untracked {
                    tracked_count += 1;
                }
                if is_staged {
                    staged_count += 1;
                }
                let keep = match filter {
                    "tracked" => !is_untracked,
                    "staged" => is_staged,
                    _ => true,
                };
                if keep {
                    let st = if is_untracked {
                        "new".to_string()
                    } else if filter == "staged" {
                        xy[..1].trim().to_string()
                    } else {
                        xy.trim().to_string()
                    };
                    entries.push((path.to_string(), old_path, st, is_untracked, is_staged));
                }
            }
            entries.sort_by(|a, b| a.0.cmp(&b.0));

            let has_head = git(&root, &["rev-parse", "--verify", "-q", "HEAD"])?.status.success();
            let stats_out = if filter == "staged" || !has_head {
                git_ok(&root, &["diff", "--cached", "--numstat", "-z", "--no-renames"]).unwrap_or_default()
            } else {
                git_ok(&root, &["diff", "HEAD", "--numstat", "-z", "--no-renames"]).unwrap_or_default()
            };
            let stats = parse_numstat_with_binary(&stats_out);

            let total = entries.len();
            let root_canon = root.canonicalize().ok();
            let mut files = Vec::new();
            let mut counted = 0usize;
            for (path, old_path, st, is_untracked, is_staged) in entries {
                if only.as_deref().is_some_and(|o| {
                    !o.iter().any(|x| x == &path || old_path.as_deref() == Some(x.as_str()))
                }) || files.len() >= STATUS_MAX
                {
                    continue;
                }
                let diff = if !with_diff {
                    String::new()
                } else if is_untracked {
                    untracked_diff(&root, root_canon.as_deref(), &path)
                } else {
                    let mut args = vec!["diff"];
                    if filter == "staged" || !has_head {
                        args.push("--cached");
                    } else {
                        args.push("HEAD");
                    }
                    let lit_path = literal_pathspec(&path);
                    let lit_old = old_path.as_deref().map(literal_pathspec);
                    if let Some(ref old) = lit_old {
                        args.extend(["-M", "--no-color", "--", old.as_str(), lit_path.as_str()]);
                    } else {
                        args.extend(["--no-color", "--", lit_path.as_str()]);
                    }
                    let d = git_ok(&root, &args).unwrap_or_default();
                    if d.len() > DIFF_MAX { String::new() } else { d }
                };
                counted += 1;
                let (added, deleted, size, binary) = if is_untracked {
                    if counted <= COUNT_MAX {
                        let (lines, sz, bin) = inspect_untracked_file(&root, &path);
                        (lines, Some(0), sz, bin)
                    } else {
                        (None, Some(0), None, false)
                    }
                } else {
                    let (a, d, bin) = stats.get(&path).copied().unwrap_or((None, None, false));
                    let sz = if counted <= COUNT_MAX {
                        std::fs::symlink_metadata(root.join(&path))
                            .ok()
                            .filter(|m| !m.file_type().is_symlink() && m.is_file())
                            .map(|m| m.len())
                    } else {
                        None
                    };
                    (a, d, sz, bin)
                };
                let mut item = json!({
                    "path": path,
                    "status": st,
                    "diff": diff,
                    "added": added,
                    "deleted": deleted,
                    "size": size,
                    "binary": binary,
                    "untracked": is_untracked,
                    "staged": is_staged,
                });
                if let Some(old) = old_path {
                    item["oldPath"] = json!(old);
                }
                files.push(item);
            }

            Ok(json!({
                "repo": repo,
                "mode": "uncommitted",
                "uncommittedFilter": filter,
                "branch": branch,
                "counts": {
                    "all": all_count,
                    "tracked": tracked_count,
                    "staged": staged_count,
                },
                "total": if only.is_none() && total > files.len() { Some(total) } else { None },
                "files": files,
            }))
        }
        "branch" => {
            let chosen_base = match params["base"].as_str().map(str::trim).filter(|s| !s.is_empty()) {
                Some(b) => b.to_string(),
                None => match detect_default_branch(&root) {
                    Some(b) => b,
                    None => {
                        return Ok(json!({
                            "repo": repo,
                            "mode": "branch",
                            "branch": branch,
                            "files": [],
                            "error": "未找到可用的基准分支",
                        }));
                    }
                },
            };
            let chosen_target = match params["target"].as_str().map(str::trim).filter(|s| !s.is_empty()) {
                Some(t) => t.to_string(),
                None if !branch.is_empty() && branch != "HEAD" => branch.clone(),
                None => "HEAD".to_string(),
            };
            if !is_safe_ref_token(&chosen_base) {
                return Ok(json!({
                    "repo": repo,
                    "mode": "branch",
                    "branch": branch,
                    "base": chosen_base,
                    "target": chosen_target,
                    "files": [],
                    "error": format!("找不到分支或提交: {chosen_base}"),
                }));
            }
            if !is_safe_ref_token(&chosen_target) {
                return Ok(json!({
                    "repo": repo,
                    "mode": "branch",
                    "branch": branch,
                    "base": chosen_base,
                    "target": chosen_target,
                    "files": [],
                    "error": format!("找不到分支或提交: {chosen_target}"),
                }));
            }
            let Ok(base_oid) = git_ok(&root, &["rev-parse", "--verify", "-q", &format!("{chosen_base}^{{commit}}")]) else {
                return Ok(json!({
                    "repo": repo,
                    "mode": "branch",
                    "branch": branch,
                    "base": chosen_base,
                    "target": chosen_target,
                    "files": [],
                    "error": format!("找不到分支或提交: {chosen_base}"),
                }));
            };
            let base_oid = base_oid.trim().to_string();
            let Ok(target_oid) = git_ok(&root, &["rev-parse", "--verify", "-q", &format!("{chosen_target}^{{commit}}")]) else {
                return Ok(json!({
                    "repo": repo,
                    "mode": "branch",
                    "branch": branch,
                    "base": chosen_base,
                    "target": chosen_target,
                    "files": [],
                    "error": format!("找不到分支或提交: {chosen_target}"),
                }));
            };
            let target_oid = target_oid.trim().to_string();
            let target_is_head = chosen_target == "HEAD" || (!branch.is_empty() && branch != "HEAD" && chosen_target == branch);
            let include_uncommitted = params["includeUncommitted"].as_bool().unwrap_or(false) && target_is_head;
            let Ok(mb_raw) = git_ok(&root, &["merge-base", &base_oid, &target_oid]) else {
                return Ok(json!({
                    "repo": repo,
                    "mode": "branch",
                    "branch": branch,
                    "base": chosen_base,
                    "target": chosen_target,
                    "targetIsHead": target_is_head,
                    "includeUncommitted": include_uncommitted,
                    "files": [],
                    "error": "两个分支没有共同祖先",
                }));
            };
            let merge_base = mb_raw.trim().to_string();
            let commit_count = rev_commit_count(&root, &merge_base, &target_oid);

            if !include_uncommitted {
                let (total, files) = diff_two_revs(&root, &merge_base, &target_oid, with_diff, only.as_deref())?;
                return Ok(json!({
                    "repo": repo,
                    "mode": "branch",
                    "branch": branch,
                    "base": chosen_base,
                    "target": chosen_target,
                    "targetIsHead": target_is_head,
                    "includeUncommitted": false,
                    "mergeBase": merge_base,
                    "baseRev": merge_base,
                    "endRev": target_oid,
                    "commitCount": commit_count,
                    "total": total,
                    "files": files,
                }));
            }

            let stats = git_ok(&root, &["diff", "--numstat", "-z", "--no-renames", &merge_base])
                .map(|out| parse_numstat_with_binary(&out))
                .unwrap_or_default();
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
            let root_canon = root.canonicalize().ok();
            let mut files = Vec::new();
            let mut counted = 0usize;
            for (path, code, is_untracked) in entries {
                if only.as_deref().is_some_and(|o| !o.iter().any(|x| x == &path)) || files.len() >= STATUS_MAX {
                    continue;
                }
                let diff = if !with_diff {
                    String::new()
                } else if is_untracked {
                    untracked_diff(&root, root_canon.as_deref(), &path)
                } else {
                    let lit_path = literal_pathspec(&path);
                    let d = git_ok(&root, &["diff", &merge_base, "--no-color", "--", &lit_path]).unwrap_or_default();
                    if d.len() > DIFF_MAX { String::new() } else { d }
                };
                counted += 1;
                let (added, deleted, size, binary) = if is_untracked {
                    if counted <= COUNT_MAX {
                        let (lines, sz, bin) = inspect_untracked_file(&root, &path);
                        (lines, Some(0), sz, bin)
                    } else {
                        (None, Some(0), None, false)
                    }
                } else {
                    let (a, d, bin) = stats.get(&path).copied().unwrap_or((None, None, false));
                    let sz = if counted <= COUNT_MAX {
                        std::fs::symlink_metadata(root.join(&path))
                            .ok()
                            .filter(|m| !m.file_type().is_symlink() && m.is_file())
                            .map(|m| m.len())
                    } else {
                        None
                    };
                    (a, d, sz, bin)
                };
                files.push(json!({
                    "path": path,
                    "status": code,
                    "diff": diff,
                    "added": added,
                    "deleted": deleted,
                    "size": size,
                    "binary": binary,
                    "untracked": is_untracked,
                    "staged": false,
                }));
            }

            Ok(json!({
                "repo": repo,
                "mode": "branch",
                "branch": branch,
                "base": chosen_base,
                "target": chosen_target,
                "targetIsHead": target_is_head,
                "includeUncommitted": true,
                "mergeBase": merge_base,
                "baseRev": merge_base,
                "endRev": Value::Null,
                "commitCount": commit_count,
                "total": if only.is_none() && total > files.len() { Some(total) } else { None },
                "files": files,
            }))
        }
        "commits" => {
            struct Meta {
                hash: String,
                short: String,
                parents: Vec<String>,
                author: String,
                time: i64,
                subject: String,
            }
            let raw_list: Vec<String> = params["commits"]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str().map(|s| s.trim().to_string())).filter(|s| !s.is_empty()).take(50).collect())
                .unwrap_or_default();
            if raw_list.is_empty() {
                return Ok(json!({
                    "repo": repo,
                    "mode": "commits",
                    "branch": branch,
                    "commits": [],
                    "contiguous": false,
                    "files": [],
                }));
            }
            let fmt = format!("--format=%H{FS}%h{FS}%P{FS}%an{FS}%at{FS}%s");
            let mut metas: Vec<Meta> = Vec::new();
            let mut seen_hashes = std::collections::HashSet::new();
            for raw in raw_list {
                if !is_safe_ref_token(&raw) {
                    return Ok(json!({
                        "repo": repo,
                        "mode": "commits",
                        "branch": branch,
                        "commits": [],
                        "files": [],
                        "error": format!("找不到提交: {raw}"),
                    }));
                }
                let Ok(full) = git_ok(&root, &["rev-parse", "--verify", "-q", &format!("{raw}^{{commit}}")]) else {
                    return Ok(json!({
                        "repo": repo,
                        "mode": "commits",
                        "branch": branch,
                        "commits": [],
                        "files": [],
                        "error": format!("找不到提交: {raw}"),
                    }));
                };
                let full = full.trim().to_string();
                if !seen_hashes.insert(full.clone()) {
                    continue;
                }
                let info = git_ok(&root, &["log", "-1", &fmt, &full, "--"])?;
                let line = info.trim_end_matches(&['\n', '\r'][..]);
                let f: Vec<&str> = line.splitn(6, FS).collect();
                let [hash, short, parents, author, time, subject] = f[..] else {
                    bail!("无法解析提交信息: {full}");
                };
                metas.push(Meta {
                    hash: hash.to_string(),
                    short: short.to_string(),
                    parents: parents.split(' ').filter(|p| !p.is_empty()).map(str::to_string).collect(),
                    author: author.to_string(),
                    time: time.parse::<i64>().unwrap_or(0),
                    subject: subject.to_string(),
                });
            }

            let commits_json: Vec<Value> = metas
                .iter()
                .map(|m| {
                    json!({
                        "hash": m.hash,
                        "short": m.short,
                        "parents": m.parents,
                        "author": m.author,
                        "time": m.time,
                        "refs": [],
                        "subject": m.subject,
                    })
                })
                .collect();

            if metas.len() == 1 {
                let c = &metas[0];
                let base_oid = match c.parents.first() {
                    Some(p) => p.clone(),
                    None => empty_tree_oid(&root)?,
                };
                let (total, files) = diff_two_revs(&root, &base_oid, &c.hash, with_diff, only.as_deref())?;
                return Ok(json!({
                    "repo": repo,
                    "mode": "commits",
                    "branch": branch,
                    "commits": commits_json,
                    "contiguous": true,
                    "baseRev": base_oid,
                    "endRev": c.hash,
                    "total": total,
                    "files": files,
                }));
            }

            let by_hash: std::collections::HashMap<&str, &Meta> = metas.iter().map(|m| (m.hash.as_str(), m)).collect();
            let mut referenced_first_parents = std::collections::HashSet::new();
            for m in &metas {
                if let Some(p) = m.parents.first() {
                    if by_hash.contains_key(p.as_str()) {
                        referenced_first_parents.insert(p.as_str());
                    }
                }
            }
            let tips: Vec<&Meta> = metas.iter().filter(|m| !referenced_first_parents.contains(m.hash.as_str())).collect();
            let mut contiguous_pair: Option<(String, String)> = None;
            if tips.len() == 1 {
                let newest = tips[0];
                let mut visited = std::collections::HashSet::new();
                let mut cur = newest;
                loop {
                    if !visited.insert(cur.hash.as_str()) {
                        break;
                    }
                    let Some(fp) = cur.parents.first().and_then(|p| by_hash.get(p.as_str())).copied() else {
                        break;
                    };
                    cur = fp;
                }
                if visited.len() == metas.len() {
                    let base_oid = match cur.parents.first() {
                        Some(p) => p.clone(),
                        None => empty_tree_oid(&root)?,
                    };
                    contiguous_pair = Some((base_oid, newest.hash.clone()));
                }
            }

            if let Some((base_oid, end_oid)) = contiguous_pair {
                let (total, files) = diff_two_revs(&root, &base_oid, &end_oid, with_diff, only.as_deref())?;
                return Ok(json!({
                    "repo": repo,
                    "mode": "commits",
                    "branch": branch,
                    "commits": commits_json,
                    "contiguous": true,
                    "baseRev": base_oid,
                    "endRev": end_oid,
                    "total": total,
                    "files": files,
                }));
            }

            // Non-contiguous commits: aggregate per-commit changes oldest-to-newest
            let mut ordered: Vec<&Meta> = metas.iter().collect();
            ordered.sort_by(|a, b| {
                if a.time != b.time {
                    return a.time.cmp(&b.time);
                }
                if git(&root, &["merge-base", "--is-ancestor", &a.hash, &b.hash]).is_ok_and(|o| o.status.success()) {
                    std::cmp::Ordering::Less
                } else if git(&root, &["merge-base", "--is-ancestor", &b.hash, &a.hash]).is_ok_and(|o| o.status.success()) {
                    std::cmp::Ordering::Greater
                } else {
                    std::cmp::Ordering::Equal
                }
            });

            #[derive(Default)]
            struct Agg {
                status: String,
                added: Option<u64>,
                deleted: Option<u64>,
                binary: bool,
                diff: String,
            }
            let empty_tree = if ordered.iter().any(|c| c.parents.is_empty()) {
                Some(empty_tree_oid(&root)?)
            } else {
                None
            };
            let mut agg_map: std::collections::BTreeMap<String, Agg> = std::collections::BTreeMap::new();
            for c in ordered {
                let base_oid = c.parents.first().cloned().unwrap_or_else(|| empty_tree.clone().unwrap());
                let (_, c_files) = diff_two_revs(&root, &base_oid, &c.hash, with_diff, only.as_deref())?;
                for f in c_files {
                    let Some(p) = f["path"].as_str().map(str::to_string) else {
                        continue;
                    };
                    let st = f["status"].as_str().unwrap_or("M").to_string();
                    let a = f["added"].as_u64();
                    let d = f["deleted"].as_u64();
                    let bin = f["binary"].as_bool().unwrap_or(false);
                    let df = f["diff"].as_str().unwrap_or("");
                    let entry = agg_map.entry(p).or_default();
                    if entry.status.is_empty() {
                        entry.status = st;
                    } else if !(entry.status == "A" && st == "M") {
                        entry.status = st;
                    }
                    entry.added = match (entry.added, a) {
                        (Some(x), Some(y)) => Some(x.saturating_add(y)),
                        (Some(x), None) | (None, Some(x)) => Some(x),
                        (None, None) => None,
                    };
                    entry.deleted = match (entry.deleted, d) {
                        (Some(x), Some(y)) => Some(x.saturating_add(y)),
                        (Some(x), None) | (None, Some(x)) => Some(x),
                        (None, None) => None,
                    };
                    entry.binary |= bin;
                    if with_diff && !df.is_empty() && entry.diff.len() + df.len() <= DIFF_MAX {
                        if !entry.diff.is_empty() {
                            entry.diff.push('\n');
                        }
                        entry.diff.push_str(df);
                    }
                }
            }
            let total = agg_map.len();
            let mut files = Vec::new();
            for (path, agg) in agg_map.into_iter().take(STATUS_MAX) {
                files.push(json!({
                    "path": path,
                    "status": agg.status,
                    "diff": agg.diff,
                    "added": agg.added,
                    "deleted": agg.deleted,
                    "binary": agg.binary,
                    "size": Value::Null,
                    "untracked": false,
                    "staged": false,
                }));
            }

            Ok(json!({
                "repo": repo,
                "mode": "commits",
                "branch": branch,
                "commits": commits_json,
                "contiguous": false,
                "baseRev": Value::Null,
                "endRev": Value::Null,
                "total": if only.is_none() && total > files.len() { Some(total) } else { None },
                "files": files,
            }))
        }
        "range" => {
            let raw = params["range"].as_str().unwrap_or("").trim();
            if raw.is_empty() {
                return Ok(json!({
                    "repo": repo,
                    "mode": "range",
                    "branch": branch,
                    "rangeExpr": "",
                    "files": [],
                }));
            }
            let err_res = |msg: String| {
                Ok(json!({
                    "repo": repo,
                    "mode": "range",
                    "branch": branch,
                    "rangeExpr": raw,
                    "files": [],
                    "error": msg,
                }))
            };
            if raw.len() > 256 || raw.starts_with('-') {
                return err_res(format!("范围表达式无效: {raw}"));
            }
            let (base_oid, end_oid) = if let Some((left, right)) = raw.split_once("...") {
                let right = if right.is_empty() { "HEAD" } else { right };
                if left.is_empty() || !is_safe_ref_token(left) || !is_safe_ref_token(right) {
                    return err_res(format!("范围表达式无效: {raw}"));
                }
                let Ok(l_oid) = git_ok(&root, &["rev-parse", "--verify", "-q", &format!("{left}^{{commit}}")]) else {
                    return err_res(format!("找不到分支或提交: {left}"));
                };
                let Ok(r_oid) = git_ok(&root, &["rev-parse", "--verify", "-q", &format!("{right}^{{commit}}")]) else {
                    return err_res(format!("找不到分支或提交: {right}"));
                };
                let l_oid = l_oid.trim().to_string();
                let r_oid = r_oid.trim().to_string();
                let Ok(mb) = git_ok(&root, &["merge-base", &l_oid, &r_oid]) else {
                    return err_res("两个分支没有共同祖先".to_string());
                };
                (mb.trim().to_string(), r_oid)
            } else if let Some((left, right)) = raw.split_once("..") {
                let right = if right.is_empty() { "HEAD" } else { right };
                if left.is_empty() || right.contains("..") || !is_safe_ref_token(left) || !is_safe_ref_token(right) {
                    return err_res(format!("范围表达式无效: {raw}"));
                }
                let Ok(l_oid) = git_ok(&root, &["rev-parse", "--verify", "-q", &format!("{left}^{{commit}}")]) else {
                    return err_res(format!("找不到分支或提交: {left}"));
                };
                let Ok(r_oid) = git_ok(&root, &["rev-parse", "--verify", "-q", &format!("{right}^{{commit}}")]) else {
                    return err_res(format!("找不到分支或提交: {right}"));
                };
                (l_oid.trim().to_string(), r_oid.trim().to_string())
            } else {
                if !is_safe_ref_token(raw) {
                    return err_res(format!("范围表达式无效: {raw}"));
                }
                let Ok(end_oid) = git_ok(&root, &["rev-parse", "--verify", "-q", &format!("{raw}^{{commit}}")]) else {
                    return err_res(format!("找不到提交: {raw}"));
                };
                let end_oid = end_oid.trim().to_string();
                let base_oid = match git_ok(&root, &["rev-parse", "--verify", "-q", &format!("{end_oid}^1")]) {
                    Ok(p) => p.trim().to_string(),
                    Err(_) => empty_tree_oid(&root)?,
                };
                (base_oid, end_oid)
            };

            let (total, files) = diff_two_revs(&root, &base_oid, &end_oid, with_diff, only.as_deref())?;
            let commit_count = rev_commit_count(&root, &base_oid, &end_oid);
            Ok(json!({
                "repo": repo,
                "mode": "range",
                "branch": branch,
                "rangeExpr": raw,
                "baseRev": base_oid,
                "endRev": end_oid,
                "commitCount": commit_count,
                "total": total,
                "files": files,
            }))
        }
        _ => bail!("不支持的审阅范围模式: {mode}"),
    }
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

/// Exact `(files_changed, added_lines, deleted_lines)` between two checkpoint refs, optionally restricted to `only` paths
pub fn diff_stat_refs_filtered(cwd: &str, base_ref: &str, end_ref: &str, only: Option<&[String]>) -> Result<(u32, u64, u64)> {
    if let Some(o) = only {
        check_paths(o)?;
    }
    if !is_valid_checkpoint_ref(base_ref) || !is_valid_checkpoint_ref(end_ref) {
        bail!("快照引用无效");
    }
    let dir = crate::paths::expand(cwd);
    let Some(root) = repo_root(&dir) else {
        return Ok((0, 0, 0));
    };
    let mut args = vec!["diff", "--numstat", "-z", "--no-renames", base_ref, end_ref];
    let lit: Vec<String> = only
        .filter(|p| !p.is_empty())
        .map(|paths| paths.iter().map(|p| literal_pathspec(p)).collect())
        .unwrap_or_default();
    if !lit.is_empty() {
        args.push("--");
        args.extend(lit.iter().map(String::as_str));
    }
    let out = git_ok(&root, &args)?;
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

/// Exact `(files_changed, added_lines, deleted_lines)` between two checkpoint refs
pub fn diff_stat_refs(cwd: &str, base_ref: &str, end_ref: &str) -> Result<(u32, u64, u64)> {
    diff_stat_refs_filtered(cwd, base_ref, end_ref, None)
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

    if only.is_some_and(|o| o.is_empty()) {
        return Ok(json!({
            "repo": root.display().to_string(),
            "baseRef": base_ref,
            "endRef": end_ref,
            "total": Value::Null,
            "files": [],
        }));
    }

    let mut num_args = vec!["diff", "--numstat", "-z", "--no-renames", base_ref, end_ref];
    let mut name_args = vec!["diff", "--name-status", "-z", "--no-renames", base_ref, end_ref];
    let lit: Vec<String> = only
        .filter(|p| !p.is_empty())
        .map(|paths| paths.iter().map(|p| literal_pathspec(p)).collect())
        .unwrap_or_default();
    if !lit.is_empty() {
        num_args.push("--");
        num_args.extend(lit.iter().map(String::as_str));
        name_args.push("--");
        name_args.extend(lit.iter().map(String::as_str));
    }

    let mut stats = std::collections::HashMap::new();
    if let Ok(out) = git_ok(&root, &num_args) {
        for rec in out.split('\0').filter(|s| !s.is_empty()) {
            let mut it = rec.splitn(3, '\t');
            if let (Some(a), Some(d), Some(path)) = (it.next(), it.next(), it.next()) {
                stats.insert(path.to_string(), (a.parse::<u64>().ok(), d.parse::<u64>().ok()));
            }
        }
    }

    let mut entries: Vec<(String, String)> = Vec::new();
    let names = git_ok(&root, &name_args)?;
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
            let lit_path = literal_pathspec(&path);
            let d = git_ok(&root, &["diff", base_ref, end_ref, "--no-color", "--", &lit_path]).unwrap_or_default();
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
    fn session_worktree_env_detect_and_clean_check() {
        let d = tempfile::tempdir().unwrap();
        let repo = d.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let run = |dir: &Path, a: &[&str]| assert!(git(dir, a).unwrap().status.success(), "{a:?}");
        run(&repo, &["init", "-q", "-b", "main"]);
        run(&repo, &["config", "user.email", "t@t"]);
        run(&repo, &["config", "user.name", "t"]);
        run(&repo, &["config", "core.autocrlf", "false"]);
        std::fs::write(repo.join(".gitignore"), ".env*\nnode_modules/\n").unwrap();
        std::fs::write(repo.join("src.txt"), "hello\n").unwrap();
        run(&repo, &["add", "."]);
        run(&repo, &["commit", "-qm", "init"]);

        run(&repo, &["checkout", "-qb", "feat/v2"]);
        std::fs::write(repo.join("v2.txt"), "v2\n").unwrap();
        run(&repo, &["add", "."]);
        run(&repo, &["commit", "-qm", "feat v2"]);
        run(&repo, &["checkout", "-q", "main"]);

        // Ignored .env and node_modules should carry over into the session worktree without dirtying git status
        std::fs::write(repo.join(".env.local"), "TOKEN=abc\n").unwrap();
        std::fs::create_dir_all(repo.join("node_modules/pkg")).unwrap();
        std::fs::write(repo.join("node_modules/pkg/index.js"), "module.exports = 1;\n").unwrap();

        let wt_root = d.path().join("wt");
        let main_cwd = repo.to_string_lossy().to_string();
        assert!(detect_existing_worktree(&main_cwd).is_none());

        // Spawn from explicit base branch `feat/v2` (Method 1: always creates isolated `yi/*` branch)
        let (wt_path, wt) = worktree_add_session(&wt_root, &main_cwd, "s1", None, None, Some("feat/v2"), "yi", false).unwrap();
        assert_eq!(wt.branch, "yi/s1");
        assert_eq!(wt.base_branch.as_deref(), Some("feat/v2"));
        assert!(Path::new(&wt_path).join("v2.txt").exists());
        assert!(Path::new(&wt_path).join(".env.local").exists());
        assert!(Path::new(&wt_path).join("node_modules/pkg/index.js").exists());
        assert!(git_ok(Path::new(&wt_path), &["status", "--porcelain"]).unwrap().trim().is_empty());
        assert!(worktree_is_clean_and_merged(&wt_path, &wt));

        let detected = detect_existing_worktree(&wt_path).unwrap();
        assert_eq!(detected.branch, "yi/s1");
        assert_eq!(detected.base_branch.as_deref(), Some("main"));

        // Uncommitted changes must block automatic cleanup
        std::fs::write(Path::new(&wt_path).join("src.txt"), "dirty\n").unwrap();
        assert!(!worktree_is_clean_and_merged(&wt_path, &wt));

        // Ahead commits with a clean working tree must ALSO block automatic cleanup
        run(Path::new(&wt_path), &["add", "src.txt"]);
        run(Path::new(&wt_path), &["commit", "-qm", "wt work"]);
        assert!(!worktree_is_clean_and_merged(&wt_path, &wt));

        // Explicit removal cleans directory + temp branch without touching main repo's node_modules
        assert!(worktree_remove_best_effort(&wt_root, &wt_path, true, true));
        assert!(!Path::new(&wt_path).exists());
        assert!(git_ok(&repo, &["branch", "--list", "yi/s1"]).unwrap().trim().is_empty());
        assert!(repo.join("node_modules/pkg/index.js").exists());
    }

    #[test]
    fn branch_naming_prefix_slug_and_parse_helpers() {
        assert_eq!(sanitize_branch_prefix("yi/"), "yi");
        assert_eq!(sanitize_branch_prefix("/feat/team//"), "feat/team");
        assert_eq!(sanitize_branch_prefix(""), "");
        assert_eq!(sanitize_branch_prefix(" / "), "");
        assert_eq!(sanitize_branch_prefix("my feat@1"), "my-feat-1");

        let def = yi_proto::DaemonSettings::default();
        assert_eq!(effective_worktree_branch_prefix(&def), "yi");
        let empty = yi_proto::DaemonSettings { worktree_branch_prefix: Some(String::new()), ..Default::default() };
        assert_eq!(effective_worktree_branch_prefix(&empty), "");
        let custom = yi_proto::DaemonSettings { worktree_branch_prefix: Some("feat/".into()), ..Default::default() };
        assert_eq!(effective_worktree_branch_prefix(&custom), "feat");

        assert_eq!(format_worktree_branch("yi", "fix-login"), "yi/fix-login");
        assert_eq!(format_worktree_branch("yi", "yi/fix-login"), "yi/fix-login");
        assert_eq!(format_worktree_branch("", "fix-login"), "fix-login");

        let en_prompt = "Fix white screen when deleting AI review side chat";
        let en_slug = derive_semantic_slug(en_prompt).unwrap();
        assert!(en_slug.starts_with("fix-white-screen"), "{en_slug}");
        assert!(is_strong_ascii_slug(en_prompt, &en_slug));

        let zh_prompt = "排查删除 ai 审阅旁路后白屏的问题";
        if let Some(zh_slug) = derive_semantic_slug(zh_prompt) {
            assert!(!is_strong_ascii_slug(zh_prompt, &zh_slug));
        }
        let zh_mixed = "新增 SideChatTab 分支重命名功能";
        let zh_mixed_slug = derive_semantic_slug(zh_mixed).unwrap();
        assert!(zh_mixed_slug.starts_with("feat-sidechattab"), "{zh_mixed_slug}");
        assert!(!is_strong_ascii_slug(zh_mixed, &zh_mixed_slug));

        assert_eq!(
            parse_generated_branch_slug("```text\nfeat/fix-worktree-rename\n```"),
            Some("fix-worktree-rename".to_string())
        );
        assert_eq!(parse_generated_branch_slug("`add-dark-mode`"), Some("add-dark-mode".to_string()));
        assert_eq!(parse_generated_branch_slug("ok"), None);
    }

    #[test]
    fn rename_branch_and_collision_resolution_in_worktree() {
        let d = tempfile::tempdir().unwrap();
        let repo = d.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let run = |dir: &Path, a: &[&str]| assert!(git(dir, a).unwrap().status.success(), "{a:?}");
        run(&repo, &["init", "-q", "-b", "main"]);
        run(&repo, &["config", "user.email", "t@t"]);
        run(&repo, &["config", "user.name", "t"]);
        std::fs::write(repo.join("a.txt"), "1\n").unwrap();
        run(&repo, &["add", "."]);
        run(&repo, &["commit", "-qm", "init"]);

        run(&repo, &["branch", "yi/taken"]);
        run(&repo, &["branch", "yi/nested/child"]);
        assert_eq!(resolve_available_branch_name(&repo, "yi/taken").unwrap(), "yi/taken-2");
        assert_eq!(resolve_available_branch_name(&repo, "yi/nested").unwrap(), "yi/nested-2");

        // Parent ref blocks nested child -> flattens `/` to `-`
        run(&repo, &["branch", "topic"]);
        assert_eq!(resolve_available_branch_name(&repo, "topic/sub").unwrap(), "topic-sub");

        let wt_root = d.path().join("wt");
        let (wt_path, wt) = worktree_add_session(&wt_root, &repo.to_string_lossy(), "s1", None, Some("s-1111"), None, "yi", true).unwrap();
        assert_eq!(wt.branch, "yi/s-1111");
        assert!(wt.temporary_branch);

        // Invalid branch name rejected
        assert!(rename_branch(&wt_path, None, "--bad", false).is_err());
        assert!(rename_branch(&wt_path, None, "bad branch", false).is_err());
        // Manual collision / hierarchy conflicts rejected when resolve_collision = false
        assert!(rename_branch(&wt_path, None, "yi/taken", false).is_err());
        assert!(rename_branch(&wt_path, None, "yi/nested", false).is_err());
        assert!(rename_branch(&wt_path, None, "topic/sub", false).is_err());

        // No-op same name returns changed: false
        let same = rename_branch(&wt_path, None, "yi/s-1111", false).unwrap();
        assert_eq!(same["changed"], false);

        // Collision-resolving auto-rename picks suffixed branch
        let auto = rename_branch(&wt_path, Some("yi/s-1111"), "yi/taken", true).unwrap();
        assert_eq!(auto["oldBranch"], "yi/s-1111");
        assert_eq!(auto["branch"], "yi/taken-2");
        assert_eq!(auto["changed"], true);
        assert_eq!(current_branch(Path::new(&wt_path)).as_deref(), Some("yi/taken-2"));

        // Manual rename moves to custom branch cleanly without changing directory
        let manual = rename_branch(&wt_path, None, "feat/custom-name", false).unwrap();
        assert_eq!(manual["oldBranch"], "yi/taken-2");
        assert_eq!(manual["branch"], "feat/custom-name");
        assert_eq!(current_branch(Path::new(&wt_path)).as_deref(), Some("feat/custom-name"));

        // Detached HEAD refuses implicit rename
        run(Path::new(&wt_path), &["checkout", "--detach", "HEAD"]);
        assert!(rename_branch(&wt_path, None, "feat/any", false).is_err());
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

    #[test]
    fn branches_includes_remote_and_excludes_remote_head() {
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
        std::fs::write(repo.join("a.txt"), "1\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-qm", "init"]);
        let cwd = repo.to_string_lossy().to_string();
        push(&cwd, Some("origin"), Some("main"), true).unwrap();
        run(&["checkout", "-qb", "release/1.0"]);
        push(&cwd, Some("origin"), Some("release/1.0"), true).unwrap();
        run(&["remote", "set-head", "origin", "main"]);

        let br = branches(&cwd).unwrap();
        let remotes: Vec<&str> = br["remoteBranches"].as_array().unwrap().iter().filter_map(|v| v.as_str()).collect();
        assert!(remotes.contains(&"origin/main"), "{remotes:?}");
        assert!(remotes.contains(&"origin/release/1.0"), "{remotes:?}");
        assert!(!remotes.contains(&"origin/HEAD"), "{remotes:?}");
    }

    #[test]
    fn log_with_searches_subject_author_and_hash() {
        let d = tempfile::tempdir().unwrap();
        let repo = d.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let run = |a: &[&str]| assert!(git(&repo, a).unwrap().status.success(), "{a:?}");
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "alice@example.com"]);
        run(&["config", "user.name", "Alice"]);
        std::fs::write(repo.join("a.txt"), "1\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-qm", "alpha init"]);
        let first_hash = git_ok(&repo, &["rev-parse", "HEAD"]).unwrap().trim().to_string();

        run(&["config", "user.name", "Bob"]);
        std::fs::write(repo.join("a.txt"), "2\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-qm", "keepalive pool suffix"]);

        let cwd = repo.to_string_lossy().to_string();
        let by_subj = log_with(&cwd, 0, 20, Some("KEEPALIVE")).unwrap();
        let c1 = by_subj["commits"].as_array().unwrap();
        assert_eq!(c1.len(), 1);
        assert_eq!(c1[0]["subject"], "keepalive pool suffix");

        let by_author = log_with(&cwd, 0, 20, Some("alice")).unwrap();
        let c2 = by_author["commits"].as_array().unwrap();
        assert_eq!(c2.len(), 1);
        assert_eq!(c2[0]["hash"], first_hash);

        let by_hash = log_with(&cwd, 0, 20, Some(&first_hash[..7])).unwrap();
        let c3 = by_hash["commits"].as_array().unwrap();
        assert_eq!(c3.len(), 1);
        assert_eq!(c3[0]["hash"], first_hash);
    }

    #[test]
    fn review_scope_covers_all_modes_and_safety() {
        let d = tempfile::tempdir().unwrap();
        let repo = d.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let run = |a: &[&str]| assert!(git(&repo, a).unwrap().status.success(), "{a:?}");
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        run(&["config", "core.autocrlf", "false"]);

        // c1 (root) on main
        std::fs::write(repo.join("base.txt"), "v1\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-qm", "c1 root"]);
        let c1 = git_ok(&repo, &["rev-parse", "HEAD"]).unwrap().trim().to_string();

        // Branch feat from main with commits c2, c3, c4
        run(&["checkout", "-qb", "feat"]);
        std::fs::write(repo.join("f1.txt"), "line1\nline2\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-qm", "c2 add f1"]);
        let c2 = git_ok(&repo, &["rev-parse", "HEAD"]).unwrap().trim().to_string();

        std::fs::write(repo.join("f2.txt"), "hello\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-qm", "c3 add f2"]);
        let c3 = git_ok(&repo, &["rev-parse", "HEAD"]).unwrap().trim().to_string();

        std::fs::write(repo.join("f1.txt"), "line1\nline2\nline3\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-qm", "c4 edit f1"]);
        let c4 = git_ok(&repo, &["rev-parse", "HEAD"]).unwrap().trim().to_string();

        // Second non-HEAD branch other
        run(&["checkout", "-qb", "other", "main"]);
        std::fs::write(repo.join("other.txt"), "o1\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-qm", "other c"]);
        run(&["checkout", "-q", "feat"]);

        // Working tree uncommitted changes: staged edit, unstaged edit, untracked text & binary
        std::fs::write(repo.join("base.txt"), "v1-staged\n").unwrap();
        run(&["add", "base.txt"]);
        std::fs::write(repo.join("f2.txt"), "hello-dirty\n").unwrap();
        std::fs::write(repo.join("untracked.txt"), "u1\nu2\n").unwrap();
        std::fs::write(repo.join("bin.dat"), [0u8, 1, 2, 3]).unwrap();

        let cwd = repo.to_string_lossy().to_string();

        // 1. Uncommitted: all vs tracked vs staged
        let u_all = review_scope(&cwd, &json!({ "mode": "uncommitted", "uncommittedFilter": "all" })).unwrap();
        assert_eq!(u_all["counts"], json!({ "all": 4, "tracked": 2, "staged": 1 }));
        let af = u_all["files"].as_array().unwrap();
        assert_eq!(af.len(), 4);
        let bin = af.iter().find(|f| f["path"] == "bin.dat").unwrap();
        assert_eq!(bin["binary"], true);
        assert_eq!(bin["untracked"], true);
        assert_eq!(bin["size"].as_u64(), Some(4));

        let u_tracked = review_scope(&cwd, &json!({ "mode": "uncommitted", "uncommittedFilter": "tracked" })).unwrap();
        assert_eq!(u_tracked["files"].as_array().unwrap().len(), 2);

        let u_staged = review_scope(&cwd, &json!({ "mode": "uncommitted", "uncommittedFilter": "staged" })).unwrap();
        let sf = u_staged["files"].as_array().unwrap();
        assert_eq!(sf.len(), 1);
        assert_eq!(sf[0]["path"], "base.txt");
        assert_eq!(sf[0]["staged"], true);

        // 2. Branch compare: pure (exclude uncommitted), includeUncommitted on HEAD, non-HEAD target
        let b_pure = review_scope(&cwd, &json!({ "mode": "branch", "base": "main", "target": "feat", "includeUncommitted": false })).unwrap();
        assert_eq!(b_pure["targetIsHead"], true);
        assert_eq!(b_pure["includeUncommitted"], false);
        assert_eq!(b_pure["baseRev"], c1);
        assert_eq!(b_pure["endRev"], c4);
        assert_eq!(b_pure["commitCount"], 3);
        let bf_pure: Vec<&str> = b_pure["files"].as_array().unwrap().iter().filter_map(|f| f["path"].as_str()).collect();
        assert_eq!(bf_pure, vec!["f1.txt", "f2.txt"]);

        let b_dirty = review_scope(&cwd, &json!({ "mode": "branch", "base": "main", "target": "feat", "includeUncommitted": true })).unwrap();
        assert_eq!(b_dirty["includeUncommitted"], true);
        assert!(b_dirty["endRev"].is_null());
        let bf_dirty: Vec<&str> = b_dirty["files"].as_array().unwrap().iter().filter_map(|f| f["path"].as_str()).collect();
        assert_eq!(bf_dirty, vec!["base.txt", "bin.dat", "f1.txt", "f2.txt", "untracked.txt"]);

        // Non-HEAD target ignores includeUncommitted
        let b_other = review_scope(&cwd, &json!({ "mode": "branch", "base": "main", "target": "other", "includeUncommitted": true })).unwrap();
        assert_eq!(b_other["targetIsHead"], false);
        assert_eq!(b_other["includeUncommitted"], false);
        let bf_other: Vec<&str> = b_other["files"].as_array().unwrap().iter().filter_map(|f| f["path"].as_str()).collect();
        assert_eq!(bf_other, vec!["other.txt"]);

        // 3. Commits: root commit, contiguous chain, non-contiguous, invalid
        let cm_root = review_scope(&cwd, &json!({ "mode": "commits", "commits": [c1] })).unwrap();
        assert_eq!(cm_root["contiguous"], true);
        assert_eq!(cm_root["files"].as_array().unwrap()[0]["path"], "base.txt");

        let cm_contig = review_scope(&cwd, &json!({ "mode": "commits", "commits": [c4, c3] })).unwrap();
        assert_eq!(cm_contig["contiguous"], true);
        assert_eq!(cm_contig["baseRev"], c2);
        assert_eq!(cm_contig["endRev"], c4);
        let cf_contig: Vec<&str> = cm_contig["files"].as_array().unwrap().iter().filter_map(|f| f["path"].as_str()).collect();
        assert_eq!(cf_contig, vec!["f1.txt", "f2.txt"]);

        let cm_gap = review_scope(&cwd, &json!({ "mode": "commits", "commits": [c4, c2] })).unwrap();
        assert_eq!(cm_gap["contiguous"], false);
        assert!(cm_gap["baseRev"].is_null());
        let f1 = cm_gap["files"].as_array().unwrap().iter().find(|f| f["path"] == "f1.txt").unwrap();
        assert_eq!(f1["status"], "A");
        assert_eq!(f1["added"], 3);

        let cm_bad = review_scope(&cwd, &json!({ "mode": "commits", "commits": ["deadbeef9999"] })).unwrap();
        assert!(cm_bad["error"].as_str().is_some());

        // 4. Range: A..B, A...B, single ref, diff=true with paths, injection safety
        let r_dot2 = review_scope(&cwd, &json!({ "mode": "range", "range": format!("{c2}..{c4}"), "diff": true, "paths": ["f1.txt"] })).unwrap();
        assert_eq!(r_dot2["baseRev"], c2);
        assert_eq!(r_dot2["endRev"], c4);
        assert_eq!(r_dot2["commitCount"], 2);
        assert_eq!(r_dot2["files"].as_array().unwrap().len(), 1);
        assert!(r_dot2["files"][0]["diff"].as_str().unwrap().contains("+line3"));

        let r_dot3 = review_scope(&cwd, &json!({ "mode": "range", "range": "main...other" })).unwrap();
        assert_eq!(r_dot3["baseRev"], c1);
        assert_eq!(r_dot3["files"].as_array().unwrap()[0]["path"], "other.txt");

        let r_single = review_scope(&cwd, &json!({ "mode": "range", "range": &c3 })).unwrap();
        assert_eq!(r_single["baseRev"], c2);
        assert_eq!(r_single["endRev"], c3);

        let r_bad1 = review_scope(&cwd, &json!({ "mode": "range", "range": "--output=evil" })).unwrap();
        assert!(r_bad1["error"].as_str().is_some());
        let r_bad2 = review_scope(&cwd, &json!({ "mode": "range", "range": "HEAD; rm -rf" })).unwrap();
        assert!(r_bad2["error"].as_str().is_some());
    }

    #[test]
    fn branch_lifecycle_fetch_pull_merge_rebase_and_worktree_prune() {
        let d = tempfile::tempdir().unwrap();
        let remote_bare = d.path().join("remote.git");
        let repo = d.path().join("repo");
        let wt_root = d.path().join("wt-root");
        std::fs::create_dir_all(&remote_bare).unwrap();
        std::fs::create_dir_all(&repo).unwrap();

        let run = |dir: &Path, a: &[&str]| assert!(git(dir, a).unwrap().status.success(), "{dir:?} {a:?}");
        run(&remote_bare, &["init", "--bare", "-q", "-b", "main"]);

        run(&repo, &["init", "-q", "-b", "main"]);
        run(&repo, &["config", "user.email", "t@t"]);
        run(&repo, &["config", "user.name", "t"]);
        run(&repo, &["config", "core.autocrlf", "false"]);
        std::fs::write(repo.join("base.txt"), "v1\n").unwrap();
        run(&repo, &["add", "."]);
        run(&repo, &["commit", "-qm", "init"]);
        run(&repo, &["remote", "add", "origin", &remote_bare.to_string_lossy()]);
        run(&repo, &["push", "-u", "origin", "main"]);

        // Create a second clone to push an upstream commit onto origin/main
        let clone2 = d.path().join("clone2");
        run(d.path(), &["clone", "-q", &remote_bare.to_string_lossy(), &clone2.to_string_lossy()]);
        run(&clone2, &["config", "user.email", "t2@t"]);
        run(&clone2, &["config", "user.name", "t2"]);
        std::fs::write(clone2.join("remote.txt"), "from remote\n").unwrap();
        run(&clone2, &["add", "."]);
        run(&clone2, &["commit", "-qm", "remote commit"]);
        run(&clone2, &["push", "-q", "origin", "main"]);

        let cwd = repo.to_string_lossy().to_string();

        // 1. fetch & pull
        fetch(&cwd, None, true).unwrap();
        let wt_before_pull = worktrees_with(&cwd, true).unwrap();
        assert_eq!(wt_before_pull["defaultBranch"], "main");
        assert_eq!(wt_before_pull["worktrees"][0]["behind"].as_u64(), Some(1));
        pull(&cwd, false).unwrap();
        assert!(repo.join("remote.txt").exists());

        // 2. worktree_add_with (new branch + existing branch), details, merge back to main, prune
        let added1 = worktree_add_with(&wt_root, &cwd, "feat-a", Some("feat/a"), Some("main"), false, false).unwrap();
        let wt_a = added1["path"].as_str().unwrap().to_string();
        std::fs::write(Path::new(&wt_a).join("a.txt"), "feature a\n").unwrap();
        run(Path::new(&wt_a), &["add", "."]);
        run(Path::new(&wt_a), &["commit", "-qm", "feat a"]);

        let added2 = worktree_add_with(&wt_root, &cwd, "feat-dirty", Some("feat/dirty"), Some("main"), false, false).unwrap();
        let wt_dirty = added2["path"].as_str().unwrap().to_string();
        std::fs::write(Path::new(&wt_dirty).join("dirty.txt"), "wip\n").unwrap();

        let det = worktrees_with(&cwd, true).unwrap();
        let list = det["worktrees"].as_array().unwrap();
        let info_a = list.iter().find(|w| w["branch"] == "feat/a").unwrap();
        assert_eq!(info_a["dirtyCount"], 0);
        assert_eq!(info_a["merged"], false);
        assert_eq!(info_a["aheadBase"], 1);
        let info_dirty = list.iter().find(|w| w["branch"] == "feat/dirty").unwrap();
        assert_eq!(info_dirty["dirtyCount"], 1);

        // Merge feat/a back into main while calling from inside wt_a!
        merge(&wt_a, "feat/a", Some("main"), false, None).unwrap();
        assert!(repo.join("a.txt").exists());
        let det_after = worktrees_with(&cwd, true).unwrap();
        let info_a2 = det_after["worktrees"]
            .as_array()
            .unwrap()
            .iter()
            .find(|w| w["branch"] == "feat/a")
            .unwrap();
        assert_eq!(info_a2["merged"], true);

        // worktree_prune should remove clean merged wt_a and skip dirty wt_dirty
        let pruned = worktree_prune(&cwd, None, true).unwrap();
        assert_eq!(pruned["removed"].as_array().unwrap().len(), 1);
        assert!(!Path::new(&wt_a).exists());
        assert!(Path::new(&wt_dirty).exists());
        assert!(git_ok(&repo, &["branch", "--list", "feat/a"]).unwrap().trim().is_empty());

        // Cannot remove main; can remove dirty worktree with force
        assert!(worktree_remove_in_repo(&cwd, &cwd, true, false).is_err());
        worktree_remove_in_repo(&cwd, &wt_dirty, true, true).unwrap();
        assert!(!Path::new(&wt_dirty).exists());

        // 3. Rebase, squash merge, conflict abort, branch rename & delete
        checkout(&cwd, "feat/squash", true, Some("main")).unwrap();
        std::fs::write(repo.join("sq.txt"), "s1\n").unwrap();
        run(&repo, &["add", "."]);
        run(&repo, &["commit", "-qm", "sq 1"]);
        std::fs::write(repo.join("sq.txt"), "s2\n").unwrap();
        run(&repo, &["add", "."]);
        run(&repo, &["commit", "-qm", "sq 2"]);

        // Advance main while feat/squash is branched off earlier commit
        checkout(&cwd, "main", false, None).unwrap();
        std::fs::write(repo.join("main2.txt"), "m2\n").unwrap();
        run(&repo, &["add", "."]);
        run(&repo, &["commit", "-qm", "main advance"]);

        // Rebase feat/squash onto main
        checkout(&cwd, "feat/squash", false, None).unwrap();
        rebase(&cwd, "main").unwrap();
        assert!(repo.join("main2.txt").exists());

        // Squash merge feat/squash back into main
        merge(&cwd, "feat/squash", Some("main"), true, Some("feat: squashed")).unwrap();
        checkout(&cwd, "main", false, None).unwrap();
        assert!(repo.join("sq.txt").exists());
        assert_eq!(std::fs::read_to_string(repo.join("sq.txt")).unwrap(), "s2\n");

        // Conflict test for merge + squash merge + rebase (must abort cleanly)
        checkout(&cwd, "conflict-b", true, Some("main")).unwrap();
        std::fs::write(repo.join("base.txt"), "branch-b\n").unwrap();
        run(&repo, &["add", "."]);
        run(&repo, &["commit", "-qm", "edit base on b"]);
        checkout(&cwd, "main", false, None).unwrap();
        std::fs::write(repo.join("base.txt"), "main-edit\n").unwrap();
        run(&repo, &["add", "."]);
        run(&repo, &["commit", "-qm", "edit base on main"]);

        assert!(merge(&cwd, "conflict-b", None, false, None).is_err());
        assert!(git_ok(&repo, &["status", "--porcelain"]).unwrap().trim().is_empty());
        assert!(merge(&cwd, "conflict-b", None, true, None).is_err());
        assert!(git_ok(&repo, &["status", "--porcelain"]).unwrap().trim().is_empty());

        checkout(&cwd, "conflict-b", false, None).unwrap();
        assert!(rebase(&cwd, "main").is_err());
        assert!(git_ok(&repo, &["status", "--porcelain"]).unwrap().trim().is_empty());

        // Open existing branch in worktree & verify branch_delete refuses occupied branch
        checkout(&cwd, "main", false, None).unwrap();
        let existing_wt = worktree_add_with(&wt_root, &cwd, "cb-wt", Some("conflict-b"), None, true, false).unwrap();
        let cb_path = existing_wt["path"].as_str().unwrap().to_string();
        assert!(branch_delete(&cwd, "conflict-b", true).is_err());
        worktree_remove_in_repo(&cwd, &cb_path, false, false).unwrap();

        // Rename & delete (-d fails on unmerged, -D succeeds)
        let renamed = rename_branch(&cwd, Some("conflict-b"), "conflict-renamed", false).unwrap();
        assert_eq!(renamed["oldBranch"], "conflict-b");
        assert_eq!(renamed["branch"], "conflict-renamed");
        assert!(branch_delete(&cwd, "conflict-renamed", false).is_err());
        branch_delete(&cwd, "conflict-renamed", true).unwrap();

        // Clean checkout of remote tracking branch
        checkout(&cwd, "origin/main", false, None).unwrap();
    }

    #[test]
    fn worktree_env_skips_unignored_env_on_target_branch_and_symlinks() {
        let d = tempfile::tempdir().unwrap();
        let repo = d.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let run = |dir: &Path, a: &[&str]| assert!(git(dir, a).unwrap().status.success(), "{a:?}");
        run(&repo, &["init", "-q", "-b", "legacy"]);
        run(&repo, &["config", "user.email", "t@t"]);
        run(&repo, &["config", "user.name", "t"]);
        std::fs::write(repo.join("README.md"), "legacy\n").unwrap();
        run(&repo, &["add", "."]);
        run(&repo, &["commit", "-qm", "legacy"]);

        run(&repo, &["checkout", "-qb", "main"]);
        std::fs::write(repo.join(".gitignore"), ".env*\n").unwrap();
        run(&repo, &["add", "."]);
        run(&repo, &["commit", "-qm", "ignore env"]);

        std::fs::write(repo.join(".env.local"), "SECRET=123\n").unwrap();
        let wt_root = d.path().join("wt");
        let main_cwd = repo.to_string_lossy().to_string();
        // Target branch `legacy` does not ignore .env.local, so prepare_worktree_env must skip it.
        let (wt_path, _) = worktree_add_session(&wt_root, &main_cwd, "leg", None, None, Some("legacy"), "yi", false).unwrap();
        assert!(!Path::new(&wt_path).join(".env.local").exists());
        assert!(git_ok(Path::new(&wt_path), &["status", "--porcelain"]).unwrap().trim().is_empty());

        #[cfg(unix)]
        {
            let outside = d.path().join("outside-target");
            std::fs::create_dir_all(&outside).unwrap();
            let sub_wt = wt_root.join("sym-wt");
            std::fs::create_dir_all(&sub_wt).unwrap();
            std::os::unix::fs::symlink(&outside, sub_wt.join("sub")).unwrap();
            std::os::unix::fs::symlink(outside.join("leak.env"), sub_wt.join(".env.local")).unwrap();
            let sub_canon = sub_wt.canonicalize().unwrap();
            assert!(safe_worktree_dest(&sub_wt, &sub_canon, "sub/.env.local").is_none());
            assert!(safe_worktree_dest(&sub_wt, &sub_canon, ".env.local").is_none());
        }
    }

    #[cfg(unix)]
    #[test]
    fn untracked_diff_does_not_follow_symlinks_outside_repo() {
        let d = tempfile::tempdir().unwrap();
        let outside = d.path().join("outside-secret.txt");
        std::fs::write(&outside, "TOP_SECRET_VALUE\n").unwrap();

        let repo = d.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let run = |dir: &Path, a: &[&str]| assert!(git(dir, a).unwrap().status.success(), "{a:?}");
        run(&repo, &["init", "-q", "-b", "main"]);
        run(&repo, &["config", "user.email", "t@t"]);
        run(&repo, &["config", "user.name", "t"]);
        std::fs::write(repo.join("base.txt"), "base\n").unwrap();
        run(&repo, &["add", "."]);
        run(&repo, &["commit", "-qm", "init"]);

        std::os::unix::fs::symlink(&outside, repo.join("leak.txt")).unwrap();
        let cwd = repo.to_string_lossy().to_string();

        let ch = changes_with(&cwd, true, None).unwrap();
        let leak_ch = ch["files"].as_array().unwrap().iter().find(|f| f["path"] == "leak.txt").unwrap();
        let ch_diff = leak_ch["diff"].as_str().unwrap();
        assert!(!ch_diff.contains("TOP_SECRET_VALUE"), "{ch_diff}");
        assert!(ch_diff.contains("outside-secret.txt"), "{ch_diff}");

        let bd = branch_diff(&cwd, Some("main"), true, None).unwrap();
        let leak_bd = bd["files"].as_array().unwrap().iter().find(|f| f["path"] == "leak.txt").unwrap();
        assert!(!leak_bd["diff"].as_str().unwrap().contains("TOP_SECRET_VALUE"));

        let rs = review_scope(&cwd, &json!({"mode": "uncommitted", "diff": true})).unwrap();
        let leak_rs = rs["files"].as_array().unwrap().iter().find(|f| f["path"] == "leak.txt").unwrap();
        assert!(!leak_rs["diff"].as_str().unwrap().contains("TOP_SECRET_VALUE"));
        assert!(leak_rs["diff"].as_str().unwrap().contains("outside-secret.txt"));
    }

    #[test]
    fn staged_rename_preserves_old_path_and_commits_and_discards_cleanly() {
        let d = tempfile::tempdir().unwrap();
        let repo = d.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let run = |dir: &Path, a: &[&str]| assert!(git(dir, a).unwrap().status.success(), "{a:?}");
        run(&repo, &["init", "-q", "-b", "main"]);
        run(&repo, &["config", "user.email", "t@t"]);
        run(&repo, &["config", "user.name", "t"]);
        std::fs::write(repo.join("old.txt"), "line1\nline2\n").unwrap();
        run(&repo, &["add", "."]);
        run(&repo, &["commit", "-qm", "init"]);

        run(&repo, &["mv", "old.txt", "new.txt"]);
        let cwd = repo.to_string_lossy().to_string();

        let ch = changes_with(&cwd, true, None).unwrap();
        let item = ch["files"].as_array().unwrap().iter().find(|f| f["path"] == "new.txt").unwrap();
        assert_eq!(item["oldPath"], "old.txt");
        assert!(item["diff"].as_str().unwrap().contains("rename from old.txt"), "{}", item["diff"]);

        let rs = review_scope(&cwd, &json!({"mode": "uncommitted", "diff": true})).unwrap();
        let ritem = rs["files"].as_array().unwrap().iter().find(|f| f["path"] == "new.txt").unwrap();
        assert_eq!(ritem["oldPath"], "old.txt");

        // Discard with both new and old paths restores working tree cleanly
        discard(&cwd, &["new.txt".into(), "old.txt".into()]).unwrap();
        assert!(repo.join("old.txt").exists() && !repo.join("new.txt").exists());
        assert!(git_ok(&repo, &["status", "--porcelain"]).unwrap().trim().is_empty());

        // Rename again with another unrelated staged change and commit only the rename pair
        std::fs::write(repo.join("other.txt"), "stay-staged\n").unwrap();
        run(&repo, &["add", "other.txt"]);
        run(&repo, &["mv", "old.txt", "new.txt"]);
        commit(&cwd, &["new.txt".into(), "old.txt".into()], "rename old to new").unwrap();
        let remain = git_ok(&repo, &["status", "--porcelain"]).unwrap();
        assert_eq!(remain.trim(), "A  other.txt");
        assert!(!repo.join("old.txt").exists() && repo.join("new.txt").exists());
    }

    #[test]
    fn discard_batch_and_prune_empty_dirs() {
        let d = tempfile::tempdir().unwrap();
        let repo = d.path().join("repo");
        std::fs::create_dir_all(repo.join("keep_dir")).unwrap();
        let run = |a: &[&str]| assert!(git(&repo, a).unwrap().status.success(), "{a:?}");
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        run(&["config", "core.autocrlf", "false"]);
        std::fs::write(repo.join("mod.txt"), "v1\n").unwrap();
        std::fs::write(repo.join("staged_mod.txt"), "s1\n").unwrap();
        std::fs::write(repo.join("staged_del.txt"), "bye\n").unwrap();
        std::fs::write(repo.join("old_ren.txt"), "ren\n").unwrap();
        std::fs::write(repo.join("keep_dir/tracked.txt"), "stay\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-qm", "init"]);

        // Create every kind of uncommitted change
        std::fs::write(repo.join("mod.txt"), "v2\n").unwrap();
        std::fs::write(repo.join("staged_mod.txt"), "s2\n").unwrap();
        run(&["add", "staged_mod.txt"]);
        run(&["rm", "-q", "staged_del.txt"]);
        run(&["mv", "old_ren.txt", "new_ren.txt"]);
        std::fs::write(repo.join("staged_new_edited.txt"), "index\n").unwrap();
        run(&["add", "staged_new_edited.txt"]);
        std::fs::write(repo.join("staged_new_edited.txt"), "worktree\n").unwrap();

        std::fs::create_dir_all(repo.join(".tmp-shots/sub")).unwrap();
        std::fs::write(repo.join(".tmp-shots/sub/a.png"), "a").unwrap();
        std::fs::write(repo.join(".tmp-shots/sub/b.png"), "b").unwrap();

        std::fs::create_dir_all(repo.join("partial")).unwrap();
        std::fs::write(repo.join("partial/keep.txt"), "keep").unwrap();
        std::fs::write(repo.join("partial/drop.txt"), "drop").unwrap();

        std::fs::write(repo.join("keep_dir/untracked.txt"), "u").unwrap();

        let cwd = repo.to_string_lossy().to_string();
        assert!(discard(&cwd, &[".git/config".into()]).is_err());
        assert!(discard(&cwd, &["../escape".into()]).is_err());

        discard(
            &cwd,
            &[
                "mod.txt".into(),
                "staged_mod.txt".into(),
                "staged_del.txt".into(),
                "new_ren.txt".into(),
                "old_ren.txt".into(),
                "staged_new_edited.txt".into(),
                ".tmp-shots/sub/a.png".into(),
                ".tmp-shots/sub/b.png".into(),
                "partial/drop.txt".into(),
                "keep_dir/untracked.txt".into(),
            ],
        )
        .unwrap();

        assert_eq!(std::fs::read_to_string(repo.join("mod.txt")).unwrap(), "v1\n");
        assert_eq!(std::fs::read_to_string(repo.join("staged_mod.txt")).unwrap(), "s1\n");
        assert_eq!(std::fs::read_to_string(repo.join("staged_del.txt")).unwrap(), "bye\n");
        assert_eq!(std::fs::read_to_string(repo.join("old_ren.txt")).unwrap(), "ren\n");
        assert!(!repo.join("new_ren.txt").exists());
        assert!(!repo.join("staged_new_edited.txt").exists());
        assert!(!repo.join(".tmp-shots/sub").exists());
        assert!(!repo.join(".tmp-shots").exists());
        assert!(repo.join("partial/keep.txt").exists() && !repo.join("partial/drop.txt").exists());
        assert!(repo.join("keep_dir/tracked.txt").exists() && !repo.join("keep_dir/untracked.txt").exists());
        assert_eq!(git_ok(&repo, &["status", "--porcelain"]).unwrap().trim(), "?? partial/");
    }

    #[test]
    fn discard_unborn_head() {
        let d = tempfile::tempdir().unwrap();
        let repo = d.path().join("repo");
        std::fs::create_dir_all(repo.join("nested/dir")).unwrap();
        let run = |a: &[&str]| assert!(git(&repo, a).unwrap().status.success(), "{a:?}");
        run(&["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("nested/dir/staged.txt"), "staged\n").unwrap();
        run(&["add", "nested/dir/staged.txt"]);
        std::fs::write(repo.join("nested/dir/untracked.txt"), "untracked\n").unwrap();

        let cwd = repo.to_string_lossy().to_string();
        discard(&cwd, &["nested/dir/staged.txt".into(), "nested/dir/untracked.txt".into()]).unwrap();
        assert!(!repo.join("nested").exists());
        assert!(git_ok(&repo, &["status", "--porcelain"]).unwrap().trim().is_empty());
    }

    #[test]
    fn only_pathspec_filters_changes_branch_ref_and_show() {
        let d = tempfile::tempdir().unwrap();
        let repo = d.path().join("repo");
        std::fs::create_dir_all(repo.join("src")).unwrap();
        let run = |dir: &Path, a: &[&str]| assert!(git(dir, a).unwrap().status.success(), "{a:?}");
        run(&repo, &["init", "-q", "-b", "main"]);
        run(&repo, &["config", "user.email", "t@t"]);
        run(&repo, &["config", "user.name", "t"]);
        std::fs::write(repo.join("src/a.txt"), "a1\n").unwrap();
        std::fs::write(repo.join("src/b.txt"), "b1\n").unwrap();
        run(&repo, &["add", "."]);
        run(&repo, &["commit", "-qm", "init"]);

        run(&repo, &["checkout", "-qb", "feat"]);
        std::fs::write(repo.join("src/a.txt"), "a1\na2\n").unwrap();
        std::fs::write(repo.join("src/b.txt"), "b1\nb2\n").unwrap();
        run(&repo, &["add", "."]);
        run(&repo, &["commit", "-qm", "feat changes"]);
        let hash = git_ok(&repo, &["rev-parse", "HEAD"]).unwrap().trim().to_string();

        std::fs::write(repo.join("src/a.txt"), "a1\na2\na3\n").unwrap();
        std::fs::write(repo.join("src/b.txt"), "b1\nb2\nb3\n").unwrap();
        std::fs::write(repo.join("src/new.txt"), "hello\n").unwrap();

        let cwd = repo.to_string_lossy().to_string();
        assert_eq!(detect_default_branch(&repo).as_deref(), Some("main"));

        let ch_empty = changes_with(&cwd, true, Some(&[])).unwrap();
        assert_eq!(ch_empty["files"].as_array().unwrap().len(), 0);

        let ch_one = changes_with(&cwd, true, Some(&["src/new.txt".into()])).unwrap();
        let ch_files = ch_one["files"].as_array().unwrap();
        assert_eq!(ch_files.len(), 1);
        assert_eq!(ch_files[0]["path"], "src/new.txt");
        assert!(ch_files[0]["diff"].as_str().unwrap().contains("+hello"));

        let bd_one = branch_diff(&cwd, Some("main"), true, Some(&["src/a.txt".into()])).unwrap();
        let bd_files = bd_one["files"].as_array().unwrap();
        assert_eq!(bd_files.len(), 1);
        assert_eq!(bd_files[0]["path"], "src/a.txt");
        assert!(bd_files[0]["diff"].as_str().unwrap().contains("+a3"));

        let bd_empty = branch_diff(&cwd, Some("main"), true, Some(&[])).unwrap();
        assert!(bd_empty["files"].as_array().unwrap().is_empty());

        let sh_one = show(&cwd, &hash, Some(&["src/b.txt".into()])).unwrap();
        let sh_files = sh_one["files"].as_array().unwrap();
        assert_eq!(sh_files.len(), 1);
        assert_eq!(sh_files[0]["path"], "src/b.txt");
        assert!(sh_files[0]["diff"].as_str().unwrap().contains("+b2"));

        let sh_empty = show(&cwd, &hash, Some(&[])).unwrap();
        assert!(sh_empty["files"].as_array().unwrap().is_empty());
    }

    #[test]
    fn leading_dash_and_glob_like_filenames_work_across_git_ops() {
        let d = tempfile::tempdir().unwrap();
        let repo = d.path().join("repo");
        std::fs::create_dir_all(repo.join("dir")).unwrap();
        let run = |a: &[&str]| assert!(git(&repo, a).unwrap().status.success(), "{a:?}");
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        run(&["config", "core.autocrlf", "false"]);
        std::fs::write(repo.join("-draft.txt"), "v1\n").unwrap();
        std::fs::write(repo.join("dir/[id].txt"), "glob-like v1\n").unwrap();
        std::fs::write(repo.join("dir/i.txt"), "sibling v1\n").unwrap();
        let lit_dash = literal_pathspec("-draft.txt");
        let lit_bracket = literal_pathspec("dir/[id].txt");
        run(&["add", "--", &lit_dash, &lit_bracket, "dir/i.txt"]);
        run(&["commit", "-qm", "init special filenames"]);

        run(&["checkout", "-qb", "feat"]);
        std::fs::write(repo.join("-draft.txt"), "v2\n").unwrap();
        std::fs::write(repo.join("dir/[id].txt"), "glob-like v2\n").unwrap();
        run(&["add", "--", &lit_dash, &lit_bracket]);
        run(&["commit", "-qm", "feat update"]);
        let hash = git_ok(&repo, &["rev-parse", "HEAD"]).unwrap().trim().to_string();

        std::fs::write(repo.join("-draft.txt"), "v3\n").unwrap();
        std::fs::write(repo.join("-new.txt"), "new dash\n").unwrap();

        let cwd = repo.to_string_lossy().to_string();
        assert!(check_paths(&["".into()]).is_err());
        assert!(check_paths(&["/etc/passwd".into()]).is_err());
        assert!(check_paths(&["\\abs".into()]).is_err());
        assert!(check_paths(&["../escape".into()]).is_err());
        assert!(check_paths(&["a/../b".into()]).is_err());

        let ch_dash = changes_with(&cwd, true, Some(&["-draft.txt".into()])).unwrap();
        let ch_dash_files = ch_dash["files"].as_array().unwrap();
        assert_eq!(ch_dash_files.len(), 1);
        assert_eq!(ch_dash_files[0]["path"], "-draft.txt");
        assert!(ch_dash_files[0]["diff"].as_str().unwrap().contains("+v3"));

        let ch_new = changes_with(&cwd, true, Some(&["-new.txt".into()])).unwrap();
        assert_eq!(ch_new["files"][0]["path"], "-new.txt");
        assert!(ch_new["files"][0]["diff"].as_str().unwrap().contains("+new dash"));

        let b = blob(&cwd, "-draft.txt", "HEAD").unwrap();
        assert_eq!(b["text"], "v2\n");

        let sh = show(&cwd, &hash, Some(&["-draft.txt".into(), "dir/[id].txt".into()])).unwrap();
        let sh_paths: Vec<&str> = sh["files"].as_array().unwrap().iter().map(|f| f["path"].as_str().unwrap()).collect();
        assert_eq!(sh_paths, vec!["-draft.txt", "dir/[id].txt"]);

        let bd = branch_diff(&cwd, Some("main"), true, Some(&["-draft.txt".into()])).unwrap();
        assert_eq!(bd["files"].as_array().unwrap().len(), 1);
        assert!(bd["files"][0]["diff"].as_str().unwrap().contains("+v3"));

        commit(&cwd, &["-new.txt".into()], "commit dash file").unwrap();
        assert_eq!(blob(&cwd, "-new.txt", "HEAD").unwrap()["text"], "new dash\n");

        discard(&cwd, &["-draft.txt".into()]).unwrap();
        assert_eq!(std::fs::read_to_string(repo.join("-draft.txt")).unwrap(), "v2\n");

        // Bracket-like filenames must remain literal without matching sibling files like `dir/i.txt`
        std::fs::write(repo.join("dir/[id].txt"), "glob-like v3\n").unwrap();
        std::fs::write(repo.join("dir/i.txt"), "sibling modified\n").unwrap();

        let ch_br = changes_with(&cwd, true, Some(&["dir/[id].txt".into()])).unwrap();
        let ch_br_files = ch_br["files"].as_array().unwrap();
        assert_eq!(ch_br_files.len(), 1);
        assert_eq!(ch_br_files[0]["path"], "dir/[id].txt");
        assert!(ch_br_files[0]["diff"].as_str().unwrap().contains("+glob-like v3"));

        commit(&cwd, &["dir/[id].txt".into()], "commit literal bracket").unwrap();
        assert_eq!(blob(&cwd, "dir/[id].txt", "HEAD").unwrap()["text"], "glob-like v3\n");
        assert_eq!(std::fs::read_to_string(repo.join("dir/i.txt")).unwrap(), "sibling modified\n");
        assert_eq!(git_ok(&repo, &["status", "--porcelain"]).unwrap().trim(), "M dir/i.txt");

        std::fs::write(repo.join("dir/[id].txt"), "glob-like v4\n").unwrap();
        discard(&cwd, &["dir/[id].txt".into()]).unwrap();
        assert_eq!(std::fs::read_to_string(repo.join("dir/[id].txt")).unwrap(), "glob-like v3\n");
        assert_eq!(std::fs::read_to_string(repo.join("dir/i.txt")).unwrap(), "sibling modified\n");
    }

    #[test]
    fn hooks_do_not_inherit_git_literal_pathspecs() {
        let d = tempfile::tempdir().unwrap();
        let repo = d.path().join("repo");
        let hooks_dir = d.path().join("hooks");
        let wt_root = d.path().join("wt");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&hooks_dir).unwrap();

        let write_hook = |name: &str, body: &str| {
            let p = hooks_dir.join(name);
            std::fs::write(&p, body).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut perms = std::fs::metadata(&p).unwrap().permissions();
                perms.set_mode(0o755);
                std::fs::set_permissions(&p, perms).unwrap();
            }
        };

        write_hook(
            "pre-commit",
            "#!/bin/sh\nif [ -n \"${GIT_LITERAL_PATHSPECS:-}\" ] || [ -n \"${GIT_ICASE_PATHSPECS:-}\" ]; then\n  echo \"pathspec env leaked: literal=${GIT_LITERAL_PATHSPECS:-} icase=${GIT_ICASE_PATHSPECS:-}\" >&2\n  exit 1\nfi\ngit_dir=\"$(git rev-parse --git-dir)\" || exit 1\nmatched=\"$(git diff --cached --name-only -- '*.ts')\" || exit 1\nprintf '%s\\n' \"$matched\" > \"$git_dir/pre-commit-matched.txt\"\ntest -n \"$matched\"\n",
        );
        write_hook(
            "post-checkout",
            "#!/bin/sh\nif [ -n \"${GIT_LITERAL_PATHSPECS:-}\" ] || [ -n \"${GIT_ICASE_PATHSPECS:-}\" ]; then\n  echo \"pathspec env leaked: literal=${GIT_LITERAL_PATHSPECS:-} icase=${GIT_ICASE_PATHSPECS:-}\" >&2\n  exit 1\nfi\ngit_dir=\"$(git rev-parse --git-dir)\" || exit 1\nprintf '%s:%s' \"${GIT_LITERAL_PATHSPECS:-}\" \"${GIT_ICASE_PATHSPECS:-}\" > \"$git_dir/post-checkout-env.txt\"\ngit ls-files -- '*.ts' > \"$git_dir/post-checkout-matched.txt\"\n",
        );

        let run = |a: &[&str]| assert!(git(&repo, a).unwrap().status.success(), "{a:?}");
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        run(&["config", "core.autocrlf", "false"]);
        std::fs::write(repo.join("README.md"), "init\n").unwrap();
        run(&["add", "README.md"]);
        run(&["commit", "-qm", "init"]);

        let hooks_fwd = hooks_dir.to_string_lossy().replace('\\', "/");
        run(&["config", "core.hooksPath", &hooks_fwd]);

        std::fs::write(repo.join("[id].ts"), "export const id = 1;\n").unwrap();
        std::fs::write(repo.join("i.ts"), "export const i = 2;\n").unwrap();
        let cwd = repo.to_string_lossy().to_string();

        commit(&cwd, &["[id].ts".into()], "commit with pre-commit hook").unwrap();
        let pre_matched = std::fs::read_to_string(repo.join(".git/pre-commit-matched.txt")).unwrap();
        assert_eq!(pre_matched.lines().collect::<Vec<_>>(), vec!["[id].ts"]);
        assert_eq!(git_ok(&repo, &["status", "--porcelain"]).unwrap().trim(), "?? i.ts");

        checkout(&cwd, "feat", true, None).unwrap();
        assert_eq!(std::fs::read_to_string(repo.join(".git/post-checkout-env.txt")).unwrap(), ":");
        let co_matched = std::fs::read_to_string(repo.join(".git/post-checkout-matched.txt")).unwrap();
        assert!(co_matched.lines().any(|l| l == "[id].ts"), "co_matched={co_matched:?}");

        let wt = worktree_add(&wt_root, &cwd, "wt1", None, None).unwrap();
        let wt_path = wt["path"].as_str().unwrap();
        let wt_git_raw = git_ok(Path::new(wt_path), &["rev-parse", "--git-dir"]).unwrap();
        let wt_git_p = Path::new(wt_git_raw.trim());
        let wt_git_dir = if wt_git_p.is_absolute() {
            wt_git_p.to_path_buf()
        } else {
            Path::new(wt_path).join(wt_git_p)
        };
        assert_eq!(std::fs::read_to_string(wt_git_dir.join("post-checkout-env.txt")).unwrap(), ":");
        let wt_matched = std::fs::read_to_string(wt_git_dir.join("post-checkout-matched.txt")).unwrap();
        assert!(wt_matched.lines().any(|l| l == "[id].ts"), "wt_matched={wt_matched:?}");
    }

    #[test]
    fn git_cmd_clears_pathspec_env_vars_and_isolates_case_differing_files() {
        let d = tempfile::tempdir().unwrap();
        let repo = d.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();

        let cmd = git_cmd(&repo, &["commit", "-m", "x", "--", ":(literal)Foo.ts"]);
        let envs: std::collections::HashMap<_, _> = cmd.get_envs().collect();
        for key in [
            "GIT_LITERAL_PATHSPECS",
            "GIT_ICASE_PATHSPECS",
            "GIT_GLOB_PATHSPECS",
            "GIT_NOGLOB_PATHSPECS",
        ] {
            assert_eq!(
                envs.get(std::ffi::OsStr::new(key)),
                Some(&None),
                "{key} should be cleared on git child processes"
            );
        }

        // Case-insensitive filesystems cannot hold both `Foo.ts` and `foo.ts` simultaneously.
        std::fs::write(repo.join("Foo.ts"), "upper v1\n").unwrap();
        if repo.join("foo.ts").exists() {
            return;
        }

        let run = |a: &[&str]| assert!(git(&repo, a).unwrap().status.success(), "{a:?}");
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        run(&["config", "core.autocrlf", "false"]);
        std::fs::write(repo.join("foo.ts"), "lower v1\n").unwrap();
        run(&["add", "--", ":(literal)Foo.ts", ":(literal)foo.ts"]);
        run(&["commit", "-qm", "init"]);

        std::fs::write(repo.join("Foo.ts"), "upper v2\n").unwrap();
        std::fs::write(repo.join("foo.ts"), "lower v2\n").unwrap();

        let cwd = repo.to_string_lossy().to_string();
        commit(&cwd, &["Foo.ts".into()], "commit upper only").unwrap();
        assert_eq!(blob(&cwd, "Foo.ts", "HEAD").unwrap()["text"], "upper v2\n");
        assert_eq!(blob(&cwd, "foo.ts", "HEAD").unwrap()["text"], "lower v1\n");
        assert_eq!(git_ok(&repo, &["status", "--porcelain"]).unwrap().trim(), "M foo.ts");

        std::fs::write(repo.join("Foo.ts"), "upper v3\n").unwrap();
        discard(&cwd, &["Foo.ts".into()]).unwrap();
        assert_eq!(std::fs::read_to_string(repo.join("Foo.ts")).unwrap(), "upper v2\n");
        assert_eq!(std::fs::read_to_string(repo.join("foo.ts")).unwrap(), "lower v2\n");
    }

    #[test]
    fn commit_handles_large_batches_and_validates_fallback_cmdline_limit() {
        assert_eq!(parse_git_version("git version 2.25.1"), Some((2, 25)));
        assert_eq!(parse_git_version("git version 2.26.0"), Some((2, 26)));
        assert_eq!(parse_git_version("git version 2.39.5"), Some((2, 39)));
        assert_eq!(parse_git_version("git version 2.54.0.windows.1"), Some((2, 54)));
        assert_eq!(parse_git_version("git version 2.39.3 (Apple Git-146)"), Some((2, 39)));
        assert_eq!(parse_git_version("not-a-version"), None);

        let d = tempfile::tempdir().unwrap();
        let repo = d.path().join("repo");
        std::fs::create_dir_all(repo.join("batch")).unwrap();
        let run = |a: &[&str]| assert!(git(&repo, a).unwrap().status.success(), "{a:?}");
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        run(&["config", "core.autocrlf", "false"]);
        std::fs::write(repo.join("staged-del.txt"), "del\n").unwrap();
        std::fs::write(repo.join("untouched.txt"), "keep v1\n").unwrap();
        std::fs::write(repo.join("small.txt"), "small v1\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-qm", "init"]);

        std::fs::write(repo.join("untouched.txt"), "keep v2\n").unwrap();
        std::fs::write(repo.join("small.txt"), "small v2\n").unwrap();
        run(&["rm", "-q", "staged-del.txt"]);

        let pad = "a".repeat(80);
        let mut batch_paths: Vec<String> = Vec::with_capacity(360);
        for i in 0..360 {
            let rel = format!("batch/file_{i:04}_{pad}.txt");
            std::fs::write(repo.join(&rel), format!("{i}\n")).unwrap();
            batch_paths.push(rel);
        }

        let lit_batch: Vec<String> = batch_paths.iter().map(|p| literal_pathspec(p)).collect();
        let mut oversized_args = vec!["commit", "-m", "too long", "--"];
        oversized_args.extend(lit_batch.iter().map(String::as_str));
        assert!(estimate_cmdline_utf16(&oversized_args) > WINDOWS_CMDLINE_MAX);

        let cwd = repo.to_string_lossy().to_string();
        let too_long = commit_with_options(&cwd, &batch_paths, "too long", Some(false), Some(WINDOWS_CMDLINE_MAX));
        assert!(too_long.is_err());
        assert_eq!(git_ok(&repo, &["diff", "--cached", "--name-only"]).unwrap().trim(), "staged-del.txt");

        // Asymmetric case: `git add` fits when staged deletions are omitted, while `git commit` exceeds the limit.
        let lit_small = literal_pathspec("small.txt");
        let lit_del = literal_pathspec("staged-del.txt");
        let add_est = estimate_cmdline_utf16(&["add", "-A", "--", &lit_small]);
        let commit_est = estimate_cmdline_utf16(&["commit", "-m", "asymmetric", "--", &lit_small, &lit_del]);
        assert!(add_est < commit_est);
        let asym_err = commit_with_options(
            &cwd,
            &["small.txt".into(), "staged-del.txt".into()],
            "asymmetric",
            Some(false),
            Some(add_est),
        );
        assert!(asym_err.is_err());
        assert_eq!(git_ok(&repo, &["diff", "--cached", "--name-only"]).unwrap().trim(), "staged-del.txt");

        // Primary path on Git >= 2.26 streams all paths (and staged deletions) through stdin.
        let mut all_paths = batch_paths.clone();
        all_paths.push("staged-del.txt".into());
        commit(&cwd, &all_paths, "batch commit via stdin").unwrap();

        let head_tree = git_ok(&repo, &["ls-tree", "-r", "--name-only", "HEAD"]).unwrap();
        let committed: std::collections::HashSet<&str> = head_tree.lines().collect();
        assert_eq!(committed.len(), 362);
        assert!(!committed.contains("staged-del.txt"));
        for p in &batch_paths {
            assert!(committed.contains(p.as_str()), "missing committed path {p}");
        }
        let st_lines: Vec<String> = git_ok(&repo, &["status", "--porcelain"])
            .unwrap()
            .lines()
            .map(|l| l.trim().to_string())
            .collect();
        assert_eq!(st_lines, vec!["M small.txt", "M untouched.txt"]);
    }

    #[test]
    fn changes_reports_upstream_ahead_behind() {
        assert_eq!(parse_status_branch_track("## No commits yet on main"), (None, None));
        assert_eq!(parse_status_branch_track("## HEAD (no branch)"), (None, None));
        assert_eq!(parse_status_branch_track("## main"), (None, None));
        assert_eq!(parse_status_branch_track("## main...origin/main"), (Some(0), Some(0)));
        assert_eq!(parse_status_branch_track("## main...origin/main [ahead 2]"), (Some(2), Some(0)));
        assert_eq!(parse_status_branch_track("## main...origin/main [behind 3]"), (Some(0), Some(3)));
        assert_eq!(parse_status_branch_track("## main...origin/main [ahead 2, behind 3]"), (Some(2), Some(3)));
        assert_eq!(parse_status_branch_track("## main...origin/main [gone]"), (None, None));
        assert_eq!(parse_status_branch_track("## main...origin/main [ahead 1, gone]"), (None, None));

        let tmp = tempfile::tempdir().unwrap();
        let remote = tmp.path().join("remote.git");
        let repo = tmp.path().join("repo");
        let other = tmp.path().join("other");

        let run_in = |dir: &Path, args: &[&str]| {
            let out = std::process::Command::new("git").args(args).current_dir(dir).output().unwrap();
            assert!(out.status.success(), "git {:?} failed: {}", args, String::from_utf8_lossy(&out.stderr));
        };

        std::fs::create_dir_all(&remote).unwrap();
        run_in(&remote, &["init", "--bare", "-q", "-b", "main"]);
        std::fs::create_dir_all(&repo).unwrap();
        run_in(&repo, &["init", "-q", "-b", "main"]);
        run_in(&repo, &["config", "user.email", "t@t"]);
        run_in(&repo, &["config", "user.name", "t"]);
        std::fs::write(repo.join("a.txt"), "v1\n").unwrap();
        run_in(&repo, &["add", "a.txt"]);
        run_in(&repo, &["commit", "-qm", "init"]);

        let cwd = repo.to_string_lossy().to_string();
        let no_upstream = changes_with(&cwd, false, None).unwrap();
        assert!(no_upstream["ahead"].is_null());
        assert!(no_upstream["behind"].is_null());

        run_in(&repo, &["remote", "add", "origin", remote.to_str().unwrap()]);
        run_in(&repo, &["push", "-q", "-u", "origin", "main"]);

        let synced = changes_with(&cwd, false, None).unwrap();
        assert_eq!(synced["ahead"], 0);
        assert_eq!(synced["behind"], 0);

        // Second clone pushes a remote commit while local also creates a divergent commit.
        let clone_out = std::process::Command::new("git")
            .args(["clone", "-q", remote.to_str().unwrap(), other.to_str().unwrap()])
            .output()
            .unwrap();
        assert!(clone_out.status.success());
        run_in(&other, &["config", "user.email", "t@t"]);
        run_in(&other, &["config", "user.name", "t"]);
        std::fs::write(other.join("b.txt"), "remote\n").unwrap();
        run_in(&other, &["add", "b.txt"]);
        run_in(&other, &["commit", "-qm", "remote commit"]);
        run_in(&other, &["push", "-q", "origin", "main"]);

        std::fs::write(repo.join("a.txt"), "local\n").unwrap();
        run_in(&repo, &["add", "a.txt"]);
        run_in(&repo, &["commit", "-qm", "local commit"]);
        run_in(&repo, &["fetch", "-q", "origin"]);

        let diverged = changes_with(&cwd, false, None).unwrap();
        assert_eq!(diverged["ahead"], 1);
        assert_eq!(diverged["behind"], 1);

        run_in(&repo, &["reset", "--hard", "-q", "origin/main~1"]);
        let behind_only = changes_with(&cwd, false, None).unwrap();
        assert_eq!(behind_only["ahead"], 0);
        assert_eq!(behind_only["behind"], 1);
    }

    #[test]
    fn changes_with_single_path_preserves_rename_from_either_side() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git").args(args).current_dir(repo).output().unwrap();
            assert!(out.status.success(), "git {:?} failed: {}", args, String::from_utf8_lossy(&out.stderr));
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(repo.join("old.txt"), "hello\nworld\n").unwrap();
        run(&["add", "old.txt"]);
        run(&["commit", "-qm", "init"]);
        run(&["mv", "old.txt", "new.txt"]);

        let cwd = repo.to_string_lossy().to_string();
        for side in ["new.txt", "old.txt"] {
            let ch = changes_with(&cwd, true, Some(&[side.into()])).unwrap();
            let files = ch["files"].as_array().unwrap();
            assert_eq!(files.len(), 1, "side={side}: {files:?}");
            assert_eq!(files[0]["path"], "new.txt");
            assert_eq!(files[0]["oldPath"], "old.txt");
            assert!(files[0]["status"].as_str().unwrap().contains('R'));
            assert!(files[0]["diff"].as_str().unwrap().contains("rename from old.txt"));
        }

        // Also when renamed and modified in working tree
        std::fs::write(repo.join("new.txt"), "hello\nworld\nmore\n").unwrap();
        for side in ["new.txt", "old.txt"] {
            let ch = changes_with(&cwd, true, Some(&[side.into()])).unwrap();
            let files = ch["files"].as_array().unwrap();
            assert_eq!(files.len(), 1, "modified side={side}: {files:?}");
            assert_eq!(files[0]["path"], "new.txt");
            assert_eq!(files[0]["oldPath"], "old.txt");
            assert!(files[0]["diff"].as_str().unwrap().contains("+more"));
        }
    }
}
