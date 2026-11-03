//! File browsing: list directories, read files, plus writing, creating, renaming and deleting from the session's file panel.

use anyhow::{bail, Result};
use yi_proto::DirEntry;
use serde_json::{json, Value};
use std::io::{Read, Seek, SeekFrom};

const MAX_READ: u64 = 1024 * 1024;
/// Size limit for image previews in the file panel
const MAX_IMAGE: u64 = 10 * 1024 * 1024;

pub fn list(path: &str, show_hidden: bool) -> Result<Value> {
    let dir = crate::paths::expand(path);
    // Strip the \\?\ prefix canonicalize adds before listing, otherwise every child path carries it and the frontend can't match it against the root and git paths
    let dir = dir.canonicalize().map(|p| std::path::PathBuf::from(strip_verbatim(&p.display().to_string()))).unwrap_or(dir);
    let mut entries = vec![];
    for e in std::fs::read_dir(&dir)?.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if !show_hidden && name.starts_with('.') {
            continue;
        }
        let meta = match e.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        // Follow symlinks to decide whether it's a directory
        let is_dir = std::fs::metadata(e.path()).map(|m| m.is_dir()).unwrap_or(meta.is_dir());
        entries.push(DirEntry {
            path: e.path().display().to_string(),
            name,
            is_dir,
            size: meta.len(),
            modified: meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0),
        });
    }
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    let path_str = strip_verbatim(&dir.display().to_string());
    Ok(json!({
        "path": path_str,
        "parent": dir.parent().map(|p| strip_verbatim(&p.display().to_string())),
        "entries": entries,
    }))
}

/// Read a file; with tail true, read only the last part of files that are too large (for viewing logs)
pub fn read(path: &str, tail: bool) -> Result<Value> {
    let p = crate::paths::expand(path);
    let meta = std::fs::metadata(&p)?;
    if meta.is_dir() {
        bail!("这是一个目录");
    }
    let mut buf = vec![];
    let mut f = std::fs::File::open(&p)?;
    if tail && meta.len() > MAX_READ {
        f.seek(SeekFrom::End(-(MAX_READ as i64)))?;
    }
    f.take(MAX_READ).read_to_end(&mut buf)?;
    let binary = buf.iter().take(8000).any(|&b| b == 0);
    // Not valid UTF-8 (GBK and the like): shown lossily, so saving would write U+FFFD over the original bytes.
    // An incomplete sequence at the very end only means the 1MB cut fell inside a character
    let not_utf8 = !binary && std::str::from_utf8(&buf).is_err_and(|e| e.error_len().is_some());
    Ok(json!({
        "path": p.display().to_string(),
        "size": meta.len(),
        "modified": mtime(&meta),
        "truncated": meta.len() > MAX_READ,
        "binary": binary,
        "notUtf8": not_utf8,
        "content": if binary { String::new() } else { String::from_utf8_lossy(&buf).to_string() },
    }))
}

/// Read an image: the whole file as base64, refusing anything over 10MB.
/// `strict` (images referenced in chat replies) checks the file header to confirm it really is an image and refuses otherwise:
/// the path comes from model output, and a reply containing `![](~/.ssh/id_rsa)` mustn't be able to read the private key.
/// `probe` only returns size and type without reading the content, for asking the user before downloading from a remote host
pub fn read_image(path: &str, strict: bool, probe: bool) -> Result<Value> {
    use base64::Engine;
    let p = crate::paths::expand(path);
    let meta = std::fs::metadata(&p)?;
    if meta.is_dir() {
        bail!("这是一个目录");
    }
    let mime = if strict || probe {
        let mut head = vec![];
        std::fs::File::open(&p)?.take(512).read_to_end(&mut head)?;
        match sniff_image(&head) {
            Some(m) => m,
            None if strict => bail!("不是图片文件"),
            None => "",
        }
    } else {
        ""
    };
    let too_big = meta.len() > MAX_IMAGE;
    if probe {
        return Ok(json!({
            "path": p.display().to_string(),
            "size": meta.len(),
            "mimeType": mime,
            "tooBig": too_big,
        }));
    }
    if too_big {
        bail!("图片超过 10MB，无法预览");
    }
    let buf = std::fs::read(&p)?;
    Ok(json!({
        "path": p.display().to_string(),
        "size": meta.len(),
        "modified": mtime(&meta),
        "mimeType": mime,
        "base64": base64::engine::general_purpose::STANDARD.encode(buf),
    }))
}

/// Recognize image formats by file header: PNG / JPEG / GIF / WebP / BMP / ICO / SVG
fn sniff_image(h: &[u8]) -> Option<&'static str> {
    if h.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some("image/png");
    }
    if h.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("image/jpeg");
    }
    if h.starts_with(b"GIF87a") || h.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if h.len() >= 12 && &h[..4] == b"RIFF" && &h[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    if h.starts_with(b"BM") && h.len() >= 14 {
        return Some("image/bmp");
    }
    if h.starts_with(&[0, 0, 1, 0]) && h.len() >= 6 {
        return Some("image/x-icon");
    }
    // SVG is text: after stripping BOM and whitespace it starts with <svg, or with <?xml / a comment / DOCTYPE followed by <svg
    let t = String::from_utf8_lossy(h);
    let t = t.trim_start_matches('\u{feff}').trim_start();
    if t.starts_with("<svg") || ((t.starts_with("<?xml") || t.starts_with("<!--") || t.starts_with("<!DOCTYPE svg")) && t.contains("<svg")) {
        return Some("image/svg+xml");
    }
    None
}

/// Limit for things that need the raw file content, like downloads and PDF previews: 50MB (about 67MB in JSON after base64)
pub(crate) const MAX_RAW: u64 = 50 * 1024 * 1024;

/// Read a whole file as is and return base64 (downloads, PDF previews).
/// With root (web previews), only files inside root may be read: compare after resolving symlinks,
/// so a link to ~/.ssh placed in a repo can't be read by the preview page's scripts
pub fn read_raw(path: &str, root: Option<&str>) -> Result<Value> {
    use base64::Engine;
    let mut p = crate::paths::expand(path);
    if let Some(root) = root {
        let root = std::fs::canonicalize(crate::paths::expand(root))?;
        let real = std::fs::canonicalize(&p)?;
        if !real.starts_with(&root) {
            bail!("不能读取预览目录以外的文件");
        }
        // Read the resolved path, so swapping the link after the check doesn't help
        p = real;
    }
    let meta = std::fs::metadata(&p)?;
    if meta.is_dir() {
        bail!("这是一个目录");
    }
    if meta.len() > MAX_RAW {
        bail!("文件超过 50MB（{:.1}MB），太大了", meta.len() as f64 / 1048576.0);
    }
    let buf = std::fs::read(&p)?;
    Ok(json!({
        "path": p.display().to_string(),
        "size": meta.len(),
        "modified": mtime(&meta),
        "base64": base64::engine::general_purpose::STANDARD.encode(buf),
    }))
}

/// Upload: write base64 content to a file (files dropped onto the file tree). Refuses if it exists and overwrite is false
pub fn upload(path: &str, data: &str, overwrite: bool) -> Result<Value> {
    use base64::Engine;
    let p = crate::paths::expand(path);
    if let Ok(m) = std::fs::metadata(&p) {
        if m.is_dir() {
            bail!("已经有同名文件夹");
        }
        if !overwrite {
            bail!("已经存在同名文件");
        }
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data.as_bytes())
        .map_err(|_| anyhow::anyhow!("文件数据无效"))?;
    if bytes.len() as u64 > MAX_RAW {
        bail!("文件超过 50MB，太大了");
    }
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&p, bytes)?;
    let m = std::fs::metadata(&p)?;
    Ok(json!({ "path": p.display().to_string(), "size": m.len(), "modified": mtime(&m) }))
}

/// Open with the system default program, or reveal in the file manager (local clients only)
pub fn open_native(path: &str, reveal: bool) -> Result<Value> {
    let p = crate::paths::expand(path);
    if !p.exists() {
        bail!("文件不存在: {}", p.display());
    }
    let mut cmd;
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        if reveal {
            cmd = std::process::Command::new("explorer");
            cmd.arg(format!("/select,{}", p.display()));
        } else {
            // start treats its first quoted argument as the window title, so pass an empty title first
            cmd = std::process::Command::new("cmd");
            cmd.args(["/c", "start", ""]).arg(&p);
            cmd.creation_flags(0x0800_0000);
        }
    }
    #[cfg(target_os = "macos")]
    {
        cmd = std::process::Command::new("open");
        if reveal {
            cmd.arg("-R");
        }
        cmd.arg(&p);
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        cmd = std::process::Command::new("xdg-open");
        cmd.arg(if reveal { p.parent().unwrap_or(&p) } else { &p });
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    cmd.spawn().map_err(|e| anyhow::anyhow!("打开失败: {e}"))?;
    Ok(Value::Null)
}

/// Options for [`grep`]. include / exclude are comma-separated globs (VS Code's "files to include / exclude")
#[derive(Default)]
pub struct GrepOpts {
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub regex: bool,
    pub include: String,
    pub exclude: String,
}

/// Turn "a, b" into one regex over /-separated relative paths, following VS Code's search globs: `*` `?` stay
/// within a path segment, `**` crosses them, `{a,b}` alternates, `[..]` is a class. A pattern starting with `/` or `./`
/// is anchored at the root, anything else may match at any depth; a match on a folder also covers everything inside it
fn glob_set(list: &str) -> Result<Option<regex::Regex>> {
    let mut alts = vec![];
    for pat in split_globs(list) {
        let anchored = pat.starts_with('/') || pat.starts_with("./");
        let pat = pat.trim_start_matches("./").trim_start_matches('/').trim_end_matches('/');
        if pat.is_empty() {
            continue;
        }
        let mut re = String::from(if anchored { "" } else { "(?:.*/)?" });
        let cs: Vec<char> = pat.chars().collect();
        let mut i = 0;
        let mut depth = 0;
        while i < cs.len() {
            match cs[i] {
                '*' if cs.get(i + 1) == Some(&'*') => {
                    i += 1;
                    if cs.get(i + 1) == Some(&'/') {
                        i += 1;
                        re.push_str("(?:.*/)?");
                    } else {
                        re.push_str(".*");
                    }
                }
                '*' => re.push_str("[^/]*"),
                '?' => re.push_str("[^/]"),
                '{' => {
                    depth += 1;
                    re.push_str("(?:");
                }
                '}' if depth > 0 => {
                    depth -= 1;
                    re.push(')');
                }
                ',' if depth > 0 => re.push('|'),
                '[' => match cs[i + 1..].iter().position(|&c| c == ']') {
                    Some(n) if n > 0 => {
                        let body: String = cs[i + 1..i + 1 + n].iter().collect();
                        let body = body.replacen('!', "^", usize::from(body.starts_with('!')));
                        re.push('[');
                        re.push_str(&body.replace('\\', "\\\\"));
                        re.push(']');
                        i += n + 1;
                    }
                    _ => re.push_str("\\["),
                },
                c => re.push_str(&regex::escape(&c.to_string())),
            }
            i += 1;
        }
        for _ in 0..depth {
            re.push(')');
        }
        alts.push(format!("(?:{re})"));
    }
    if alts.is_empty() {
        return Ok(None);
    }
    let src = format!("^(?:{})(?:/.*)?$", alts.join("|"));
    // Windows paths are case-insensitive, so are the globs there
    let re = regex::RegexBuilder::new(&src)
        .case_insensitive(cfg!(windows))
        .build()
        .map_err(|e| anyhow::anyhow!("文件范围写法有误: {e}"))?;
    Ok(Some(re))
}

/// Split on commas that aren't inside {..}
fn split_globs(list: &str) -> Vec<&str> {
    let (mut out, mut depth, mut from) = (vec![], 0i32, 0);
    for (i, c) in list.char_indices() {
        match c {
            '{' => depth += 1,
            '}' => depth -= 1,
            ',' if depth <= 0 => {
                out.push(list[from..i].trim());
                from = i + 1;
            }
            _ => {}
        }
    }
    out.push(list[from..].trim());
    out.retain(|s| !s.is_empty());
    out
}

/// Content search: under root (git ls-files in a git repo), find lines matching query file by file.
/// The query is literal text unless `regex` is set; matching is per line. Skips binaries and files over 2MB;
/// keeps only the first 300 characters of each line, at most limit hits
pub fn grep(root: &str, query: &str, o: &GrepOpts, limit: usize) -> Result<Value> {
    let dir = crate::paths::expand(root);
    if !dir.is_dir() {
        bail!("目录不存在: {}", dir.display());
    }
    if query.is_empty() {
        return Ok(json!({ "hits": [], "truncated": false, "files": 0 }));
    }
    let body = if o.regex { query.to_string() } else { regex::escape(query) };
    // Whole word: the characters next to the match must not be letters, digits or _. The regex crate has no lookaround,
    // so those neighbours are matched outside group 1, which holds the actual match
    let src = if o.whole_word { format!(r"(?:^|[^\w])({body})(?:[^\w]|$)") } else { format!("({body})") };
    let re = regex::RegexBuilder::new(&src)
        .case_insensitive(!o.case_sensitive)
        .size_limit(1 << 22)
        .build()
        .map_err(|e| anyhow::anyhow!("正则表达式有误: {e}"))?;
    let include = glob_set(&o.include)?;
    let exclude = glob_set(&o.exclude)?;
    let files = file_list(&dir);
    let started = std::time::Instant::now();
    let mut hits = vec![];
    let mut matched_files = 0usize;
    let mut truncated = false;
    'files: for rel in files.iter() {
        // Don't let a single search hang for too long when there are many files
        if started.elapsed() > std::time::Duration::from_secs(8) {
            truncated = true;
            break;
        }
        if include.as_ref().is_some_and(|g| !g.is_match(rel)) || exclude.as_ref().is_some_and(|g| g.is_match(rel)) {
            continue;
        }
        let full = dir.join(rel);
        let Ok(meta) = std::fs::metadata(&full) else { continue };
        if !meta.is_file() || meta.len() > 2 * 1024 * 1024 {
            continue;
        }
        let Ok(buf) = std::fs::read(&full) else { continue };
        if buf.iter().take(8000).any(|&b| b == 0) {
            continue;
        }
        let text = String::from_utf8_lossy(&buf);
        let mut any = false;
        for (i, line) in text.lines().enumerate() {
            // First non-empty match (a pattern like `a*` matches the empty string everywhere)
            let Some(m) = re.captures_iter(line).filter_map(|c| c.get(1)).find(|m| !m.is_empty()) else { continue };
            // Columns are counted in characters (the frontend highlights by character)
            let ch = line[..m.start()].chars().count();
            let qlen = m.as_str().chars().count();
            let chars: Vec<char> = line.chars().collect();
            // When the match is far into the line, cut off the beginning so the match stays visible
            let start = ch.saturating_sub(60);
            let shown: String = chars[start..].iter().take(300).collect();
            hits.push(json!({
                "path": rel,
                "line": i + 1,
                "col": ch - start,
                "len": qlen.min(300usize.saturating_sub(ch - start)),
                "offset": start,
                "text": shown,
            }));
            any = true;
            if hits.len() >= limit {
                truncated = true;
                matched_files += 1;
                break 'files;
            }
        }
        if any {
            matched_files += 1;
        }
    }
    Ok(json!({ "hits": hits, "truncated": truncated, "files": matched_files }))
}

fn mtime(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Write a file. expect_modified / expect_size are the modification time and size seen when reading: if either differs, someone
/// (e.g. the agent) changed it in between, so refuse to overwrite. Comparing mtime alone misses changes within the same millisecond, or changes where a tool preserved mtime
pub fn write(
    path: &str,
    content: &str,
    expect_modified: Option<i64>,
    expect_size: Option<u64>,
    create_only: bool,
) -> Result<Value> {
    let p = crate::paths::expand(path);
    match std::fs::metadata(&p) {
        Ok(m) if m.is_dir() => bail!("这是一个目录"),
        Ok(_) if create_only => bail!("已经存在同名文件"),
        Ok(m) => {
            let changed = expect_modified.is_some_and(|t| mtime(&m) != t)
                || expect_size.is_some_and(|n| m.len() != n);
            if changed {
                bail!("文件在打开后被修改过，请重新加载再改");
            }
        }
        Err(_) => {}
    }
    std::fs::write(&p, content)?;
    let m = std::fs::metadata(&p)?;
    Ok(json!({ "path": p.display().to_string(), "size": m.len(), "modified": mtime(&m) }))
}

/// Whether two paths name the same file on disk (not just equal strings)
fn same_file(a: &std::path::Path, b: &std::path::Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match (std::fs::symlink_metadata(a), std::fs::symlink_metadata(b)) {
            (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
            _ => false,
        }
    }
    #[cfg(not(unix))]
    {
        match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }
}

pub fn mkdir(path: &str) -> Result<Value> {
    let p = crate::paths::expand(path);
    if p.exists() {
        bail!("已经存在同名文件或文件夹");
    }
    std::fs::create_dir_all(&p)?;
    Ok(json!({ "path": p.display().to_string() }))
}

pub fn rename(from: &str, to: &str) -> Result<Value> {
    let (a, b) = (crate::paths::expand(from), crate::paths::expand(to));
    // On case-insensitive file systems (Windows, macOS) readme.md -> README.md finds "itself" at the target
    if b.exists() && !same_file(&a, &b) {
        bail!("目标已存在：{}", b.display());
    }
    std::fs::rename(&a, &b)?;
    Ok(json!({ "path": b.display().to_string() }))
}

/// Delete a file or empty folder; non-empty folders are refused, so a slip can't wipe out a whole tree
pub fn delete(path: &str) -> Result<Value> {
    let p = crate::paths::expand(path);
    let m = std::fs::symlink_metadata(&p)?;
    if m.is_dir() {
        if std::fs::read_dir(&p)?.next().is_some() {
            bail!("文件夹不是空的，请先删除里面的内容");
        }
        std::fs::remove_dir(&p)?;
    } else {
        std::fs::remove_file(&p)?;
    }
    Ok(Value::Null)
}

/// Browsable roots: existing drive letters on Windows, just / elsewhere
pub fn roots() -> Value {
    #[cfg(windows)]
    {
        let drives: Vec<String> = (b'A'..=b'Z')
            .map(|c| format!("{}:\\", c as char))
            .filter(|d| std::path::Path::new(d).exists())
            .collect();
        json!(drives)
    }
    #[cfg(not(windows))]
    json!(["/"])
}

/// Windows canonicalize adds a `\\?\` prefix; strip it for display
fn strip_verbatim(s: &str) -> String {
    match s.strip_prefix(r"\\?\") {
        // Only strip the prefix from drive-letter paths; \\?\UNC\server\share becomes \\server\share again
        Some(r) if r.as_bytes().get(1) == Some(&b':') => r.to_string(),
        Some(r) if r.starts_with(r"UNC\") => format!(r"\\{}", &r[4..]),
        _ => s.to_string(),
    }
}

/// Cache of file lists under a directory: (root, time fetched, relative paths)
static FILE_CACHE: std::sync::Mutex<Option<(String, std::time::Instant, std::sync::Arc<Vec<String>>)>> =
    std::sync::Mutex::new(None);

const SKIP_DIRS: [&str; 9] = [".git", "node_modules", "target", "dist", "build", ".venv", "__pycache__", ".next", ".gradle"];
const MAX_FILES: usize = 50_000;

/// List files under root (relative paths, /-separated). git repos use git ls-files (respecting .gitignore); otherwise walk the tree ourselves
fn file_list(root: &std::path::Path) -> std::sync::Arc<Vec<String>> {
    let key = root.display().to_string();
    if let Some((k, t, v)) = FILE_CACHE.lock().unwrap().as_ref() {
        if *k == key && t.elapsed() < std::time::Duration::from_secs(20) {
            return v.clone();
        }
    }
    let list = git_files(root).unwrap_or_else(|| walk_files(root));
    let list = std::sync::Arc::new(list);
    *FILE_CACHE.lock().unwrap() = Some((key, std::time::Instant::now(), list.clone()));
    list
}

fn git_files(root: &std::path::Path) -> Option<Vec<String>> {
    let mut cmd = std::process::Command::new("git");
    cmd.args(["ls-files", "-co", "--exclude-standard", "-z"]).current_dir(root);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let out = cmd.stderr(std::process::Stdio::null()).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(
        out.stdout
            .split(|&b| b == 0)
            .filter(|s| !s.is_empty())
            .take(MAX_FILES)
            .map(|s| String::from_utf8_lossy(s).to_string())
            .collect(),
    )
}

fn walk_files(root: &std::path::Path) -> Vec<String> {
    let mut out = vec![];
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                if depth < 12 && !SKIP_DIRS.contains(&name.as_str()) && !name.starts_with('.') {
                    stack.push((e.path(), depth + 1));
                }
            } else if let Ok(rel) = e.path().strip_prefix(root) {
                out.push(rel.to_string_lossy().replace('\\', "/"));
                if out.len() >= MAX_FILES {
                    return out;
                }
            }
        }
    }
    out
}

/// Fuzzy match score: file name contains > path contains > characters in order; shorter ranks higher. None on no match
fn score(path: &str, q: &str) -> Option<i64> {
    let p = path.to_lowercase();
    let name = p.rsplit('/').next().unwrap_or(&p);
    let len = p.chars().count() as i64;
    if name.starts_with(q) {
        return Some(4000 - len);
    }
    if name.contains(q) {
        return Some(3000 - len);
    }
    if p.contains(q) {
        return Some(2000 - len);
    }
    let mut it = p.chars();
    for c in q.chars().filter(|c| !c.is_whitespace()) {
        it.find(|&x| x == c)?;
    }
    Some(1000 - len)
}

/// @ files in the input box: fuzzy-find files and directories by name under root
pub fn search(root: &str, query: &str, limit: usize) -> Result<Value> {
    let dir = crate::paths::expand(root);
    if !dir.is_dir() {
        bail!("目录不存在: {}", dir.display());
    }
    let files = file_list(&dir);
    let q = query.trim().to_lowercase().replace('\\', "/");
    // Directories are derived from the file paths
    let mut dirs: std::collections::BTreeSet<&str> = Default::default();
    for f in files.iter() {
        let mut p = f.as_str();
        while let Some((parent, _)) = p.rsplit_once('/') {
            if !dirs.insert(parent) {
                break;
            }
            p = parent;
        }
    }
    let mut hits: Vec<(i64, &str, bool)> = vec![];
    if q.is_empty() {
        // With no input, return top-level directories and files
        hits.extend(dirs.iter().filter(|d| !d.contains('/')).map(|d| (0, *d, true)));
        hits.extend(files.iter().filter(|f| !f.contains('/')).map(|f| (0, f.as_str(), false)));
    } else {
        hits.extend(dirs.iter().filter_map(|d| score(d, &q).map(|s| (s + 50, *d, true))));
        hits.extend(files.iter().filter_map(|f| score(f, &q).map(|s| (s, f.as_str(), false))));
        hits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
    }
    let list: Vec<Value> = hits
        .into_iter()
        .take(limit)
        .map(|(_, p, d)| json!({ "path": p, "isDir": d }))
        .collect();
    Ok(json!(list))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniff_images() {
        assert_eq!(sniff_image(b"\x89PNG\r\n\x1a\nxxxx"), Some("image/png"));
        assert_eq!(sniff_image(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("image/jpeg"));
        assert_eq!(sniff_image(b"GIF89a.."), Some("image/gif"));
        assert_eq!(sniff_image(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));
        assert_eq!(sniff_image("\u{feff}  <?xml version=\"1.0\"?>\n<svg xmlns=…>".as_bytes()), Some("image/svg+xml"));
        assert_eq!(sniff_image(b"-----BEGIN OPENSSH PRIVATE KEY-----"), None);
        assert_eq!(sniff_image(b"<?xml version=\"1.0\"?><project/>"), None);
        assert_eq!(sniff_image(b""), None);
    }

    #[test]
    fn strict_image_read() {
        let dir = std::env::temp_dir().join(format!("ah-img-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("a.png");
        std::fs::write(&png, b"\x89PNG\r\n\x1a\n0123").unwrap();
        let key = dir.join("id_rsa");
        std::fs::write(&key, b"-----BEGIN OPENSSH PRIVATE KEY-----").unwrap();
        let r = read_image(png.to_str().unwrap(), true, true).unwrap();
        assert_eq!(r["mimeType"], "image/png");
        assert_eq!(r["size"], 12);
        assert!(r.get("base64").is_none());
        let r = read_image(png.to_str().unwrap(), true, false).unwrap();
        assert!(r["base64"].as_str().unwrap().len() > 0);
        assert!(read_image(key.to_str().unwrap(), true, false).is_err());
        assert!(read_image(key.to_str().unwrap(), true, true).is_err());
        // The file panel's original calls aren't checked
        assert!(read_image(key.to_str().unwrap(), false, false).is_ok());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fuzzy_score() {
        assert!(score("src/hub.rs", "hub").unwrap() > score("src/github/x.rs", "hub").unwrap());
        assert!(score("app/src/store.ts", "ass").is_some());
        assert!(score("app/src/store.ts", "zz").is_none());
    }

    #[test]
    fn write_rejects_changed_file() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("a.txt");
        let p = f.to_str().unwrap();
        let r = write(p, "abc", None, None, false).unwrap();
        let (t, n) = (r["modified"].as_i64().unwrap(), r["size"].as_u64().unwrap());
        // Same mtime but a different size (changed within the same millisecond, or the change preserved mtime) must be stopped too
        std::fs::write(&f, "abcdef").unwrap();
        let mtime_now = mtime(&std::fs::metadata(&f).unwrap());
        assert!(write(p, "x", Some(mtime_now), Some(n), false).is_err());
        assert!(write(p, "x", Some(t - 1), Some(6), false).is_err());
        write(p, "x", Some(mtime_now), Some(6), false).unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "x");
    }

    #[test]
    fn grep_finds_lines() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.txt"), "hello World\nfoo bar\nworldwide\n").unwrap();
        std::fs::write(d.path().join("b.bin"), b"world\0\0").unwrap();
        let root = d.path().display().to_string();
        let o = |cs, ww, regex| GrepOpts { case_sensitive: cs, whole_word: ww, regex, ..Default::default() };
        let r = grep(&root, "world", &o(false, false, false), 10).unwrap();
        let hits = r["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0]["line"], 1);
        assert_eq!(hits[0]["col"], 6);
        let r = grep(&root, "world", &o(false, true, false), 10).unwrap();
        assert_eq!(r["hits"].as_array().unwrap().len(), 1);
        let r = grep(&root, "World", &o(true, false, false), 10).unwrap();
        assert_eq!(r["hits"].as_array().unwrap().len(), 1);
        // Literal mode escapes regex syntax; regex mode reports the matched length
        assert_eq!(grep(&root, "o.b", &o(false, false, false), 10).unwrap()["hits"].as_array().unwrap().len(), 0);
        let r = grep(&root, "o+ b", &o(false, false, true), 10).unwrap();
        assert_eq!(r["hits"][0]["col"], 1);
        assert_eq!(r["hits"][0]["len"], 4);
        assert!(grep(&root, "(", &o(false, false, true), 10).is_err());
    }

    #[test]
    fn read_flags_non_utf8() {
        let d = tempfile::tempdir().unwrap();
        let (gbk, utf) = (d.path().join("gbk.txt"), d.path().join("utf.txt"));
        std::fs::write(&gbk, b"\xc4\xe3\xba\xc3\n").unwrap(); // "你好" in GBK
        std::fs::write(&utf, "你好\r\n").unwrap();
        assert_eq!(read(&gbk.display().to_string(), false).unwrap()["notUtf8"], true);
        let r = read(&utf.display().to_string(), false).unwrap();
        assert_eq!(r["notUtf8"], false);
        assert_eq!(r["content"], "你好\r\n");
    }

    #[test]
    fn rename_refuses_existing_but_allows_case_change() {
        let d = tempfile::tempdir().unwrap();
        let (a, b) = (d.path().join("a.txt"), d.path().join("b.txt"));
        std::fs::write(&a, "1").unwrap();
        std::fs::write(&b, "2").unwrap();
        assert!(rename(&a.display().to_string(), &b.display().to_string()).is_err());
        let up = d.path().join("A.txt");
        rename(&a.display().to_string(), &up.display().to_string()).unwrap();
        let names: Vec<String> = std::fs::read_dir(d.path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into()).collect();
        assert!(names.contains(&"A.txt".to_string()), "{names:?}");
    }

    #[test]
    fn grep_include_exclude() {
        let d = tempfile::tempdir().unwrap();
        for f in ["src/a.ts", "src/deep/b.ts", "lib/c.ts", "src/d.rs", "docs/src/e.ts"] {
            let p = d.path().join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, "needle\n").unwrap();
        }
        let root = d.path().display().to_string();
        let files = |inc: &str, exc: &str| {
            let o = GrepOpts { include: inc.into(), exclude: exc.into(), ..Default::default() };
            let r = grep(&root, "needle", &o, 100).unwrap();
            let mut v: Vec<String> = r["hits"].as_array().unwrap().iter().map(|h| h["path"].as_str().unwrap().to_string()).collect();
            v.sort();
            v
        };
        assert_eq!(files("*.rs", ""), ["src/d.rs"]);
        assert_eq!(files("src", ""), ["docs/src/e.ts", "src/a.ts", "src/d.rs", "src/deep/b.ts"]);
        assert_eq!(files("./src", "**/deep"), ["src/a.ts", "src/d.rs"]);
        assert_eq!(files("src/*.{ts,rs}", ""), ["docs/src/e.ts", "src/a.ts", "src/d.rs"]);
        assert_eq!(files("", "src, lib"), Vec::<String>::new());
        assert_eq!(files("", "*.ts"), ["src/d.rs"]);
    }

    #[cfg(unix)]
    #[test]
    fn read_raw_stays_inside_root() {
        let d = tempfile::tempdir().unwrap();
        let (site, secret) = (d.path().join("site"), d.path().join("secret.txt"));
        std::fs::create_dir_all(site.join("css")).unwrap();
        std::fs::write(&secret, "key").unwrap();
        std::fs::write(site.join("css/a.css"), "body{}").unwrap();
        std::os::unix::fs::symlink(&secret, site.join("x.txt")).unwrap();
        std::os::unix::fs::symlink(d.path(), site.join("up")).unwrap();
        std::os::unix::fs::symlink(site.join("css/a.css"), site.join("b.css")).unwrap();
        let root = site.to_str().unwrap();
        let rd = |rel: &str| read_raw(site.join(rel).to_str().unwrap(), Some(root));
        assert!(rd("css/a.css").is_ok());
        // Links pointing inside the directory are fine
        assert!(rd("b.css").is_ok());
        // Files pointing outside, or paths through directories pointing outside, are refused
        assert!(rd("x.txt").is_err());
        assert!(rd("up/secret.txt").is_err());
        assert!(read_raw(secret.to_str().unwrap(), Some(root)).is_err());
        // The root itself being a link works as usual
        let link_root = d.path().join("site-link");
        std::os::unix::fs::symlink(&site, &link_root).unwrap();
        let p = link_root.join("css/a.css");
        assert!(read_raw(p.to_str().unwrap(), Some(link_root.to_str().unwrap())).is_ok());
    }

    #[test]
    fn upload_and_read_raw() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("sub/x.bin").display().to_string();
        upload(&p, "AAEC", false).unwrap();
        assert!(upload(&p, "AAEC", false).is_err());
        assert_eq!(read_raw(&p, None).unwrap()["base64"], "AAEC");
    }

    #[test]
    fn search_walk() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("src/a")).unwrap();
        std::fs::create_dir_all(d.path().join("node_modules/x")).unwrap();
        std::fs::write(d.path().join("src/a/main.rs"), "").unwrap();
        std::fs::write(d.path().join("node_modules/x/main.js"), "").unwrap();
        let r = search(&d.path().display().to_string(), "main", 10).unwrap();
        let paths: Vec<&str> = r.as_array().unwrap().iter().map(|v| v["path"].as_str().unwrap()).collect();
        assert_eq!(paths, vec!["src/a/main.rs"]);
    }
}

#[cfg(test)]
mod verbatim_tests {
    #[test]
    fn strip() {
        assert_eq!(super::strip_verbatim(r"\\?\D:\git\a"), r"D:\git\a");
        assert_eq!(super::strip_verbatim(r"\\?\UNC\srv\share\x"), r"\\srv\share\x");
        assert_eq!(super::strip_verbatim("/home/a"), "/home/a");
    }
}
