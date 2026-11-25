//! Workspace code navigation: go-to-definition, import/path resolution, and references lookup.
//! Designed behind a `NavProvider` trait so an optional LSP provider can be plugged in later without changing RPC callers.

use anyhow::{bail, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Instant;

const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_PREVIEW_CHARS: usize = 200;
const MAX_CONTEXT_CHARS: usize = 260;
const MAX_CACHED_WORKSPACES: usize = 4;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DefRequest {
    pub root: String,
    #[serde(default)]
    pub path: String,
    #[serde(default = "default_line")]
    pub line: usize,
    /// 0-based UTF-16 code-unit column within `line`.
    #[serde(default)]
    pub col: usize,
    pub symbol: Option<String>,
    pub kind: Option<String>,
    pub path_text: Option<String>,
    pub container: Option<String>,
    pub content: Option<String>,
    #[serde(default = "default_def_limit")]
    pub limit: usize,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RefRequest {
    pub root: String,
    #[serde(default)]
    pub path: String,
    #[serde(default = "default_line")]
    pub line: usize,
    /// 0-based UTF-16 code-unit column within `line`.
    #[serde(default)]
    pub col: usize,
    pub symbol: String,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub include_declaration: bool,
    #[serde(default = "default_ctx_lines")]
    pub context_lines: usize,
    #[serde(default = "default_ref_limit")]
    pub limit: usize,
}

fn default_line() -> usize {
    1
}
fn default_def_limit() -> usize {
    30
}
fn default_ctx_lines() -> usize {
    3
}
fn default_ref_limit() -> usize {
    200
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct NavLocation {
    pub path: String,
    pub abs_path: String,
    pub line: usize,
    pub col: usize,
    pub len: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_line: Option<usize>,
    pub kind: String,
    pub symbol: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    pub preview: String,
    pub score: i32,
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NavDefinitionResult {
    pub provider: String,
    pub kind: String,
    pub query: String,
    pub at_definition: bool,
    pub locations: Vec<NavLocation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NavReferenceHit {
    pub path: String,
    pub abs_path: String,
    pub line: usize,
    pub col: usize,
    pub len: usize,
    pub text: String,
    pub is_definition: bool,
    pub context_start_line: usize,
    pub context: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NavReferencesResult {
    pub provider: String,
    pub symbol: String,
    pub truncated: bool,
    pub total_files: usize,
    pub total_hits: usize,
    pub hits: Vec<NavReferenceHit>,
}

/// Provider interface for code navigation backends (built-in index today; optional LSP adapters later).
pub trait NavProvider: Send + Sync {
    fn name(&self) -> &'static str;
    fn definition(&self, req: &DefRequest) -> Result<NavDefinitionResult>;
    fn references(&self, req: &RefRequest) -> Result<NavReferencesResult>;
}

pub struct BuiltinNavProvider;

impl NavProvider for BuiltinNavProvider {
    fn name(&self) -> &'static str {
        "builtin"
    }

    fn definition(&self, req: &DefRequest) -> Result<NavDefinitionResult> {
        resolve_definition(self.name(), req)
    }

    fn references(&self, req: &RefRequest) -> Result<NavReferencesResult> {
        find_references(self.name(), req)
    }
}

pub fn definition(req: DefRequest) -> Result<Value> {
    let res = BuiltinNavProvider.definition(&req)?;
    Ok(serde_json::to_value(res)?)
}

pub fn references(req: RefRequest) -> Result<Value> {
    let res = BuiltinNavProvider.references(&req)?;
    Ok(serde_json::to_value(res)?)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum LangFamily {
    JsTs,
    Rust,
    Python,
    Go,
    Jvm,
    CStyle,
    Ruby,
    Php,
    Shell,
    Css,
    Markup,
    Other,
}

impl LangFamily {
    fn from_path(path: &str) -> Self {
        let base = if cfg!(windows) {
            path.rsplit(['/', '\\']).next().unwrap_or(path)
        } else {
            path.rsplit('/').next().unwrap_or(path)
        };
        let ext = base.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
        match ext.as_deref() {
            Some("js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs" | "mts" | "cts" | "vue" | "svelte") => Self::JsTs,
            Some("rs") => Self::Rust,
            Some("py" | "pyi") => Self::Python,
            Some("go") => Self::Go,
            Some("java" | "kt" | "kts" | "scala" | "cs") => Self::Jvm,
            Some("c" | "h" | "cc" | "cpp" | "hpp" | "cxx" | "m" | "mm") => Self::CStyle,
            Some("rb") => Self::Ruby,
            Some("php") => Self::Php,
            Some("sh" | "bash" | "zsh") => Self::Shell,
            Some("css" | "scss" | "sass" | "less") => Self::Css,
            Some("md" | "markdown" | "mdx" | "html" | "htm" | "xml" | "toml" | "yaml" | "yml" | "ini" | "json") => Self::Markup,
            _ => Self::Other,
        }
    }

    fn is_code(self) -> bool {
        !matches!(self, Self::Css | Self::Markup | Self::Other)
    }
}

#[derive(Debug, Clone)]
struct IndexedDef {
    name: String,
    kind: &'static str,
    line: usize,
    col: usize,
    len: usize,
    container: Option<String>,
    exported: bool,
    preview: String,
}

#[derive(Debug, Clone)]
struct IndexedFile {
    mtime: i64,
    size: u64,
    lang: LangFamily,
    defs: Vec<IndexedDef>,
}

#[derive(Default)]
struct WorkspaceIndex {
    files: HashMap<String, Arc<IndexedFile>>,
    by_name: HashMap<String, Vec<(String, usize)>>,
    last_used: u64,
}

impl WorkspaceIndex {
    fn rebuild_lookup(&mut self) {
        self.by_name.clear();
        let mut paths: Vec<&String> = self.files.keys().collect();
        paths.sort();
        for p in paths {
            if let Some(f) = self.files.get(p) {
                for (idx, d) in f.defs.iter().enumerate() {
                    self.by_name.entry(d.name.clone()).or_default().push((p.clone(), idx));
                }
            }
        }
    }
}

static INDEX_CACHE: Mutex<Option<(u64, HashMap<String, WorkspaceIndex>)>> = Mutex::new(None);
static WORKSPACE_UPDATE_LOCKS: OnceLock<Mutex<HashMap<String, Weak<Mutex<()>>>>> = OnceLock::new();
static TSCONFIG_CACHE: Mutex<Option<HashMap<String, CachedTsConfig>>> = Mutex::new(None);

fn workspace_cache_key(root_canon: &Path) -> String {
    norm_slashes(&root_canon.display().to_string()).trim_end_matches('/').to_string()
}

fn workspace_update_lock(root_key: &str) -> Arc<Mutex<()>> {
    let map_mutex = WORKSPACE_UPDATE_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut map = map_mutex.lock().unwrap_or_else(|e| e.into_inner());
    map.retain(|_, w| w.strong_count() > 0);
    if let Some(existing) = map.get(root_key).and_then(Weak::upgrade) {
        return existing;
    }
    let lock = Arc::new(Mutex::new(()));
    map.insert(root_key.to_string(), Arc::downgrade(&lock));
    lock
}

fn path_strictly_under(root: &Path, candidate: &Path) -> bool {
    let mut root_comps = root.components();
    let mut cand_comps = candidate.components();
    for rc in root_comps.by_ref() {
        if cand_comps.next() != Some(rc) {
            return false;
        }
    }
    cand_comps.next().is_some()
}

#[derive(Clone)]
struct CachedTsConfig {
    mtime: i64,
    base_url: PathBuf,
    paths: Vec<(String, Vec<String>)>,
}

fn canon_clean(p: &Path) -> Option<PathBuf> {
    p.canonicalize()
        .ok()
        .map(|c| PathBuf::from(crate::fsapi::strip_verbatim(&c.display().to_string())))
}

fn expand_root(raw: &str) -> Result<PathBuf> {
    let p = crate::paths::expand(raw);
    let canon = canon_clean(&p).unwrap_or(p);
    if !canon.is_dir() {
        bail!("目录不存在: {}", canon.display());
    }
    Ok(canon)
}

fn norm_slashes(p: &str) -> String {
    if cfg!(windows) {
        p.replace('\\', "/")
    } else {
        p.to_string()
    }
}

fn norm_cmp(p: &str) -> String {
    let s = norm_slashes(p).trim_end_matches('/').to_string();
    if cfg!(windows) {
        s.to_lowercase()
    } else {
        s
    }
}

fn is_safe_rel(rel: &str) -> bool {
    if rel.is_empty() || rel.contains('\0') {
        return false;
    }
    if cfg!(windows) {
        !rel.starts_with('/')
            && !rel.starts_with('\\')
            && !rel.contains(':')
            && rel.split(['/', '\\']).all(|seg| !seg.is_empty() && seg != "." && seg != "..")
    } else {
        !rel.starts_with('/') && rel.split('/').all(|seg| !seg.is_empty() && seg != "." && seg != "..")
    }
}

fn canonical_contained_path(root_canon: &Path, candidate: &Path) -> Option<(PathBuf, String)> {
    let canon = canon_clean(candidate)?;
    if !canon.is_file() || !canon.starts_with(root_canon) {
        return None;
    }
    let rel = norm_slashes(&canon.strip_prefix(root_canon).ok()?.to_string_lossy());
    Some((canon, rel))
}

fn normalize_rel_path(root_canon: &Path, raw_root: Option<&Path>, path: &str) -> String {
    if path.is_empty() {
        return String::new();
    }
    let stripped = crate::fsapi::strip_verbatim(path);
    let p_str = norm_slashes(&stripped);
    let p_cmp = norm_cmp(&p_str);
    for base in [Some(root_canon), raw_root].into_iter().flatten() {
        let root_str = norm_slashes(&crate::fsapi::strip_verbatim(&base.display().to_string()))
            .trim_end_matches('/')
            .to_string();
        let root_cmp = norm_cmp(&root_str);
        if p_cmp == root_cmp {
            return String::new();
        }
        let prefix = format!("{root_cmp}/");
        if p_cmp.starts_with(&prefix) && p_str.is_char_boundary(root_str.len() + 1) {
            let rel = &p_str[root_str.len() + 1..];
            return if is_safe_rel(rel) { rel.to_string() } else { String::new() };
        }
    }
    let p_path = Path::new(&stripped);
    if p_path.is_absolute() {
        return canonical_contained_path(root_canon, p_path).map(|(_, rel)| rel).unwrap_or_default();
    }
    let rel = p_str.trim_start_matches("./");
    if is_safe_rel(rel) {
        rel.to_string()
    } else {
        String::new()
    }
}

fn join_abs(root: &Path, rel: &str) -> String {
    let mut pb = root.to_path_buf();
    #[cfg(windows)]
    for seg in rel.split(['/', '\\']).filter(|s| !s.is_empty() && *s != ".") {
        pb.push(seg);
    }
    #[cfg(not(windows))]
    for seg in rel.split('/').filter(|s| !s.is_empty() && *s != ".") {
        pb.push(seg);
    }
    crate::fsapi::strip_verbatim(&pb.display().to_string())
}

fn mtime_ms(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn is_ignored_asset(rel: &str) -> bool {
    let lower = rel.to_ascii_lowercase();
    let name = lower.rsplit('/').next().unwrap_or(&lower);
    if matches!(
        name,
        "package-lock.json" | "cargo.lock" | "yarn.lock" | "pnpm-lock.yaml" | "bun.lockb" | "composer.lock"
    ) {
        return true;
    }
    lower.ends_with(".min.js")
        || lower.ends_with(".min.css")
        || lower.ends_with(".map")
        || lower.ends_with(".lock")
        || lower.ends_with(".wasm")
        || lower.ends_with(".png")
        || lower.ends_with(".jpg")
        || lower.ends_with(".jpeg")
        || lower.ends_with(".gif")
        || lower.ends_with(".webp")
        || lower.ends_with(".ico")
        || lower.ends_with(".pdf")
}

fn is_test_path(rel: &str) -> bool {
    let lower = rel.to_ascii_lowercase();
    lower.contains("/test/")
        || lower.contains("/tests/")
        || lower.contains("/__tests__/")
        || lower.starts_with("tests/")
        || lower.starts_with("test/")
        || lower.ends_with("_test.rs")
        || lower.ends_with("_test.go")
        || lower.ends_with("_test.py")
        || lower.contains(".test.")
        || lower.contains(".spec.")
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

fn trunc_chars(s: &str, max_chars: usize) -> String {
    let trimmed = s.trim();
    let count = trimmed.chars().count();
    if count <= max_chars {
        trimmed.to_string()
    } else {
        let mut out: String = trimmed.chars().take(max_chars.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

fn trunc_line(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max_chars.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

fn line_spans(text: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).collect();
    if lines.is_empty() {
        lines.push("");
    }
    lines
}

fn read_text_from_file(mut file: std::fs::File) -> Option<String> {
    use std::io::Read;
    let mut buf = Vec::new();
    file.by_ref().take(MAX_FILE_BYTES + 1).read_to_end(&mut buf).ok()?;
    if buf.len() as u64 > MAX_FILE_BYTES {
        return None;
    }
    if buf.iter().take(8000).any(|&b| b == 0) {
        return None;
    }
    Some(String::from_utf8_lossy(&buf).into_owned())
}

#[cfg(unix)]
fn unix_dir_open_flags() -> libc::c_int {
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
    {
        libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC
    }
    #[cfg(all(
        not(any(target_os = "linux", target_os = "android", target_os = "freebsd")),
        any(target_vendor = "apple", target_os = "netbsd", target_os = "solaris", target_os = "illumos")
    ))]
    {
        libc::O_SEARCH | libc::O_DIRECTORY | libc::O_CLOEXEC
    }
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "freebsd",
        target_vendor = "apple",
        target_os = "netbsd",
        target_os = "solaris",
        target_os = "illumos"
    )))]
    {
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC
    }
}

#[cfg(unix)]
struct DirHandle(std::os::unix::io::OwnedFd);

#[cfg(unix)]
impl DirHandle {
    fn open_root(root_canon: &Path) -> Option<Self> {
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
        use std::path::Component;

        if !root_canon.is_absolute() {
            return None;
        }
        let dir_flags = unix_dir_open_flags();
        let slash = std::ffi::CString::new("/").ok()?;
        let root_fd = unsafe { libc::open(slash.as_ptr(), dir_flags) };
        if root_fd < 0 {
            return None;
        }
        let mut cur = unsafe { OwnedFd::from_raw_fd(root_fd) };
        for comp in root_canon.components() {
            match comp {
                Component::RootDir => continue,
                Component::Normal(seg) => {
                    let cstr = std::ffi::CString::new(seg.as_bytes()).ok()?;
                    let fd = unsafe {
                        libc::openat(cur.as_raw_fd(), cstr.as_ptr(), dir_flags | libc::O_NOFOLLOW)
                    };
                    if fd < 0 {
                        return None;
                    }
                    cur = unsafe { OwnedFd::from_raw_fd(fd) };
                }
                Component::CurDir | Component::ParentDir | Component::Prefix(_) => return None,
            }
        }
        Some(Self(cur))
    }

    fn open_rel_nofollow(&self, rel: &str) -> std::result::Result<std::fs::File, Option<i32>> {
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
        use std::path::Component;

        let mut comps: Vec<&std::ffi::OsStr> = Vec::new();
        for comp in Path::new(rel).components() {
            match comp {
                Component::Normal(seg) => comps.push(seg),
                Component::CurDir => continue,
                Component::RootDir | Component::ParentDir | Component::Prefix(_) => return Err(None),
            }
        }
        if comps.is_empty() {
            return Err(None);
        }
        let dir_flags = unix_dir_open_flags() | libc::O_NOFOLLOW;
        let mut dir_holder: Option<OwnedFd> = None;
        for comp in &comps[..comps.len() - 1] {
            let parent_fd = dir_holder.as_ref().map_or(self.0.as_raw_fd(), |f| f.as_raw_fd());
            let cstr = std::ffi::CString::new(comp.as_bytes()).map_err(|_| None)?;
            let fd = unsafe { libc::openat(parent_fd, cstr.as_ptr(), dir_flags) };
            if fd < 0 {
                return Err(std::io::Error::last_os_error().raw_os_error());
            }
            dir_holder = Some(unsafe { OwnedFd::from_raw_fd(fd) });
        }
        let parent_fd = dir_holder.as_ref().map_or(self.0.as_raw_fd(), |f| f.as_raw_fd());
        let last = std::ffi::CString::new(comps[comps.len() - 1].as_bytes()).map_err(|_| None)?;
        let fd = unsafe {
            libc::openat(
                parent_fd,
                last.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().raw_os_error());
        }
        Ok(std::fs::File::from(unsafe { OwnedFd::from_raw_fd(fd) }))
    }

    fn open_contained(&self, root_canon: &Path, rel: &str) -> Option<(std::fs::File, std::fs::Metadata)> {
        let file = match self.open_rel_nofollow(rel) {
            Ok(f) => f,
            // Only retry once after resolving symlinks strictly inside root_canon, then re-open with O_NOFOLLOW
            Err(Some(err)) if err == libc::ELOOP || err == libc::ENOTDIR => {
                let (_, resolved_rel) = canonical_contained_path(root_canon, &root_canon.join(rel))?;
                if !is_safe_rel(&resolved_rel) {
                    return None;
                }
                self.open_rel_nofollow(&resolved_rel).ok()?
            }
            Err(_) => return None,
        };
        let meta = file.metadata().ok()?;
        if !meta.is_file() || meta.len() > MAX_FILE_BYTES {
            return None;
        }
        Some((file, meta))
    }
}

#[cfg(windows)]
fn handle_final_path(file: &std::fs::File) -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{GetFinalPathNameByHandleW, FILE_NAME_NORMALIZED};

    let h = file.as_raw_handle() as _;
    let mut buf = vec![0u16; 260];
    loop {
        let len = unsafe { GetFinalPathNameByHandleW(h, buf.as_mut_ptr(), buf.len() as u32, FILE_NAME_NORMALIZED) };
        if len == 0 {
            return None;
        }
        let len = len as usize;
        if len < buf.len() {
            return Some(PathBuf::from(std::ffi::OsString::from_wide(&buf[..len])));
        }
        buf.resize(len + 1, 0);
    }
}

#[cfg(windows)]
fn open_dir_handle(path: &Path) -> Option<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;

    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
        .ok()
}

#[cfg(windows)]
struct DirHandle {
    root_final: PathBuf,
}

#[cfg(windows)]
impl DirHandle {
    fn open_root(root_canon: &Path) -> Option<Self> {
        let root_dir = open_dir_handle(root_canon)?;
        let root_final = handle_final_path(&root_dir)?;
        Some(Self { root_final })
    }

    fn open_contained(&self, root_canon: &Path, rel: &str) -> Option<(std::fs::File, std::fs::Metadata)> {
        if !is_safe_rel(rel) {
            return None;
        }
        let file = std::fs::File::open(root_canon.join(rel)).ok()?;
        let final_path = handle_final_path(&file)?;
        if !path_strictly_under(&self.root_final, &final_path) {
            return None;
        }
        let meta = file.metadata().ok()?;
        if !meta.is_file() || meta.len() > MAX_FILE_BYTES {
            return None;
        }
        Some((file, meta))
    }
}

fn read_contained_text_file_with(root_canon: &Path, dir: &DirHandle, candidate: &Path) -> Option<String> {
    let (_, rel) = canonical_contained_path(root_canon, candidate)?;
    let (file, _) = dir.open_contained(root_canon, &rel)?;
    read_text_from_file(file)
}

fn read_contained_text_file(root_canon: &Path, candidate: &Path) -> Option<String> {
    let root_clean = canon_clean(root_canon).unwrap_or_else(|| root_canon.to_path_buf());
    let dir = DirHandle::open_root(&root_clean)?;
    read_contained_text_file_with(&root_clean, &dir, candidate)
}

fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

fn utf16_col(line: &str, byte_idx: usize) -> usize {
    line[..byte_idx].encode_utf16().count()
}

/// Count net `{` minus `}` on a line while skipping strings and trailing `//` comments.
fn brace_delta(line: &str) -> i32 {
    let bytes = line.as_bytes();
    let mut i = 0;
    let mut delta = 0i32;
    let mut quote: Option<u8> = None;
    while i < bytes.len() {
        let b = bytes[i];
        if let Some(q) = quote {
            if b == b'\\' {
                i += 2;
                continue;
            }
            if b == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        if b == b'/' && bytes.get(i + 1) == Some(&b'/') {
            break;
        }
        match b {
            b'"' | b'\'' | b'`' => quote = Some(b),
            b'{' => delta += 1,
            b'}' => delta -= 1,
            _ => {}
        }
        i += 1;
    }
    delta
}

fn is_import_line(lang: LangFamily, trimmed: &str) -> bool {
    match lang {
        LangFamily::JsTs => {
            (trimmed.starts_with("import ") && !trimmed.starts_with("import("))
                || (trimmed.starts_with("export ") && trimmed.contains(" from "))
        }
        LangFamily::Rust => trimmed.starts_with("use ") || trimmed.starts_with("pub use "),
        LangFamily::Python => trimmed.starts_with("import ") || trimmed.starts_with("from "),
        LangFamily::CStyle => trimmed.starts_with("#include"),
        LangFamily::Go => trimmed.starts_with("import "),
        _ => false,
    }
}

struct LangRules {
    js_fn: Regex,
    js_class: Regex,
    js_iface: Regex,
    js_type: Regex,
    js_enum: Regex,
    js_arrow: Regex,
    js_var: Regex,
    js_method: Regex,
    rs_fn: Regex,
    rs_struct: Regex,
    rs_enum: Regex,
    rs_trait: Regex,
    rs_type: Regex,
    rs_const: Regex,
    rs_mod_block: Regex,
    rs_macro: Regex,
    rs_impl: Regex,
    py_def: Regex,
    py_class: Regex,
    py_var: Regex,
    go_method: Regex,
    go_fn: Regex,
    go_type: Regex,
    go_var: Regex,
    jvm_type: Regex,
    jvm_kt_fn: Regex,
    jvm_method: Regex,
    c_macro: Regex,
    c_struct: Regex,
    c_fn: Regex,
    rb_def: Regex,
    rb_class: Regex,
    php_fn: Regex,
    php_class: Regex,
    sh_fn: Regex,
    css_var: Regex,
    css_sel: Regex,
    md_head: Regex,
}

fn rules() -> &'static LangRules {
    static RULES: OnceLock<LangRules> = OnceLock::new();
    RULES.get_or_init(|| LangRules {
        js_fn: Regex::new(r"^\s*(?:export\s+)?(?:default\s+)?(?:async\s+)?function\s*\*?\s*([A-Za-z_$][\w$]*)").unwrap(),
        js_class: Regex::new(r"^\s*(?:export\s+)?(?:default\s+)?(?:abstract\s+)?class\s+([A-Za-z_$][\w$]*)").unwrap(),
        js_iface: Regex::new(r"^\s*(?:export\s+)?interface\s+([A-Za-z_$][\w$]*)").unwrap(),
        js_type: Regex::new(r"^\s*(?:export\s+)?type\s+([A-Za-z_$][\w$]*)\s*(?:<[^=]*>)?\s*=").unwrap(),
        js_enum: Regex::new(r"^\s*(?:export\s+)?(?:const\s+)?enum\s+([A-Za-z_$][\w$]*)").unwrap(),
        js_arrow: Regex::new(
            r"^\s*(?:export\s+)?(?:const|let|var)\s+([A-Za-z_$][\w$]*)\s*(?::[^=]+)?=\s*(?:async\s+)?(?:\([^)]*\)|[A-Za-z_$][\w$]*)\s*(?::[^=]+)?=>",
        )
        .unwrap(),
        js_var: Regex::new(r"^\s*(?:export\s+)?(?:const|let|var)\s+([A-Za-z_$][\w$]*)\b").unwrap(),
        js_method: Regex::new(
            r"^\s+(?:(?:public|private|protected|static|async|override|readonly|get|set)\s+)*([A-Za-z_$][\w$]*)\s*(?:<[^>]*>)?\s*\([^)]*\)\s*(?::[^{;]+)?\{",
        )
        .unwrap(),
        rs_fn: Regex::new(
            r#"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:const\s+)?(?:async\s+)?(?:unsafe\s+)?(?:extern\s+"[^"]*"\s+)?fn\s+([A-Za-z_]\w*)"#,
        )
        .unwrap(),
        rs_struct: Regex::new(r"^\s*(?:pub(?:\([^)]*\))?\s+)?struct\s+([A-Za-z_]\w*)").unwrap(),
        rs_enum: Regex::new(r"^\s*(?:pub(?:\([^)]*\))?\s+)?enum\s+([A-Za-z_]\w*)").unwrap(),
        rs_trait: Regex::new(r"^\s*(?:pub(?:\([^)]*\))?\s+)?trait\s+([A-Za-z_]\w*)").unwrap(),
        rs_type: Regex::new(r"^\s*(?:pub(?:\([^)]*\))?\s+)?type\s+([A-Za-z_]\w*)").unwrap(),
        rs_const: Regex::new(r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:const|static)\s+([A-Za-z_]\w*)\s*:").unwrap(),
        rs_mod_block: Regex::new(r"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+([A-Za-z_]\w*)\s*\{").unwrap(),
        rs_macro: Regex::new(r"^\s*macro_rules!\s*([A-Za-z_]\w*)").unwrap(),
        rs_impl: Regex::new(r"^\s*impl(?:<[^>]*>)?\s+(?:[A-Za-z_]\w*(?:::[A-Za-z_]\w*)*(?:<[^>]*>)?\s+for\s+)?([A-Za-z_]\w*)").unwrap(),
        py_def: Regex::new(r"^\s*(?:async\s+)?def\s+([A-Za-z_]\w*)\s*\(").unwrap(),
        py_class: Regex::new(r"^\s*class\s+([A-Za-z_]\w*)").unwrap(),
        py_var: Regex::new(r"^([A-Za-z_]\w*)\s*(?::[^=]+)?=").unwrap(),
        go_method: Regex::new(r"^func\s+\(\s*[A-Za-z_]\w*\s+\*?([A-Za-z_]\w*)\s*\)\s+([A-Za-z_]\w*)\s*(?:\[|\()").unwrap(),
        go_fn: Regex::new(r"^func\s+([A-Za-z_]\w*)\s*(?:\[|\()").unwrap(),
        go_type: Regex::new(r"^type\s+([A-Za-z_]\w*)\s+(struct|interface|\S+)").unwrap(),
        go_var: Regex::new(r"^(?:const|var)\s+([A-Za-z_]\w*)\b").unwrap(),
        jvm_type: Regex::new(r"^\s*(?:(?:public|private|protected|internal|abstract|final|sealed|open|static|data|enum)\s+)*(?:class|interface|enum|record|object)\s+([A-Za-z_]\w*)").unwrap(),
        jvm_kt_fn: Regex::new(r"^\s*(?:\w+\s+)*fun\s+(?:<[^>]*>\s*)?(?:[\w.]+\.)?([A-Za-z_]\w*)\s*\(").unwrap(),
        jvm_method: Regex::new(r"^\s+(?:(?:public|private|protected|static|final|synchronized|abstract|override|async)\s+)+[\w<>?\[\],\s]+\s+([A-Za-z_]\w*)\s*\([^;]*$").unwrap(),
        c_macro: Regex::new(r"^\s*#\s*define\s+([A-Za-z_]\w*)").unwrap(),
        c_struct: Regex::new(r"^\s*(?:typedef\s+)?(?:struct|class|enum)\s+([A-Za-z_]\w*)\s*\{?").unwrap(),
        c_fn: Regex::new(r"^(?:(?:static|inline|extern|virtual|constexpr)\s+)*[A-Za-z_][\w:<>\s*&]*?\b([A-Za-z_]\w*)\s*\([^;]*$").unwrap(),
        rb_def: Regex::new(r"^\s*def\s+(?:self\.)?([A-Za-z_]\w*[?!]?)").unwrap(),
        rb_class: Regex::new(r"^\s*(?:class|module)\s+([A-Z]\w*(?:::\w+)*)").unwrap(),
        php_fn: Regex::new(r"^\s*(?:(?:public|private|protected|static|final|abstract)\s+)*function\s+([A-Za-z_]\w*)").unwrap(),
        php_class: Regex::new(r"^\s*(?:(?:final|abstract)\s+)*(?:class|interface|trait)\s+([A-Za-z_]\w*)").unwrap(),
        sh_fn: Regex::new(r"^\s*(?:function\s+)?([A-Za-z_][\w-]*)\s*\(\)\s*\{?").unwrap(),
        css_var: Regex::new(r"^\s*(--[A-Za-z_][\w-]*)\s*:").unwrap(),
        css_sel: Regex::new(r"^([.#][A-Za-z_][\w-]*)\s*[,{]").unwrap(),
        md_head: Regex::new(r"^(#{1,6})\s+(.+?)\s*#*\s*$").unwrap(),
    })
}

fn is_control_kw(name: &str) -> bool {
    matches!(
        name,
        "if" | "for" | "while" | "switch" | "catch" | "return" | "else" | "do" | "try" | "with" | "function" | "new" | "sizeof" | "typeof" | "await" | "match"
    )
}

fn push_def(
    out: &mut Vec<IndexedDef>,
    line_num: usize,
    line: &str,
    m: regex::Match<'_>,
    kind: &'static str,
    container: Option<String>,
    exported: bool,
) {
    let name = m.as_str().to_string();
    if name.is_empty() || is_control_kw(&name) {
        return;
    }
    let col = utf16_col(line, m.start());
    let len = utf16_len(&name);
    out.push(IndexedDef {
        name,
        kind,
        line: line_num,
        col,
        len,
        container,
        exported,
        preview: trunc_chars(line, MAX_PREVIEW_CHARS),
    });
}

fn extract_definitions(lang: LangFamily, text: &str) -> Vec<IndexedDef> {
    let r = rules();
    let mut out = Vec::new();
    let mut in_block_comment = false;
    let mut py_triple: Option<&'static str> = None;
    let mut fence = false;
    let mut depth: i32 = 0;
    // (enter_depth_after_open_brace, container_name)
    let mut brace_scopes: Vec<(i32, String)> = Vec::new();
    // (indent_spaces, class_name) for Python
    let mut py_scopes: Vec<(usize, String)> = Vec::new();

    for (idx, line) in text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).enumerate() {
        let ln = idx + 1;
        let trimmed = line.trim();

        if lang == LangFamily::Markup {
            if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
                fence = !fence;
                continue;
            }
            if !fence {
                if let Some(c) = r.md_head.captures(line) {
                    if let Some(m) = c.get(2) {
                        push_def(&mut out, ln, line, m, "heading", None, true);
                    }
                }
            }
            continue;
        }

        if lang == LangFamily::Python {
            if let Some(delim) = py_triple {
                if line.contains(delim) {
                    py_triple = None;
                }
                continue;
            }
            if trimmed.starts_with("\"\"\"") {
                if trimmed[3..].find("\"\"\"").is_none() {
                    py_triple = Some("\"\"\"");
                }
                continue;
            }
            if trimmed.starts_with("'''") {
                if trimmed[3..].find("'''").is_none() {
                    py_triple = Some("'''");
                }
                continue;
            }
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let indent = line.chars().take_while(|c| *c == ' ' || *c == '\t').map(|c| if c == '\t' { 4 } else { 1 }).sum::<usize>();
            while let Some((top_ind, _)) = py_scopes.last() {
                if indent <= *top_ind {
                    py_scopes.pop();
                } else {
                    break;
                }
            }
            let container = py_scopes.last().map(|(_, n)| n.clone());
            if let Some(c) = r.py_class.captures(line) {
                if let Some(m) = c.get(1) {
                    let cname = m.as_str().to_string();
                    let exp = !cname.starts_with('_');
                    push_def(&mut out, ln, line, m, "class", container, exp);
                    py_scopes.push((indent, cname));
                }
            } else if let Some(c) = r.py_def.captures(line) {
                if let Some(m) = c.get(1) {
                    let kind = if container.is_some() { "method" } else { "function" };
                    let exp = !m.as_str().starts_with('_');
                    push_def(&mut out, ln, line, m, kind, container, exp);
                }
            } else if indent == 0 {
                if let Some(c) = r.py_var.captures(line) {
                    if let Some(m) = c.get(1) {
                        let exp = !m.as_str().starts_with('_');
                        push_def(&mut out, ln, line, m, "const", None, exp);
                    }
                }
            }
            continue;
        }

        // Block comments for C/JS/Rust/Go/CSS/etc.
        if in_block_comment {
            if let Some(end) = line.find("*/") {
                in_block_comment = false;
                depth += brace_delta(&line[end + 2..]);
                while brace_scopes.last().is_some_and(|(d, _)| depth < *d) {
                    brace_scopes.pop();
                }
            }
            continue;
        }
        if trimmed.starts_with("/*") && !trimmed.contains("*/") {
            in_block_comment = true;
            continue;
        }

        let delta = brace_delta(line);
        if trimmed.is_empty()
            || trimmed.starts_with("//")
            || (trimmed.starts_with('#') && lang != LangFamily::CStyle && lang != LangFamily::Ruby && lang != LangFamily::Shell && lang != LangFamily::Php && lang != LangFamily::Css)
            || trimmed.starts_with('*')
        {
            depth += delta;
            while brace_scopes.last().is_some_and(|(d, _)| depth < *d) {
                brace_scopes.pop();
            }
            continue;
        }

        let pre_depth = depth;
        while brace_scopes.last().is_some_and(|(d, _)| pre_depth < *d) {
            brace_scopes.pop();
        }
        let container = brace_scopes.last().map(|(_, n)| n.clone());

        if !is_import_line(lang, trimmed) {
            match lang {
                LangFamily::JsTs => {
                    let exported = trimmed.starts_with("export ");
                    if let Some(c) = r.js_fn.captures(line) {
                        if let Some(m) = c.get(1) {
                            push_def(&mut out, ln, line, m, "function", container.clone(), exported);
                        }
                    } else if let Some(c) = r.js_class.captures(line) {
                        if let Some(m) = c.get(1) {
                            let cname = m.as_str().to_string();
                            push_def(&mut out, ln, line, m, "class", container.clone(), exported);
                            if delta > 0 {
                                brace_scopes.push((pre_depth + 1, cname));
                            }
                        }
                    } else if let Some(c) = r.js_iface.captures(line) {
                        if let Some(m) = c.get(1) {
                            let cname = m.as_str().to_string();
                            push_def(&mut out, ln, line, m, "interface", container.clone(), exported);
                            if delta > 0 {
                                brace_scopes.push((pre_depth + 1, cname));
                            }
                        }
                    } else if let Some(c) = r.js_type.captures(line) {
                        if let Some(m) = c.get(1) {
                            push_def(&mut out, ln, line, m, "type", container.clone(), exported);
                        }
                    } else if let Some(c) = r.js_enum.captures(line) {
                        if let Some(m) = c.get(1) {
                            push_def(&mut out, ln, line, m, "enum", container.clone(), exported);
                        }
                    } else if let Some(c) = r.js_arrow.captures(line) {
                        if pre_depth == 0 || exported {
                            if let Some(m) = c.get(1) {
                                push_def(&mut out, ln, line, m, "function", container.clone(), exported);
                            }
                        }
                    } else if let Some(c) = r.js_var.captures(line) {
                        if pre_depth == 0 || exported {
                            if let Some(m) = c.get(1) {
                                push_def(&mut out, ln, line, m, "const", container.clone(), exported);
                            }
                        }
                    } else if container.is_some() {
                        if let Some(c) = r.js_method.captures(line) {
                            if let Some(m) = c.get(1) {
                                push_def(&mut out, ln, line, m, "method", container.clone(), exported);
                            }
                        }
                    }
                }
                LangFamily::Rust => {
                    let exported = trimmed.starts_with("pub ") || trimmed.starts_with("pub(");
                    if let Some(c) = r.rs_impl.captures(line) {
                        if let Some(m) = c.get(1) {
                            if delta > 0 {
                                brace_scopes.push((pre_depth + 1, m.as_str().to_string()));
                            }
                        }
                    } else if let Some(c) = r.rs_fn.captures(line) {
                        if let Some(m) = c.get(1) {
                            let kind = if container.is_some() { "method" } else { "function" };
                            push_def(&mut out, ln, line, m, kind, container.clone(), exported);
                        }
                    } else if let Some(c) = r.rs_struct.captures(line) {
                        if let Some(m) = c.get(1) {
                            push_def(&mut out, ln, line, m, "struct", container.clone(), exported);
                        }
                    } else if let Some(c) = r.rs_enum.captures(line) {
                        if let Some(m) = c.get(1) {
                            push_def(&mut out, ln, line, m, "enum", container.clone(), exported);
                        }
                    } else if let Some(c) = r.rs_trait.captures(line) {
                        if let Some(m) = c.get(1) {
                            let cname = m.as_str().to_string();
                            push_def(&mut out, ln, line, m, "interface", container.clone(), exported);
                            if delta > 0 {
                                brace_scopes.push((pre_depth + 1, cname));
                            }
                        }
                    } else if let Some(c) = r.rs_type.captures(line) {
                        if let Some(m) = c.get(1) {
                            push_def(&mut out, ln, line, m, "type", container.clone(), exported);
                        }
                    } else if let Some(c) = r.rs_const.captures(line) {
                        if pre_depth == 0 || container.is_some() || exported {
                            if let Some(m) = c.get(1) {
                                push_def(&mut out, ln, line, m, "const", container.clone(), exported);
                            }
                        }
                    } else if let Some(c) = r.rs_mod_block.captures(line) {
                        if let Some(m) = c.get(1) {
                            let mname = m.as_str().to_string();
                            push_def(&mut out, ln, line, m, "module", container.clone(), exported);
                            if delta > 0 {
                                brace_scopes.push((pre_depth + 1, mname));
                            }
                        }
                    } else if let Some(c) = r.rs_macro.captures(line) {
                        if let Some(m) = c.get(1) {
                            push_def(&mut out, ln, line, m, "function", container.clone(), true);
                        }
                    }
                }
                LangFamily::Go => {
                    if let Some(c) = r.go_method.captures(line) {
                        if let (Some(recv), Some(m)) = (c.get(1), c.get(2)) {
                            let exp = m.as_str().chars().next().is_some_and(|ch| ch.is_ascii_uppercase());
                            push_def(&mut out, ln, line, m, "method", Some(recv.as_str().to_string()), exp);
                        }
                    } else if let Some(c) = r.go_fn.captures(line) {
                        if let Some(m) = c.get(1) {
                            let exp = m.as_str().chars().next().is_some_and(|ch| ch.is_ascii_uppercase());
                            push_def(&mut out, ln, line, m, "function", None, exp);
                        }
                    } else if let Some(c) = r.go_type.captures(line) {
                        if let (Some(m), Some(kw)) = (c.get(1), c.get(2)) {
                            let kind = match kw.as_str() {
                                "struct" => "struct",
                                "interface" => "interface",
                                _ => "type",
                            };
                            let exp = m.as_str().chars().next().is_some_and(|ch| ch.is_ascii_uppercase());
                            push_def(&mut out, ln, line, m, kind, None, exp);
                        }
                    } else if let Some(c) = r.go_var.captures(line) {
                        if pre_depth == 0 {
                            if let Some(m) = c.get(1) {
                                let exp = m.as_str().chars().next().is_some_and(|ch| ch.is_ascii_uppercase());
                                push_def(&mut out, ln, line, m, "const", None, exp);
                            }
                        }
                    }
                }
                LangFamily::Jvm => {
                    let exported = trimmed.starts_with("public ") || trimmed.starts_with("open ") || !trimmed.starts_with("private ");
                    if let Some(c) = r.jvm_type.captures(line) {
                        if let Some(m) = c.get(1) {
                            let cname = m.as_str().to_string();
                            let kind = if trimmed.contains("interface ") { "interface" } else if trimmed.contains("enum ") { "enum" } else { "class" };
                            push_def(&mut out, ln, line, m, kind, container.clone(), exported);
                            if delta > 0 {
                                brace_scopes.push((pre_depth + 1, cname));
                            }
                        }
                    } else if let Some(c) = r.jvm_kt_fn.captures(line) {
                        if let Some(m) = c.get(1) {
                            let kind = if container.is_some() { "method" } else { "function" };
                            push_def(&mut out, ln, line, m, kind, container.clone(), exported);
                        }
                    } else if let Some(c) = r.jvm_method.captures(line) {
                        if let Some(m) = c.get(1) {
                            push_def(&mut out, ln, line, m, "method", container.clone(), exported);
                        }
                    }
                }
                LangFamily::CStyle => {
                    if let Some(c) = r.c_macro.captures(line) {
                        if let Some(m) = c.get(1) {
                            push_def(&mut out, ln, line, m, "const", None, true);
                        }
                    } else if let Some(c) = r.c_struct.captures(line) {
                        if let Some(m) = c.get(1) {
                            let cname = m.as_str().to_string();
                            let kind = if trimmed.contains("enum ") { "enum" } else if trimmed.contains("class ") { "class" } else { "struct" };
                            push_def(&mut out, ln, line, m, kind, container.clone(), true);
                            if delta > 0 {
                                brace_scopes.push((pre_depth + 1, cname));
                            }
                        }
                    } else if pre_depth == 0 || container.is_some() {
                        if let Some(c) = r.c_fn.captures(line) {
                            if let Some(m) = c.get(1) {
                                let kind = if container.is_some() { "method" } else { "function" };
                                push_def(&mut out, ln, line, m, kind, container.clone(), !trimmed.starts_with("static "));
                            }
                        }
                    }
                }
                LangFamily::Ruby => {
                    if let Some(c) = r.rb_class.captures(line) {
                        if let Some(m) = c.get(1) {
                            push_def(&mut out, ln, line, m, "class", None, true);
                        }
                    } else if let Some(c) = r.rb_def.captures(line) {
                        if let Some(m) = c.get(1) {
                            push_def(&mut out, ln, line, m, "function", None, true);
                        }
                    }
                }
                LangFamily::Php => {
                    if let Some(c) = r.php_class.captures(line) {
                        if let Some(m) = c.get(1) {
                            let cname = m.as_str().to_string();
                            push_def(&mut out, ln, line, m, "class", container.clone(), true);
                            if delta > 0 {
                                brace_scopes.push((pre_depth + 1, cname));
                            }
                        }
                    } else if let Some(c) = r.php_fn.captures(line) {
                        if let Some(m) = c.get(1) {
                            let kind = if container.is_some() { "method" } else { "function" };
                            push_def(&mut out, ln, line, m, kind, container.clone(), !trimmed.starts_with("private "));
                        }
                    }
                }
                LangFamily::Shell => {
                    if let Some(c) = r.sh_fn.captures(line) {
                        if let Some(m) = c.get(1) {
                            push_def(&mut out, ln, line, m, "function", None, true);
                        }
                    }
                }
                LangFamily::Css => {
                    if let Some(c) = r.css_var.captures(line) {
                        if let Some(m) = c.get(1) {
                            push_def(&mut out, ln, line, m, "const", None, true);
                        }
                    } else if let Some(c) = r.css_sel.captures(line) {
                        if let Some(m) = c.get(1) {
                            push_def(&mut out, ln, line, m, "selector", None, true);
                        }
                    }
                }
                LangFamily::Python | LangFamily::Markup | LangFamily::Other => {}
            }
        }

        depth += delta;
        while brace_scopes.last().is_some_and(|(d, _)| depth < *d) {
            brace_scopes.pop();
        }
    }
    out
}

/// Record `(byte_offset, depth_after)` for every `{` and `}` on a single line outside strings and `//` comments.
fn line_brace_trace(line: &str, base: i32) -> Vec<(usize, i32)> {
    let bytes = line.as_bytes();
    let mut i = 0usize;
    let mut cur = base;
    let mut quote: Option<u8> = None;
    let mut trace = Vec::new();
    while i < bytes.len() {
        let b = bytes[i];
        if let Some(q) = quote {
            if b == b'\\' {
                i += 2;
                continue;
            }
            if b == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        if b == b'/' && bytes.get(i + 1) == Some(&b'/') {
            break;
        }
        match b {
            b'"' | b'\'' | b'`' => quote = Some(b),
            b'{' => {
                cur += 1;
                trace.push((i, cur));
            }
            b'}' => {
                cur -= 1;
                trace.push((i, cur));
            }
            _ => {}
        }
        i += 1;
    }
    trace
}

fn brace_depth_at(trace: &[(usize, i32)], base: i32, byte_pos: usize) -> i32 {
    let mut d = base;
    for &(off, depth) in trace {
        if off <= byte_pos {
            d = depth;
        } else {
            break;
        }
    }
    d
}

fn brace_min_depth_between(trace: &[(usize, i32)], start_depth: i32, from_byte: usize, to_byte: usize) -> i32 {
    let mut min_d = start_depth;
    for &(off, depth) in trace {
        if off > from_byte && off <= to_byte {
            min_d = min_d.min(depth);
        }
    }
    min_d
}

fn utf16_col_to_byte(line: &str, target_col: usize) -> usize {
    let mut acc = 0usize;
    for (idx, ch) in line.char_indices() {
        if acc >= target_col {
            return idx;
        }
        acc += ch.len_utf16();
    }
    line.len()
}

fn contains_whole_word(text: &str, word: &str) -> bool {
    let mut from = 0usize;
    while let Some(pos) = text[from..].find(word) {
        let s = from + pos;
        let e = s + word.len();
        let b_ok = !text[..s].chars().next_back().is_some_and(is_word_char);
        let a_ok = !text[e..].chars().next().is_some_and(is_word_char);
        if b_ok && a_ok {
            return true;
        }
        from = s + word.chars().next().map_or(1, char::len_utf8);
    }
    false
}

/// Find matching `symbol` identifier spans declared in a pattern LHS (simple ident, `{ a, b: sym }`, `[a, sym]`, `(a, mut sym)`).
fn collect_pattern_defs(
    line: &str,
    ln: usize,
    seg_start: usize,
    seg_end: usize,
    symbol: &str,
    _lang: LangFamily,
    out: &mut Vec<(usize, usize, IndexedDef)>,
) {
    if seg_start >= seg_end || seg_end > line.len() {
        return;
    }
    let slice = &line[seg_start..seg_end];
    if !contains_whole_word(slice, symbol) {
        return;
    }
    let bytes = line.as_bytes();
    let mut i = seg_start;
    let mut quote: Option<u8> = None;
    let mut obj_depth = 0usize;
    let mut seq_depth = 0usize;
    let mut skip_default_depth: Option<usize> = None;
    let mut expect_rust_type = false;

    while i < seg_end {
        let b = bytes[i];
        if let Some(q) = quote {
            if b == b'\\' {
                i += 2;
                continue;
            }
            if b == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        if matches!(b, b'"' | b'\'' | b'`') {
            quote = Some(b);
            i += 1;
            continue;
        }
        let cur_depth = obj_depth + seq_depth;
        if let Some(sd) = skip_default_depth {
            match b {
                b'{' | b'[' | b'(' => seq_depth += 1,
                b'}' | b']' | b')' => {
                    if cur_depth <= sd {
                        skip_default_depth = None;
                        if b == b'{' || b == b'}' {
                            obj_depth = obj_depth.saturating_sub(1);
                        } else {
                            seq_depth = seq_depth.saturating_sub(1);
                        }
                    } else {
                        seq_depth = seq_depth.saturating_sub(1);
                    }
                }
                b',' if cur_depth == sd => skip_default_depth = None,
                b';' => break,
                _ => {}
            }
            i += 1;
            continue;
        }
        if expect_rust_type {
            match b {
                b'=' => {
                    expect_rust_type = false;
                    skip_default_depth = Some(cur_depth);
                }
                b';' => break,
                _ => {}
            }
            i += 1;
            continue;
        }

        match b {
            b'{' => obj_depth += 1,
            b'}' => obj_depth = obj_depth.saturating_sub(1),
            b'[' | b'(' => seq_depth += 1,
            b']' | b')' => seq_depth = seq_depth.saturating_sub(1),
            b'=' => skip_default_depth = Some(cur_depth),
            b':' => {
                if bytes.get(i + 1) == Some(&b'=') {
                    // Go `:=` terminates the LHS target list
                    break;
                }
                if cur_depth == 0 {
                    // Top-level `: Type` annotation before `=`
                    expect_rust_type = true;
                }
            }
            b';' => break,
            _ if b.is_ascii_alphabetic() || b == b'_' || b == b'$' => {
                let start = i;
                while i < seg_end {
                    let ch = line[i..].chars().next().unwrap();
                    if is_word_char(ch) {
                        i += ch.len_utf8();
                    } else {
                        break;
                    }
                }
                let word = &line[start..i];
                if word == symbol {
                    // Inside `{ origKey: alias }`, skip `origKey` immediately followed by single `:`
                    let rest = line[i..seg_end].trim_start();
                    let is_prop_key = obj_depth > 0 && rest.starts_with(':') && !rest.starts_with("::");
                    if !is_prop_key && !matches!(word, "mut" | "ref" | "box" | "const" | "let" | "var" | "type") {
                        out.push((
                            start,
                            i,
                            IndexedDef {
                                name: word.to_string(),
                                kind: "const",
                                line: ln,
                                col: utf16_col(line, start),
                                len: utf16_len(word),
                                container: None,
                                exported: false,
                                preview: trunc_chars(line, MAX_PREVIEW_CHARS),
                            },
                        ));
                    }
                }
                continue;
            }
            _ => {}
        }
        i += 1;
    }
}

/// Extract parameter declarations inside a `(params)` snippet matching `symbol`.
fn collect_param_defs(
    line: &str,
    ln: usize,
    params_start: usize,
    params_end: usize,
    lang: LangFamily,
    symbol: &str,
    out: &mut Vec<(usize, usize, IndexedDef)>,
) {
    if params_start >= params_end || params_end > line.len() {
        return;
    }
    let seg = &line[params_start..params_end];
    if !contains_whole_word(seg, symbol) {
        return;
    }
    // Split top-level `,` parameters while ignoring strings and nested brackets.
    let bytes = line.as_bytes();
    let mut part_start = params_start;
    let mut i = params_start;
    let mut paren = 0i32;
    let mut bracket = 0i32;
    let mut brace = 0i32;
    let mut angle = 0i32;
    let mut quote: Option<u8> = None;
    let mut slices = Vec::new();
    while i < params_end {
        let b = bytes[i];
        if let Some(q) = quote {
            if b == b'\\' {
                i += 2;
                continue;
            }
            if b == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        match b {
            b'"' | b'\'' | b'`' => quote = Some(b),
            b'(' => paren += 1,
            b')' => paren = (paren - 1).max(0),
            b'[' => bracket += 1,
            b']' => bracket = (bracket - 1).max(0),
            b'{' => brace += 1,
            b'}' => brace = (brace - 1).max(0),
            b'<' => angle += 1,
            b'>' => angle = (angle - 1).max(0),
            b',' if paren == 0 && bracket == 0 && brace == 0 && angle == 0 => {
                slices.push((part_start, i));
                part_start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    slices.push((part_start, params_end));

    for (ps, pe) in slices {
        let p = &line[ps..pe];
        if !contains_whole_word(p, symbol) {
            continue;
        }
        let mut s = ps;
        while s < pe && bytes[s].is_ascii_whitespace() {
            s += 1;
        }
        if s >= pe {
            continue;
        }
        // Strip leading destructuring spread / mut / ref / modifiers
        loop {
            let cur = &line[s..pe];
            if cur.starts_with("...") {
                s += 3;
            } else if let Some(kw) = ["mut ", "ref ", "public ", "private ", "protected ", "readonly ", "val ", "var "]
                .iter()
                .find(|k| cur.starts_with(*k))
            {
                s += kw.len();
            } else {
                break;
            }
            while s < pe && bytes[s].is_ascii_whitespace() {
                s += 1;
            }
        }
        if s >= pe {
            continue;
        }
        if matches!(bytes[s], b'{' | b'[' | b'(') {
            collect_pattern_defs(line, ln, s, pe, symbol, lang, out);
            continue;
        }
        if matches!(lang, LangFamily::JsTs | LangFamily::Rust | LangFamily::Python)
            || (lang == LangFamily::Jvm && line[s..pe].contains(':'))
        {
            // Name comes before `:` or `=`
            let head_end = line[s..pe]
                .find([':', '='])
                .map_or(pe, |rel| s + rel);
            let head = line[s..head_end].trim_end();
            if head == symbol {
                out.push((
                    s,
                    s + head.len(),
                    IndexedDef {
                        name: symbol.to_string(),
                        kind: "const",
                        line: ln,
                        col: utf16_col(line, s),
                        len: utf16_len(symbol),
                        container: None,
                        exported: false,
                        preview: trunc_chars(line, MAX_PREVIEW_CHARS),
                    },
                ));
            }
        } else if lang == LangFamily::Go {
            let trimmed = line[s..pe].trim();
            if trimmed == symbol && pe < params_end {
                // `func(a, b int)` shorthand where `a` precedes a comma
                out.push((
                    s,
                    s + trimmed.len(),
                    IndexedDef {
                        name: symbol.to_string(),
                        kind: "const",
                        line: ln,
                        col: utf16_col(line, s),
                        len: utf16_len(symbol),
                        container: None,
                        exported: false,
                        preview: trunc_chars(line, MAX_PREVIEW_CHARS),
                    },
                ));
            } else if let Some((first, rest)) = trimmed.split_once(char::is_whitespace) {
                if first == symbol && !rest.trim().is_empty() {
                    out.push((
                        s,
                        s + first.len(),
                        IndexedDef {
                            name: symbol.to_string(),
                            kind: "const",
                            line: ln,
                            col: utf16_col(line, s),
                            len: utf16_len(symbol),
                            container: None,
                            exported: false,
                            preview: trunc_chars(line, MAX_PREVIEW_CHARS),
                        },
                    ));
                }
            }
        }
    }
}

/// Resolve lexically visible local variable and parameter declarations for `symbol` at `(target_line, target_col)`.
/// Returned items are ordered innermost / nearest-preceding first as `(scope_depth, IndexedDef)`.
fn resolve_visible_local_bindings(
    lang: LangFamily,
    text: &str,
    symbol: &str,
    target_line: usize,
    target_col: usize,
) -> Vec<(usize, IndexedDef)> {
    if symbol.is_empty() || target_line == 0 {
        return Vec::new();
    }
    let esc = regex::escape(symbol);

    if lang == LangFamily::Python {
        let assign_re = Regex::new(&format!(r"^\s*({esc})\s*(?::[^=]+)?=")).ok();
        let def_re = Regex::new(r"^\s*(?:async\s+)?def\s+[A-Za-z_]\w*\s*\(([^)]*)\)").ok();
        let as_re = Regex::new(&format!(r"\bas\s+({esc})\b")).ok();
        let for_re = Regex::new(&format!(r"^\s*(?:async\s+)?for\s+(?:[A-Za-z_][\w,\s]*\b)?({esc})\b.*\bin\b")).ok();
        let mut active: Vec<(usize, IndexedDef)> = Vec::new();
        let mut py_triple: Option<&'static str> = None;

        for (idx, line) in line_spans(text).into_iter().enumerate() {
            let ln = idx + 1;
            if ln > target_line {
                break;
            }
            let trimmed = line.trim();
            if let Some(delim) = py_triple {
                if line.contains(delim) {
                    py_triple = None;
                }
                continue;
            }
            if trimmed.starts_with("\"\"\"") {
                if trimmed[3..].find("\"\"\"").is_none() {
                    py_triple = Some("\"\"\"");
                }
                continue;
            }
            if trimmed.starts_with("'''") {
                if trimmed[3..].find("'''").is_none() {
                    py_triple = Some("'''");
                }
                continue;
            }
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let indent = line
                .chars()
                .take_while(|c| *c == ' ' || *c == '\t')
                .map(|c| if c == '\t' { 4 } else { 1 })
                .sum::<usize>();
            let is_header = trimmed.starts_with("def ") || trimmed.starts_with("async def ") || trimmed.starts_with("class ");
            active.retain(|(ind, _)| if is_header { *ind < indent } else { *ind <= indent });

            let col_limit = if ln == target_line { target_col } else { usize::MAX };
            if let Some(c) = def_re.as_ref().and_then(|r| r.captures(line)) {
                if let Some(m) = c.get(1) {
                    let mut pdefs = Vec::new();
                    collect_param_defs(line, ln, m.start(), m.end(), lang, symbol, &mut pdefs);
                    for (_, _, d) in pdefs {
                        if d.col <= col_limit {
                            active.push((indent + 1, d));
                        }
                    }
                }
            }
            if ln < target_line || col_limit > 0 {
                if let Some(c) = assign_re.as_ref().and_then(|r| r.captures(line)) {
                    if let Some(m) = c.get(1) {
                        if !line[m.end()..].trim_start().starts_with("==") {
                            let col = utf16_col(line, m.start());
                            if col <= col_limit {
                                active.push((
                                    indent,
                                    IndexedDef {
                                        name: symbol.to_string(),
                                        kind: "const",
                                        line: ln,
                                        col,
                                        len: utf16_len(symbol),
                                        container: None,
                                        exported: false,
                                        preview: trunc_chars(line, MAX_PREVIEW_CHARS),
                                    },
                                ));
                            }
                        }
                    }
                }
                for re in [for_re.as_ref(), as_re.as_ref()].into_iter().flatten() {
                    if let Some(c) = re.captures(line) {
                        if let Some(m) = c.get(1) {
                            let col = utf16_col(line, m.start());
                            if col <= col_limit {
                                active.push((
                                    indent + 1,
                                    IndexedDef {
                                        name: symbol.to_string(),
                                        kind: "const",
                                        line: ln,
                                        col,
                                        len: utf16_len(symbol),
                                        container: None,
                                        exported: false,
                                        preview: trunc_chars(line, MAX_PREVIEW_CHARS),
                                    },
                                ));
                            }
                        }
                    }
                }
            }
        }
        active.sort_by(|a, b| (b.0, b.1.line, b.1.col).cmp(&(a.0, a.1.line, a.1.col)));
        return active;
    }

    if !matches!(lang, LangFamily::JsTs | LangFamily::Rust | LangFamily::Go | LangFamily::Jvm | LangFamily::CStyle | LangFamily::Php) {
        return Vec::new();
    }

    let r = rules();
    let single_arrow_re = Regex::new(&format!(r"\b({esc})\s*=>")).ok();
    let catch_re = Regex::new(&format!(r"\bcatch\s*\(\s*({esc})\b")).ok();
    let rs_iflet_re = Regex::new(r"\b(?:if|while)\s+let\s+[^=]+=\s*[^;{]+\{").ok();
    let rs_for_re = Regex::new(r"\bfor\s+(.+?)\s+in\b").ok();
    let js_decl_re = Regex::new(r"\b(?:const|let|var)\s+").ok();
    let rs_let_re = Regex::new(r"\blet\s+").ok();
    let go_short_re = Regex::new(r"([A-Za-z_]\w*(?:\s*,\s*[A-Za-z_]\w*)*)\s*:=").ok();
    let fn_paren_re = Regex::new(r"(?:function\s*(?:\*?\s*[A-Za-z_$][\w$]*)?|\bfn\s+[A-Za-z_]\w*(?:<[^>]*>)?|\bfunc\s*(?:\([^)]*\))?\s*(?:[A-Za-z_]\w*)?(?:\[[^\]]*\])?)\s*\(([^)]*)\)").ok();
    let js_arrow_paren_re = Regex::new(r"\(([^)]*)\)\s*(?::[^=>{;]+)?=>").ok();
    let js_method_paren_re = Regex::new(r"^\s+(?:(?:public|private|protected|static|async|override|readonly|get|set)\s+)*([A-Za-z_$][\w$]*)\s*(?:<[^>]*>)?\s*\(([^)]*)\)\s*(?::[^{;]+)?\{").ok();

    struct ActiveLocal {
        scope: i32,
        def: IndexedDef,
    }
    let mut active: Vec<ActiveLocal> = Vec::new();
    let mut in_block_comment = false;
    let mut depth = 0i32;

    for (idx, line) in line_spans(text).into_iter().enumerate() {
        let ln = idx + 1;
        if ln > target_line {
            break;
        }
        let trimmed = line.trim();
        if in_block_comment {
            if let Some(end) = line.find("*/") {
                in_block_comment = false;
                let base = depth;
                let trace = line_brace_trace(&line[end + 2..], base);
                let lim = if ln == target_line {
                    utf16_col_to_byte(line, target_col).saturating_sub(end + 2)
                } else {
                    line[end + 2..].len()
                };
                let min_d = brace_min_depth_between(&trace, base, 0, lim);
                active.retain(|b| b.scope <= min_d);
                depth = brace_depth_at(&trace, base, lim);
            }
            continue;
        }
        if trimmed.starts_with("/*") && !trimmed.contains("*/") {
            in_block_comment = true;
            continue;
        }
        if trimmed.is_empty() || trimmed.starts_with("//") || trimmed.starts_with('*') || is_import_line(lang, trimmed) {
            continue;
        }

        let base = depth;
        let trace = line_brace_trace(line, base);
        let line_limit_byte = if ln == target_line {
            utf16_col_to_byte(line, target_col)
        } else {
            line.len()
        };

        // Keep existing bindings only if brace depth has not dropped below their scope before the end/cursor.
        let mut line_candidates: Vec<(i32, usize, IndexedDef)> = Vec::new();
        if contains_whole_word(line, symbol) {
            let mut found_spans: Vec<(usize, usize, IndexedDef)> = Vec::new();

            // Nested function/fn declarations inside blocks
            let nested_fn_match = match lang {
                LangFamily::JsTs => r.js_fn.captures(line).and_then(|c| c.get(1)),
                LangFamily::Rust => r.rs_fn.captures(line).and_then(|c| c.get(1)),
                _ => None,
            };
            if let Some(m) = nested_fn_match {
                let d_at = brace_depth_at(&trace, base, m.start());
                if d_at > 0 && m.as_str() == symbol {
                    line_candidates.push((
                        d_at,
                        m.end(),
                        IndexedDef {
                            name: symbol.to_string(),
                            kind: "function",
                            line: ln,
                            col: utf16_col(line, m.start()),
                            len: utf16_len(symbol),
                            container: None,
                            exported: false,
                            preview: trunc_chars(line, MAX_PREVIEW_CHARS),
                        },
                    ));
                }
            }

            // Parameters of function / fn / func / method / arrow definitions
            if let Some(c) = fn_paren_re.as_ref().and_then(|re| re.captures(line)) {
                if let Some(m) = c.get(1) {
                    collect_param_defs(line, ln, m.start(), m.end(), lang, symbol, &mut found_spans);
                }
            }
            if lang == LangFamily::JsTs {
                if let Some(c) = js_arrow_paren_re.as_ref().and_then(|re| re.captures(line)) {
                    if let Some(m) = c.get(1) {
                        collect_param_defs(line, ln, m.start(), m.end(), lang, symbol, &mut found_spans);
                    }
                }
                if let Some(c) = js_method_paren_re.as_ref().and_then(|re| re.captures(line)) {
                    if let Some(m) = c.get(2) {
                        collect_param_defs(line, ln, m.start(), m.end(), lang, symbol, &mut found_spans);
                    }
                }
                if let Some(c) = single_arrow_re.as_ref().and_then(|re| re.captures(line)) {
                    if let Some(m) = c.get(1) {
                        found_spans.push((
                            m.start(),
                            m.end(),
                            IndexedDef {
                                name: symbol.to_string(),
                                kind: "const",
                                line: ln,
                                col: utf16_col(line, m.start()),
                                len: utf16_len(symbol),
                                container: None,
                                exported: false,
                                preview: trunc_chars(line, MAX_PREVIEW_CHARS),
                            },
                        ));
                    }
                }
            }
            for (ps, pe, d) in found_spans.drain(..) {
                // Bind parameters inside the body `{` scope (last unclosed `{` after `pe`), or inline on `target_line` for expression arrows.
                let end_depth = brace_depth_at(&trace, base, line.len());
                let body_brace = if end_depth > base {
                    trace
                        .iter()
                        .rfind(|&&(off, bdepth)| off >= pe && bdepth == end_depth)
                        .or_else(|| trace.iter().find(|&&(off, bdepth)| off >= pe && bdepth > base))
                        .copied()
                } else {
                    trace.iter().rfind(|&&(off, bdepth)| off >= pe && bdepth > base).copied()
                };
                if let Some((off, bdepth)) = body_brace {
                    line_candidates.push((bdepth, off, d));
                } else if ln == target_line && lang == LangFamily::JsTs && line[pe..].contains("=>") {
                    line_candidates.push((brace_depth_at(&trace, base, ps).max(1), pe, d));
                }
            }

            // Catch binding (`catch (err) {`)
            if let Some(c) = catch_re.as_ref().and_then(|re| re.captures(line)) {
                if let Some(m) = c.get(1) {
                    if let Some(&(off, bdepth)) = trace.iter().find(|&&(off, _)| off >= m.end()) {
                        line_candidates.push((
                            bdepth,
                            off,
                            IndexedDef {
                                name: symbol.to_string(),
                                kind: "const",
                                line: ln,
                                col: utf16_col(line, m.start()),
                                len: utf16_len(symbol),
                                container: None,
                                exported: false,
                                preview: trunc_chars(line, MAX_PREVIEW_CHARS),
                            },
                        ));
                    }
                }
            }

            // Variable / loop declarations
            match lang {
                LangFamily::JsTs => {
                    if let Some(re) = js_decl_re.as_ref() {
                        for m in re.find_iter(line) {
                            let is_for_head = line[..m.start()].trim_end().ends_with('(') && line[..m.start()].contains("for");
                            let end_lhs = line[m.end()..].find(['=', ';']).map_or(line.len(), |r| m.end() + r);
                            let mut lhs_defs = Vec::new();
                            collect_pattern_defs(line, ln, m.start(), end_lhs.max(m.end()), symbol, lang, &mut lhs_defs);
                            for (ds, de, d) in lhs_defs {
                                if is_for_head {
                                    if let Some(&(off, bdepth)) = trace.iter().find(|&&(off, _)| off >= de) {
                                        line_candidates.push((bdepth, off, d));
                                        continue;
                                    }
                                }
                                line_candidates.push((brace_depth_at(&trace, base, ds), de, d));
                            }
                        }
                    }
                }
                LangFamily::Rust => {
                    if let Some(re) = rs_let_re.as_ref() {
                        for m in re.find_iter(line) {
                            let is_ctrl = rs_iflet_re.as_ref().is_some_and(|ir| ir.is_match(line));
                            let end_lhs = line[m.end()..].find(['=', ';']).map_or(line.len(), |r| m.end() + r);
                            let mut lhs_defs = Vec::new();
                            collect_pattern_defs(line, ln, m.end(), end_lhs, symbol, lang, &mut lhs_defs);
                            for (ds, de, d) in lhs_defs {
                                if is_ctrl {
                                    if let Some(&(off, bdepth)) = trace.iter().find(|&&(off, _)| off >= de) {
                                        line_candidates.push((bdepth, off, d));
                                        continue;
                                    }
                                }
                                line_candidates.push((brace_depth_at(&trace, base, ds), de, d));
                            }
                        }
                    }
                    if let Some(c) = rs_for_re.as_ref().and_then(|re| re.captures(line)) {
                        if let Some(m) = c.get(1) {
                            let mut lhs_defs = Vec::new();
                            collect_pattern_defs(line, ln, m.start(), m.end(), symbol, lang, &mut lhs_defs);
                            for (_, de, d) in lhs_defs {
                                if let Some(&(off, bdepth)) = trace.iter().find(|&&(off, _)| off >= de) {
                                    line_candidates.push((bdepth, off, d));
                                }
                            }
                        }
                    }
                }
                LangFamily::Go => {
                    if let Some(re) = go_short_re.as_ref() {
                        for c in re.captures_iter(line) {
                            if let Some(m) = c.get(1) {
                                let is_for = line[..m.start()].trim_start().starts_with("for ");
                                let mut lhs_defs = Vec::new();
                                collect_pattern_defs(line, ln, m.start(), m.end(), symbol, lang, &mut lhs_defs);
                                for (ds, de, d) in lhs_defs {
                                    if is_for {
                                        if let Some(&(off, bdepth)) = trace.iter().find(|&&(off, _)| off >= de) {
                                            line_candidates.push((bdepth, off, d));
                                            continue;
                                        }
                                    }
                                    line_candidates.push((brace_depth_at(&trace, base, ds), de, d));
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        // First pop any existing bindings exited before `line_limit_byte`.
        let min_existing = brace_min_depth_between(&trace, base, 0, line_limit_byte);
        active.retain(|b| b.scope <= min_existing);

        // Add candidates declared on this line whose scope remains live at `line_limit_byte`.
        for (bind_scope, anchor_byte, def) in line_candidates {
            if ln == target_line && def.col > target_col {
                continue;
            }
            let min_after = brace_min_depth_between(&trace, bind_scope, anchor_byte, line_limit_byte);
            if min_after >= bind_scope {
                active.push(ActiveLocal { scope: bind_scope, def });
            }
        }

        depth = brace_depth_at(&trace, base, line_limit_byte);
    }

    active.sort_by(|a, b| (b.scope, b.def.line, b.def.col).cmp(&(a.scope, a.def.line, a.def.col)));
    active.into_iter().map(|b| (b.scope.max(0) as usize, b.def)).collect()
}

fn find_enclosing_container(lang: LangFamily, text: &str, target_line: usize) -> Option<String> {
    if lang == LangFamily::Python {
        let mut scopes: Vec<(usize, String)> = Vec::new();
        for (idx, line) in text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).enumerate() {
            let ln = idx + 1;
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                if ln >= target_line {
                    break;
                }
                continue;
            }
            let indent = line.chars().take_while(|c| *c == ' ' || *c == '\t').map(|c| if c == '\t' { 4 } else { 1 }).sum::<usize>();
            while let Some((top, _)) = scopes.last() {
                if indent <= *top {
                    scopes.pop();
                } else {
                    break;
                }
            }
            if ln >= target_line {
                break;
            }
            if let Some(c) = rules().py_class.captures(line) {
                if let Some(m) = c.get(1) {
                    scopes.push((indent, m.as_str().to_string()));
                }
            }
        }
        return scopes.last().map(|(_, n)| n.clone());
    }

    let r = rules();
    let mut depth: i32 = 0;
    let mut scopes: Vec<(i32, String)> = Vec::new();
    for (idx, line) in text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).enumerate() {
        let ln = idx + 1;
        while scopes.last().is_some_and(|(d, _)| depth < *d) {
            scopes.pop();
        }
        if ln == target_line {
            return scopes.last().map(|(_, n)| n.clone());
        }
        let delta = brace_delta(line);
        let pre = depth;
        match lang {
            LangFamily::Rust => {
                if let Some(c) = r.rs_impl.captures(line) {
                    if let Some(m) = c.get(1) {
                        if delta > 0 {
                            scopes.push((pre + 1, m.as_str().to_string()));
                        }
                    }
                } else if let Some(c) = r.rs_trait.captures(line) {
                    if let Some(m) = c.get(1) {
                        if delta > 0 {
                            scopes.push((pre + 1, m.as_str().to_string()));
                        }
                    }
                }
            }
            LangFamily::JsTs => {
                if let Some(c) = r.js_class.captures(line) {
                    if let Some(m) = c.get(1) {
                        if delta > 0 {
                            scopes.push((pre + 1, m.as_str().to_string()));
                        }
                    }
                }
            }
            LangFamily::Jvm => {
                if let Some(c) = r.jvm_type.captures(line) {
                    if let Some(m) = c.get(1) {
                        if delta > 0 {
                            scopes.push((pre + 1, m.as_str().to_string()));
                        }
                    }
                }
            }
            LangFamily::CStyle => {
                if let Some(c) = r.c_struct.captures(line) {
                    if let Some(m) = c.get(1) {
                        if delta > 0 {
                            scopes.push((pre + 1, m.as_str().to_string()));
                        }
                    }
                }
            }
            _ => {}
        }
        depth += delta;
    }
    None
}

fn update_workspace_index(root_dir: &Path) -> Vec<String> {
    update_workspace_index_inner(root_dir, || {})
}

fn update_workspace_index_inner(root_dir: &Path, on_after_snapshot: impl FnOnce()) -> Vec<String> {
    let root_canon = canon_clean(root_dir).unwrap_or_else(|| root_dir.to_path_buf());
    let root_key = workspace_cache_key(&root_canon);
    let ws_lock = workspace_update_lock(&root_key);
    let _ws_guard = ws_lock.lock().unwrap_or_else(|e| e.into_inner());

    // Snapshot indexed files (Arc handles) without holding INDEX_CACHE across filesystem checks or parsing
    let existing: HashMap<String, Arc<IndexedFile>> = {
        let mut guard = INDEX_CACHE.lock().unwrap();
        let (_, cache) = guard.get_or_insert_with(|| (0, HashMap::new()));
        cache
            .get(&root_key)
            .map(|ws| ws.files.clone())
            .unwrap_or_default()
    };
    on_after_snapshot();

    // Open root handle with O_NOFOLLOW traversal before enumerating/reading so a swapped root cannot be followed
    let Some(root_handle) = DirHandle::open_root(&root_canon) else {
        return crate::fsapi::file_list(&root_canon).as_ref().clone();
    };
    let file_list = crate::fsapi::file_list(&root_canon);
    let started = Instant::now();
    let mut live_set = HashSet::with_capacity(file_list.len());
    let mut updated: Vec<(String, Arc<IndexedFile>)> = Vec::new();

    for rel in file_list.iter() {
        if !is_safe_rel(rel) || is_ignored_asset(rel) {
            continue;
        }
        let lang = LangFamily::from_path(rel);
        if lang == LangFamily::Other {
            continue;
        }
        let Some((file, meta)) = root_handle.open_contained(&root_canon, rel) else {
            continue;
        };
        if started.elapsed() > std::time::Duration::from_secs(6) {
            if existing.contains_key(rel) {
                live_set.insert(rel.clone());
            }
            continue;
        }
        live_set.insert(rel.clone());
        let mt = mtime_ms(&meta);
        let sz = meta.len();
        if existing.get(rel).is_some_and(|prev| prev.mtime == mt && prev.size == sz) {
            continue;
        }
        let Some(text) = read_text_from_file(file) else {
            continue;
        };
        let defs = extract_definitions(lang, &text);
        updated.push((rel.clone(), Arc::new(IndexedFile { mtime: mt, size: sz, lang, defs })));
    }

    let mut guard = INDEX_CACHE.lock().unwrap();
    let (seq, cache) = guard.get_or_insert_with(|| (0, HashMap::new()));
    *seq += 1;
    let now_seq = *seq;

    let was_evicted = !cache.contains_key(&root_key);
    if was_evicted && cache.len() >= MAX_CACHED_WORKSPACES {
        if let Some(oldest_k) = cache.iter().min_by_key(|(_, v)| v.last_used).map(|(k, _)| k.clone()) {
            cache.remove(&oldest_k);
        }
    }

    let ws = cache.entry(root_key).or_default();
    ws.last_used = now_seq;

    if was_evicted {
        for (rel, indexed) in existing {
            if live_set.contains(&rel) {
                ws.files.entry(rel).or_insert(indexed);
            }
        }
    }

    let mut changed = !updated.is_empty();
    for (rel, indexed) in updated {
        ws.files.insert(rel, indexed);
    }

    let before_len = ws.files.len();
    ws.files.retain(|k, _| live_set.contains(k));
    if ws.files.len() != before_len {
        changed = true;
    }

    if changed || was_evicted || (ws.by_name.is_empty() && !ws.files.is_empty()) {
        ws.rebuild_lookup();
    }

    file_list.as_ref().clone()
}

fn query_index(root_dir: &Path, symbol: &str) -> Vec<(String, LangFamily, IndexedDef)> {
    let root_canon = canon_clean(root_dir).unwrap_or_else(|| root_dir.to_path_buf());
    let root_key = workspace_cache_key(&root_canon);
    let guard = INDEX_CACHE.lock().unwrap();
    let Some((_, cache)) = guard.as_ref() else {
        return Vec::new();
    };
    let Some(ws) = cache.get(&root_key) else {
        return Vec::new();
    };
    let Some(hits) = ws.by_name.get(symbol) else {
        return Vec::new();
    };
    hits.iter()
        .filter_map(|(rel, idx)| {
            let f = ws.files.get(rel)?;
            let d = f.defs.get(*idx)?;
            Some((rel.clone(), f.lang, d.clone()))
        })
        .collect()
}

// ---------- Path, tsconfig alias & import tracking ----------

fn parse_path_and_line(raw: &str) -> (String, usize, usize) {
    let s = raw.trim().trim_matches(|c| matches!(c, '\'' | '"' | '`' | '<' | '>'));
    // Find last path separator so Windows `C:\foo` drive colon is never mistaken for a line number.
    let last_sep = s.rfind(['/', '\\']);
    let tail_start = last_sep.map_or(0, |i| i + 1);
    let tail = &s[tail_start..];
    let parts: Vec<&str> = tail.split(':').collect();
    if parts.len() >= 3 {
        if let (Ok(line), Ok(col)) = (parts[parts.len() - 2].parse::<usize>(), parts[parts.len() - 1].parse::<usize>()) {
            let prefix_len = s.len() - parts[parts.len() - 2].len() - parts[parts.len() - 1].len() - 2;
            return (s[..prefix_len].to_string(), line.max(1), col.saturating_sub(1));
        }
    }
    if parts.len() >= 2 {
        if let Ok(line) = parts[parts.len() - 1].parse::<usize>() {
            let prefix_len = s.len() - parts[parts.len() - 1].len() - 1;
            return (s[..prefix_len].to_string(), line.max(1), 0);
        }
    }
    (s.to_string(), 1, 0)
}

fn strip_jsonc(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    let mut in_str = false;
    while i < chars.len() {
        let c = chars[i];
        if in_str {
            out.push(c);
            if c == '\\' && i + 1 < chars.len() {
                i += 1;
                out.push(chars[i]);
            } else if c == '"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        if c == '"' {
            in_str = true;
            out.push(c);
            i += 1;
            continue;
        }
        if c == '/' && chars.get(i + 1) == Some(&'/') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if c == '/' && chars.get(i + 1) == Some(&'*') {
            i += 2;
            while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                i += 1;
            }
            i += 2;
            continue;
        }
        out.push(c);
        i += 1;
    }
    // Remove trailing commas before `}` or `]`.
    let re = Regex::new(r",(\s*[}\]])").unwrap();
    re.replace_all(&out, "$1").into_owned()
}

fn load_tsconfig(root_dir: &Path, from_rel: &str) -> Option<CachedTsConfig> {
    let root_canon = canon_clean(root_dir).unwrap_or_else(|| root_dir.to_path_buf());
    let mut dir = if from_rel.is_empty() {
        root_canon.clone()
    } else {
        let p = root_canon.join(from_rel);
        p.parent().unwrap_or(&root_canon).to_path_buf()
    };
    let mut found: Option<PathBuf> = None;
    loop {
        if !dir.starts_with(&root_canon) {
            break;
        }
        for name in ["tsconfig.json", "jsconfig.json"] {
            let cand = dir.join(name);
            if let Some((canon, _)) = canonical_contained_path(&root_canon, &cand) {
                found = Some(canon);
                break;
            }
        }
        if found.is_some() || norm_cmp(&dir.display().to_string()) == norm_cmp(&root_canon.display().to_string()) {
            break;
        }
        if !dir.pop() {
            break;
        }
    }
    let cfg_path = found?;
    let key = norm_cmp(&cfg_path.display().to_string());
    let mt = std::fs::metadata(&cfg_path).ok().map(|m| mtime_ms(&m)).unwrap_or(0);
    {
        let g = TSCONFIG_CACHE.lock().unwrap();
        if let Some(map) = g.as_ref() {
            if let Some(c) = map.get(&key) {
                if c.mtime == mt {
                    return Some(c.clone());
                }
            }
        }
    }
    let raw = read_contained_text_file(&root_canon, &cfg_path)?;
    let val: Value = serde_json::from_str(&strip_jsonc(&raw)).ok()?;
    let co = &val["compilerOptions"];
    let cfg_dir = cfg_path.parent().unwrap_or(&root_canon);
    let base_url = match co["baseUrl"].as_str() {
        Some(b) if !b.is_empty() => cfg_dir.join(b),
        _ => cfg_dir.to_path_buf(),
    };
    let mut paths = Vec::new();
    if let Some(obj) = co["paths"].as_object() {
        for (pat, arr) in obj {
            if let Some(list) = arr.as_array() {
                let targets: Vec<String> = list.iter().filter_map(|v| v.as_str().map(str::to_string)).collect();
                if !targets.is_empty() {
                    paths.push((pat.clone(), targets));
                }
            }
        }
    }
    let parsed = CachedTsConfig { mtime: mt, base_url, paths };
    let mut g = TSCONFIG_CACHE.lock().unwrap();
    let map = g.get_or_insert_with(HashMap::new);
    map.insert(key, parsed.clone());
    Some(parsed)
}

fn probe_js_file(base: &Path) -> Option<PathBuf> {
    if base.is_file() {
        return Some(base.to_path_buf());
    }
    let base_str = base.to_string_lossy();
    // Handle TS ESM imports ending in `.js` / `.jsx` pointing to `.ts` / `.tsx`.
    if let Some(stem) = base_str.strip_suffix(".js") {
        for ext in [".ts", ".tsx", ".mts", ".js", ".jsx"] {
            let cand = PathBuf::from(format!("{stem}{ext}"));
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    for suffix in [
        ".ts", ".tsx", ".mts", ".cts", ".js", ".jsx", ".mjs", ".cjs", ".d.ts", ".json",
        "/index.ts", "/index.tsx", "/index.mts", "/index.js", "/index.jsx",
    ] {
        let cand = PathBuf::from(format!("{base_str}{suffix}"));
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

fn check_contained_file(root_canon: &Path, candidate: &Path) -> Option<String> {
    canonical_contained_path(root_canon, candidate).map(|(_, rel)| rel)
}

fn rust_module_dir(root_dir: &Path, from_rel: &str) -> (PathBuf, bool) {
    let from_dir = if from_rel.is_empty() {
        root_dir.to_path_buf()
    } else {
        root_dir.join(from_rel).parent().unwrap_or(root_dir).to_path_buf()
    };
    let stem = Path::new(from_rel).file_stem().and_then(|s| s.to_str()).unwrap_or("");
    let parent_name = from_dir.file_name().and_then(|s| s.to_str()).unwrap_or("");
    let is_dir_root = stem.is_empty()
        || matches!(stem, "lib" | "main" | "mod")
        || matches!(parent_name, "bin" | "tests" | "examples" | "benches");
    if is_dir_root {
        (from_dir, true)
    } else {
        (from_dir.join(stem), false)
    }
}

fn resolve_import_specifier(root_dir: &Path, from_rel: &str, spec: &str, all_files: &[String]) -> Option<String> {
    let root_canon = canon_clean(root_dir).unwrap_or_else(|| root_dir.to_path_buf());
    let clean = spec.trim().trim_matches(|c| matches!(c, '\'' | '"' | '`' | '<' | '>'));
    if clean.is_empty() {
        return None;
    }
    let from_dir = if from_rel.is_empty() {
        root_canon.clone()
    } else {
        root_canon.join(from_rel).parent().unwrap_or(&root_canon).to_path_buf()
    };

    // Rust `mod foo;`: probe only the source file's submodule directory (`src/a/foo.rs` or `src/a/foo/mod.rs` for `src/a.rs`).
    if let Some(mod_name) = clean.strip_prefix("mod:") {
        let (mod_dir, _) = rust_module_dir(&root_canon, from_rel);
        for c in [mod_dir.join(format!("{mod_name}.rs")), mod_dir.join(mod_name).join("mod.rs")] {
            if let Some(rel) = check_contained_file(&root_canon, &c) {
                return Some(rel);
            }
        }
        return None;
    }

    let (clean_path, _, _) = parse_path_and_line(clean);
    let clean = clean_path.as_str();

    // Relative paths (`./...`, `../...`)
    if clean.starts_with("./") || clean.starts_with("../") || clean == "." || clean == ".." {
        let joined = from_dir.join(clean);
        if let Some(found) = probe_js_file(&joined) {
            if let Some(rel) = check_contained_file(&root_canon, &found) {
                return Some(rel);
            }
        }
        for ext in [".py", "/__init__.py", ".rs", "/mod.rs"] {
            let cand = PathBuf::from(format!("{}{ext}", joined.to_string_lossy()));
            if let Some(rel) = check_contained_file(&root_canon, &cand) {
                return Some(rel);
            }
        }
    }

    // tsconfig.json / jsconfig.json path aliases & baseUrl
    if let Some(cfg) = load_tsconfig(&root_canon, from_rel) {
        for (pat, targets) in &cfg.paths {
            if let Some(star) = pat.find('*') {
                let (pre, suf) = (&pat[..star], &pat[star + 1..]);
                if clean.starts_with(pre) && clean.ends_with(suf) && clean.len() >= pre.len() + suf.len() {
                    let captured = &clean[pre.len()..clean.len() - suf.len()];
                    for tgt in targets {
                        let replaced = tgt.replace('*', captured);
                        if let Some(found) = probe_js_file(&cfg.base_url.join(replaced)) {
                            if let Some(rel) = check_contained_file(&root_canon, &found) {
                                return Some(rel);
                            }
                        }
                    }
                }
            } else if pat == clean {
                for tgt in targets {
                    if let Some(found) = probe_js_file(&cfg.base_url.join(tgt)) {
                        if let Some(rel) = check_contained_file(&root_canon, &found) {
                            return Some(rel);
                        }
                    }
                }
            }
        }
        if let Some(found) = probe_js_file(&cfg.base_url.join(clean)) {
            if let Some(rel) = check_contained_file(&root_canon, &found) {
                return Some(rel);
            }
        }
    }

    // Direct workspace-relative file or probe
    let direct = root_canon.join(clean);
    if let Some(rel) = check_contained_file(&root_canon, &direct) {
        return Some(rel);
    }
    if let Some(found) = probe_js_file(&direct) {
        if let Some(rel) = check_contained_file(&root_canon, &found) {
            return Some(rel);
        }
    }
    let rel_from_dir = from_dir.join(clean);
    if let Some(rel) = check_contained_file(&root_canon, &rel_from_dir) {
        return Some(rel);
    }
    if let Some(found) = probe_js_file(&rel_from_dir) {
        if let Some(rel) = check_contained_file(&root_canon, &found) {
            return Some(rel);
        }
    }

    // Suffix match in workspace file list (re-verified through canonical containment check)
    let norm_q = norm_slashes(clean).trim_start_matches("./").trim_start_matches('/').to_string();
    if is_safe_rel(&norm_q) {
        for cand in all_files.iter().filter(|f| norm_cmp(f) == norm_cmp(&norm_q)) {
            if let Some(rel) = check_contained_file(&root_canon, &root_canon.join(cand)) {
                return Some(rel);
            }
        }
        let suffix = format!("/{norm_q}");
        let mut matches: Vec<&String> = all_files.iter().filter(|f| norm_cmp(f).ends_with(&norm_cmp(&suffix))).collect();
        matches.sort_by_key(|f| f.len());
        for best in matches {
            if let Some(rel) = check_contained_file(&root_canon, &root_canon.join(best)) {
                return Some(rel);
            }
        }
    }

    None
}

fn find_crate_src_dir(root_dir: &Path, from_rel: &str) -> PathBuf {
    let mut dir = if from_rel.is_empty() {
        root_dir.to_path_buf()
    } else {
        root_dir.join(from_rel).parent().unwrap_or(root_dir).to_path_buf()
    };
    loop {
        if dir.join("Cargo.toml").is_file() {
            let src = dir.join("src");
            if src.is_dir() {
                return src;
            }
            return dir;
        }
        if norm_cmp(&dir.display().to_string()) == norm_cmp(&root_dir.display().to_string()) {
            break;
        }
        if !dir.pop() {
            break;
        }
    }
    let src = root_dir.join("src");
    if src.is_dir() { src } else { root_dir.to_path_buf() }
}

fn resolve_rust_mod_path(root_dir: &Path, from_rel: &str, segments: &[&str]) -> Option<String> {
    if segments.is_empty() {
        return None;
    }
    let root_canon = canon_clean(root_dir).unwrap_or_else(|| root_dir.to_path_buf());
    let (mod_dir, _) = rust_module_dir(&root_canon, from_rel);
    let mut from_super = false;
    let (base, rest): (PathBuf, &[&str]) = match segments[0] {
        "crate" => (find_crate_src_dir(&root_canon, from_rel), &segments[1..]),
        "self" => (mod_dir, &segments[1..]),
        "super" => {
            from_super = true;
            let mut d = mod_dir;
            let mut idx = 0;
            while idx < segments.len() && segments[idx] == "super" {
                d = d.parent().unwrap_or(&root_canon).to_path_buf();
                idx += 1;
            }
            (d, &segments[idx..])
        }
        _ => (mod_dir, segments),
    };
    if rest.is_empty() {
        for entry in ["lib.rs", "main.rs", "mod.rs"] {
            if let Some(r) = check_contained_file(&root_canon, &base.join(entry)) {
                return Some(r);
            }
        }
        if from_super && norm_cmp(&base.display().to_string()) != norm_cmp(&root_canon.display().to_string()) {
            if let Some(r) = check_contained_file(&root_canon, &base.with_extension("rs")) {
                return Some(r);
            }
        }
        if segments[0] == "self" && !from_rel.is_empty() {
            if let Some(r) = check_contained_file(&root_canon, &root_canon.join(from_rel)) {
                return Some(r);
            }
        }
        return None;
    }
    let mut cur = base;
    for s in &rest[..rest.len().saturating_sub(1)] {
        cur.push(s);
    }
    let last = rest[rest.len() - 1];
    for cand in [cur.join(format!("{last}.rs")), cur.join(last).join("mod.rs")] {
        if let Some(r) = check_contained_file(&root_canon, &cand) {
            return Some(r);
        }
    }
    None
}

#[derive(Debug)]
struct ImportTarget {
    spec: String,
    imported_name: String,
    is_star: bool,
    is_default: bool,
}

fn find_js_import_for_symbol(text: &str, symbol: &str, container: Option<&str>) -> Vec<ImportTarget> {
    let mut out = Vec::new();
    // Normalize multi-line import/export statements into logical statements ending with `;` or newline-closed `from '...'`.
    let collapsed = collapse_js_imports(text);
    let named_re = Regex::new(r#"import\s+(?:type\s+)?(?:([A-Za-z_$][\w$]*)\s*,\s*)?\{([^}]+)\}\s*from\s*['"]([^'"]+)['"]"#).unwrap();
    let def_re = Regex::new(r#"import\s+(?:type\s+)?([A-Za-z_$][\w$]*)\s+from\s*['"]([^'"]+)['"]"#).unwrap();
    let ns_re = Regex::new(r#"import\s+\*\s+as\s+([A-Za-z_$][\w$]*)\s+from\s*['"]([^'"]+)['"]"#).unwrap();
    let req_destr_re = Regex::new(r#"(?:const|let|var)\s+\{([^}]+)\}\s*=\s*require\s*\(\s*['"]([^'"]+)['"]\s*\)"#).unwrap();
    let req_def_re = Regex::new(r#"(?:const|let|var)\s+([A-Za-z_$][\w$]*)\s*=\s*require\s*\(\s*['"]([^'"]+)['"]\s*\)"#).unwrap();

    for stmt in &collapsed {
        if let Some(c) = named_re.captures(stmt) {
            let spec = c[3].to_string();
            if let Some(def_local) = c.get(1) {
                if def_local.as_str() == symbol {
                    out.push(ImportTarget { spec: spec.clone(), imported_name: "default".into(), is_star: false, is_default: true });
                }
            }
            for item in c[2].split(',') {
                let item = item.trim().trim_start_matches("type ").trim();
                if item.is_empty() {
                    continue;
                }
                if let Some((orig, alias)) = item.split_once(" as ") {
                    if alias.trim() == symbol {
                        let orig_name = orig.trim();
                        out.push(ImportTarget {
                            spec: spec.clone(),
                            imported_name: orig_name.to_string(),
                            is_star: false,
                            is_default: orig_name == "default",
                        });
                    }
                } else if item == symbol {
                    out.push(ImportTarget { spec: spec.clone(), imported_name: symbol.to_string(), is_star: false, is_default: false });
                }
            }
        } else if let Some(c) = def_re.captures(stmt) {
            if &c[1] == symbol {
                out.push(ImportTarget { spec: c[2].to_string(), imported_name: "default".into(), is_star: false, is_default: true });
            }
        } else if let Some(c) = ns_re.captures(stmt) {
            if container == Some(&c[1]) {
                out.push(ImportTarget {
                    spec: c[2].to_string(),
                    imported_name: symbol.to_string(),
                    is_star: true,
                    is_default: symbol == "default",
                });
            }
        } else if let Some(c) = req_destr_re.captures(stmt) {
            let spec = c[2].to_string();
            for item in c[1].split(',') {
                let item = item.trim();
                if let Some((orig, alias)) = item.split_once(':') {
                    if alias.trim() == symbol {
                        let orig_name = orig.trim();
                        out.push(ImportTarget {
                            spec: spec.clone(),
                            imported_name: orig_name.to_string(),
                            is_star: false,
                            is_default: orig_name == "default",
                        });
                    }
                } else if item == symbol {
                    out.push(ImportTarget { spec: spec.clone(), imported_name: symbol.to_string(), is_star: false, is_default: false });
                }
            }
        } else if let Some(c) = req_def_re.captures(stmt) {
            if &c[1] == symbol || container == Some(&c[1]) {
                out.push(ImportTarget { spec: c[2].to_string(), imported_name: symbol.to_string(), is_star: false, is_default: false });
            }
        }
    }
    out
}

fn collapse_js_imports(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut collecting = false;
    for raw in text.lines() {
        let t = raw.trim();
        if !collecting {
            if t.starts_with("import ") || ((t.starts_with("const ") || t.starts_with("let ") || t.starts_with("var ")) && t.contains("require(")) {
                buf.clear();
                buf.push_str(t);
                if t.ends_with(';') || t.contains(" from ") || t.contains("require(") && t.ends_with(')') {
                    out.push(buf.clone());
                } else {
                    collecting = true;
                }
            }
        } else {
            buf.push(' ');
            buf.push_str(t);
            if t.ends_with(';') || t.contains(" from ") || buf.len() > 2048 {
                out.push(buf.clone());
                collecting = false;
            }
        }
    }
    out
}

fn read_js_target(root_canon: &Path, target_rel: &str, cur_rel: &str, cur_content: Option<&str>) -> Option<String> {
    if let Some(draft) = cur_content {
        if !cur_rel.is_empty() && norm_cmp(target_rel) == norm_cmp(cur_rel) {
            return Some(draft.to_string());
        }
    }
    read_contained_text_file(root_canon, &root_canon.join(target_rel))
}

fn find_js_default_export(
    root_canon: &Path,
    target_rel: &str,
    text: &str,
    defs: &[IndexedDef],
    cur_rel: &str,
    cur_content: Option<&str>,
    all_files: &[String],
    depth: u8,
) -> Option<(String, IndexedDef)> {
    let def_kw_re = Regex::new(r"^\s*export\s+(default)\b").unwrap();
    let def_ident_re = Regex::new(r"^\s*export\s+default\s+([A-Za-z_$][\w$]*)\s*(?:;.*|//.*|/\*.*)?$").unwrap();
    let local_as_def_re = Regex::new(r#"export\s+\{([^}]+)\}(?:\s*;|\s*$)"#).unwrap();

    for (idx, line) in line_spans(text).into_iter().enumerate() {
        let ln = idx + 1;
        let Some(kw) = def_kw_re.captures(line).and_then(|c| c.get(1)) else {
            continue;
        };
        if let Some(d) = defs.iter().find(|d| d.line == ln) {
            return Some((target_rel.to_string(), d.clone()));
        }
        if let Some(ident_m) = def_ident_re.captures(line).and_then(|c| c.get(1)) {
            let ident = ident_m.as_str();
            if !matches!(ident, "function" | "class" | "async" | "abstract" | "new" | "true" | "false" | "null" | "undefined" | "typeof" | "await") {
                if let Some(d) = defs.iter().find(|d| d.name == ident) {
                    return Some((target_rel.to_string(), d.clone()));
                }
                if depth > 0 {
                    for imp in find_js_import_for_symbol(text, ident, None) {
                        if let Some(next_rel) = resolve_import_specifier(root_canon, target_rel, &imp.spec, all_files) {
                            if let Some(found) = resolve_js_reexport_chain(
                                root_canon,
                                &next_rel,
                                &imp.imported_name,
                                cur_rel,
                                cur_content,
                                all_files,
                                depth - 1,
                            ) {
                                return Some(found);
                            }
                        }
                    }
                }
            }
        }
        let kind = if line.contains("function") {
            "function"
        } else if line.contains("class") {
            "class"
        } else {
            "const"
        };
        return Some((
            target_rel.to_string(),
            IndexedDef {
                name: "default".into(),
                kind,
                line: ln,
                col: utf16_col(line, kw.start()),
                len: kw.as_str().len(),
                container: None,
                exported: true,
                preview: trunc_chars(line, MAX_PREVIEW_CHARS),
            },
        ));
    }

    for line in line_spans(text) {
        if line.contains(" from ") {
            continue;
        }
        if let Some(c) = local_as_def_re.captures(line) {
            for item in c[1].split(',') {
                let item = item.trim().trim_start_matches("type ").trim();
                if let Some((orig, alias)) = item.split_once(" as ") {
                    if alias.trim() == "default" {
                        let orig = orig.trim();
                        if let Some(d) = defs.iter().find(|d| d.name == orig) {
                            return Some((target_rel.to_string(), d.clone()));
                        }
                        if depth > 0 {
                            for imp in find_js_import_for_symbol(text, orig, None) {
                                if let Some(next_rel) = resolve_import_specifier(root_canon, target_rel, &imp.spec, all_files) {
                                    if let Some(found) = resolve_js_reexport_chain(
                                        root_canon,
                                        &next_rel,
                                        &imp.imported_name,
                                        cur_rel,
                                        cur_content,
                                        all_files,
                                        depth - 1,
                                    ) {
                                        return Some(found);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    None
}

fn resolve_js_reexport_chain(
    root_canon: &Path,
    target_rel: &str,
    sym: &str,
    cur_rel: &str,
    cur_content: Option<&str>,
    all_files: &[String],
    depth: u8,
) -> Option<(String, IndexedDef)> {
    let text = read_js_target(root_canon, target_rel, cur_rel, cur_content)?;
    let defs = extract_definitions(LangFamily::JsTs, &text);
    if sym == "default" {
        if let Some(found) = find_js_default_export(root_canon, target_rel, &text, &defs, cur_rel, cur_content, all_files, depth) {
            return Some(found);
        }
    } else if let Some(d) = defs.into_iter().find(|d| d.name == sym) {
        return Some((target_rel.to_string(), d));
    }
    if depth == 0 {
        return None;
    }
    let re_named = Regex::new(r#"export\s+(?:type\s+)?\{([^}]+)\}\s*from\s*['"]([^'"]+)['"]"#).unwrap();
    let re_star = Regex::new(r#"export\s+\*\s+from\s*['"]([^'"]+)['"]"#).unwrap();
    for c in re_named.captures_iter(&text) {
        let spec = &c[2];
        for item in c[1].split(',') {
            let item = item.trim().trim_start_matches("type ").trim();
            let orig = if let Some((o, a)) = item.split_once(" as ") {
                if a.trim() == sym { Some(o.trim()) } else { None }
            } else if item == sym {
                Some(sym)
            } else {
                None
            };
            if let Some(orig_name) = orig {
                if let Some(next_rel) = resolve_import_specifier(root_canon, target_rel, spec, all_files) {
                    if let Some(found) =
                        resolve_js_reexport_chain(root_canon, &next_rel, orig_name, cur_rel, cur_content, all_files, depth - 1)
                    {
                        return Some(found);
                    }
                }
            }
        }
    }
    if sym != "default" {
        for c in re_star.captures_iter(&text) {
            if let Some(next_rel) = resolve_import_specifier(root_canon, target_rel, &c[1], all_files) {
                if let Some(found) = resolve_js_reexport_chain(root_canon, &next_rel, sym, cur_rel, cur_content, all_files, depth - 1) {
                    return Some(found);
                }
            }
        }
    }
    None
}

fn find_rust_use_targets(root_dir: &Path, from_rel: &str, text: &str, symbol: &str, container: Option<&str>) -> Vec<String> {
    let mut targets = Vec::new();
    // Collapse multi-line `use ...;` statements
    let mut stmts = Vec::new();
    let mut buf = String::new();
    let mut collecting = false;
    for raw in text.lines() {
        let t = raw.trim();
        if !collecting {
            if t.starts_with("use ") || t.starts_with("pub use ") {
                buf.clear();
                buf.push_str(t);
                if t.ends_with(';') {
                    stmts.push(buf.clone());
                } else {
                    collecting = true;
                }
            }
        } else {
            buf.push(' ');
            buf.push_str(t);
            if t.ends_with(';') || buf.len() > 2048 {
                stmts.push(buf.clone());
                collecting = false;
            }
        }
    }

    for stmt in stmts {
        let body = stmt.trim_start_matches("pub ").trim_start_matches("use ").trim_end_matches(';').trim();
        if let Some(brace) = body.find('{') {
            let prefix = body[..brace].trim().trim_end_matches("::");
            let inner = body[brace + 1..].trim_end_matches('}');
            for part in inner.split(',') {
                let p = part.trim();
                let (orig, alias) = p.split_once(" as ").map(|(a, b)| (a.trim(), b.trim())).unwrap_or((p, p));
                if alias == symbol || container == Some(alias) {
                    let mut segs: Vec<&str> = prefix.split("::").filter(|s| !s.is_empty()).collect();
                    if orig == "self" {
                        // module itself
                    } else if alias == symbol && !segs.is_empty() {
                        // Look inside prefix module (or prefix/orig if orig is a module)
                        if let Some(r) = resolve_rust_mod_path(root_dir, from_rel, &segs) {
                            targets.push(r);
                        }
                        segs.push(orig);
                    } else {
                        segs.push(orig);
                    }
                    if let Some(r) = resolve_rust_mod_path(root_dir, from_rel, &segs) {
                        targets.push(r);
                    }
                }
            }
        } else {
            let (path_part, alias) = body.split_once(" as ").map(|(a, b)| (a.trim(), b.trim())).unwrap_or((body, body.rsplit("::").next().unwrap_or(body)));
            let segs: Vec<&str> = path_part.split("::").filter(|s| !s.is_empty()).collect();
            if alias == "*" && segs.len() >= 2 {
                if let Some(r) = resolve_rust_mod_path(root_dir, from_rel, &segs[..segs.len() - 1]) {
                    targets.push(r);
                }
            } else if alias == symbol && segs.len() >= 2 {
                if let Some(r) = resolve_rust_mod_path(root_dir, from_rel, &segs[..segs.len() - 1]) {
                    targets.push(r);
                }
            } else if container == Some(alias) {
                if let Some(r) = resolve_rust_mod_path(root_dir, from_rel, &segs) {
                    targets.push(r);
                }
            }
        }
    }
    if let Some(cont) = container {
        // Direct sibling module: `mod_name::symbol`
        if let Some(r) = resolve_rust_mod_path(root_dir, from_rel, &[cont]) {
            targets.push(r);
        }
    }
    targets
}

fn find_py_import_targets(root_dir: &Path, from_rel: &str, text: &str, symbol: &str, all_files: &[String]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let re = Regex::new(r"^\s*from\s+(\.{0,3}[A-Za-z0-9_.]*)\s+import\s+(.+)$").unwrap();
    for line in text.lines() {
        let Some(c) = re.captures(line) else { continue };
        let mod_spec = &c[1];
        for item in c[2].split(',') {
            let item = item.trim();
            let (orig, alias) = item.split_once(" as ").map(|(a, b)| (a.trim(), b.trim())).unwrap_or((item, item));
            if alias != symbol {
                continue;
            }
            let dots = mod_spec.chars().take_while(|&ch| ch == '.').count();
            let rest = mod_spec[dots..].replace('.', "/");
            let spec = if dots > 0 {
                let ups = "../".repeat(dots.saturating_sub(1));
                let prefix = if ups.is_empty() { "./".to_string() } else { ups };
                format!("{prefix}{rest}")
            } else {
                rest
            };
            if let Some(rel) = resolve_import_specifier(root_dir, from_rel, &spec, all_files) {
                out.push((rel, orig.to_string()));
            }
        }
    }
    out
}

fn shared_dir_depth(a: &str, b: &str) -> usize {
    let pa: Vec<&str> = a.rsplit_once('/').map_or("", |(d, _)| d).split('/').filter(|s| !s.is_empty()).collect();
    let pb: Vec<&str> = b.rsplit_once('/').map_or("", |(d, _)| d).split('/').filter(|s| !s.is_empty()).collect();
    pa.iter().zip(pb.iter()).take_while(|(x, y)| x == y).count()
}

fn same_dir(a: &str, b: &str) -> bool {
    let da = a.rsplit_once('/').map_or("", |(d, _)| d);
    let db = b.rsplit_once('/').map_or("", |(d, _)| d);
    !da.is_empty() && norm_cmp(da) == norm_cmp(db)
}

fn top_package(rel: &str) -> &str {
    rel.split('/').next().unwrap_or("")
}

fn resolve_definition(provider: &str, req: &DefRequest) -> Result<NavDefinitionResult> {
    let raw_root = crate::paths::expand(&req.root);
    let root_dir = expand_root(&req.root)?;
    let all_files = update_workspace_index(&root_dir);
    let cur_rel = normalize_rel_path(&root_dir, Some(&raw_root), &req.path);
    let req_kind = req.kind.as_deref().unwrap_or("auto");

    let read_target = |rel: &str| -> Option<String> {
        if let Some(ref draft) = req.content {
            if !cur_rel.is_empty() && norm_cmp(rel) == norm_cmp(&cur_rel) {
                return Some(draft.clone());
            }
        }
        read_contained_text_file(&root_dir, &root_dir.join(rel))
    };

    if req_kind == "path" || (req_kind == "auto" && req.path_text.as_deref().is_some_and(|s| !s.trim().is_empty())) {
        let raw_path = req.path_text.as_deref().or(req.symbol.as_deref()).unwrap_or("").trim();
        let (clean, target_line, target_col) = parse_path_and_line(raw_path);
        let mut locations = Vec::new();
        if let Some(rel) = resolve_import_specifier(&root_dir, &cur_rel, &clean, &all_files) {
            let abs = join_abs(&root_dir, &rel);
            let preview = read_target(&rel)
                .and_then(|t| line_spans(&t).get(target_line.saturating_sub(1)).map(|l| trunc_chars(l, MAX_PREVIEW_CHARS)))
                .unwrap_or_else(|| rel.clone());
            locations.push(NavLocation {
                path: rel,
                abs_path: abs,
                line: target_line,
                col: target_col,
                len: 0,
                end_line: None,
                kind: "file".into(),
                symbol: clean.clone(),
                container: None,
                preview,
                score: 1000,
                source: "path".into(),
            });
        }
        return Ok(NavDefinitionResult {
            provider: provider.into(),
            kind: "path".into(),
            query: raw_path.to_string(),
            at_definition: false,
            locations,
        });
    }

    let symbol = req.symbol.as_deref().unwrap_or("").trim();
    if symbol.is_empty() {
        return Ok(NavDefinitionResult {
            provider: provider.into(),
            kind: "symbol".into(),
            query: String::new(),
            at_definition: false,
            locations: Vec::new(),
        });
    }

    let cur_lang = LangFamily::from_path(&cur_rel);
    let has_draft_content = req.content.is_some();
    let cur_text = req
        .content
        .clone()
        .or_else(|| (!cur_rel.is_empty()).then(|| read_contained_text_file(&root_dir, &root_dir.join(&cur_rel))).flatten())
        .unwrap_or_default();

    let cur_lines = line_spans(&cur_text);
    let cur_line_text = cur_lines.get(req.line.saturating_sub(1)).copied().unwrap_or("");
    let on_import_line = is_import_line(cur_lang, cur_line_text.trim());
    let cur_defs = if !cur_text.is_empty() {
        extract_definitions(cur_lang, &cur_text)
    } else {
        Vec::new()
    };
    let local_defs = if !cur_text.is_empty() && !on_import_line {
        resolve_visible_local_bindings(cur_lang, &cur_text, symbol, req.line, req.col)
    } else {
        Vec::new()
    };

    let overlaps_cursor = |d: &IndexedDef| {
        d.name == symbol && d.line == req.line && req.col >= d.col && req.col <= d.col + d.len
    };
    let at_definition = !on_import_line && (cur_defs.iter().any(overlaps_cursor) || local_defs.iter().any(|(_, d)| overlaps_cursor(d)));
    let shadowed_by_local = !on_import_line && req.container.is_none() && local_defs.iter().any(|(sd, _)| *sd > 0);

    let effective_container: Option<String> = match req.container.as_deref() {
        Some("this" | "self" | "Self") if !cur_text.is_empty() => find_enclosing_container(cur_lang, &cur_text, req.line),
        Some(c) if !c.is_empty() => Some(c.to_string()),
        _ => None,
    };

    let mut best_by_site: HashMap<(String, usize), NavLocation> = HashMap::new();
    let mut record_loc = |loc: NavLocation| {
        let key = (norm_cmp(&loc.abs_path), loc.line);
        match best_by_site.get_mut(&key) {
            Some(prev) if prev.score >= loc.score => {}
            Some(prev) => *prev = loc,
            None => {
                best_by_site.insert(key, loc);
            }
        }
    };

    // 0. Inner-scope lexical binding shadows imports and workspace symbols.
    if shadowed_by_local {
        for (sd, def) in &local_defs {
            if at_definition && overlaps_cursor(def) {
                continue;
            }
            if *sd > 0 {
                record_loc(NavLocation {
                    path: cur_rel.clone(),
                    abs_path: join_abs(&root_dir, &cur_rel),
                    line: def.line,
                    col: def.col,
                    len: def.len,
                    end_line: None,
                    kind: def.kind.into(),
                    symbol: def.name.clone(),
                    container: def.container.clone(),
                    preview: def.preview.clone(),
                    score: 1050,
                    source: "local".into(),
                });
                break;
            }
        }
    }

    // 1. Import-tracked target in current file (when not lexically shadowed by a local declaration)
    if !cur_text.is_empty() && !shadowed_by_local {
        match cur_lang {
            LangFamily::JsTs => {
                for imp in find_js_import_for_symbol(&cur_text, symbol, req.container.as_deref()) {
                    if let Some(target_rel) = resolve_import_specifier(&root_dir, &cur_rel, &imp.spec, &all_files) {
                        if let Some((final_rel, def)) = resolve_js_reexport_chain(
                            &root_dir,
                            &target_rel,
                            &imp.imported_name,
                            &cur_rel,
                            req.content.as_deref(),
                            &all_files,
                            2,
                        ) {
                            record_loc(NavLocation {
                                abs_path: join_abs(&root_dir, &final_rel),
                                path: final_rel,
                                line: def.line,
                                col: def.col,
                                len: def.len,
                                end_line: None,
                                kind: def.kind.into(),
                                symbol: def.name,
                                container: def.container,
                                preview: def.preview,
                                score: 1000,
                                source: "import".into(),
                            });
                        } else if imp.is_star || imp.is_default || imp.imported_name == symbol {
                            // Imported module resolved; fall back to line 1 of module if no export declaration matched.
                            let abs = join_abs(&root_dir, &target_rel);
                            let preview = read_target(&target_rel)
                                .and_then(|t| line_spans(&t).first().map(|l| trunc_chars(l, MAX_PREVIEW_CHARS)))
                                .unwrap_or_else(|| target_rel.clone());
                            record_loc(NavLocation {
                                path: target_rel,
                                abs_path: abs,
                                line: 1,
                                col: 0,
                                len: 0,
                                end_line: None,
                                kind: "file".into(),
                                symbol: symbol.to_string(),
                                container: None,
                                preview,
                                score: 940,
                                source: "import".into(),
                            });
                        }
                    }
                }
            }
            LangFamily::Rust => {
                for target_rel in find_rust_use_targets(&root_dir, &cur_rel, &cur_text, symbol, req.container.as_deref()) {
                    if let Some(t) = read_target(&target_rel) {
                        for def in extract_definitions(LangFamily::Rust, &t).into_iter().filter(|d| d.name == symbol) {
                            record_loc(NavLocation {
                                abs_path: join_abs(&root_dir, &target_rel),
                                path: target_rel.clone(),
                                line: def.line,
                                col: def.col,
                                len: def.len,
                                end_line: None,
                                kind: def.kind.into(),
                                symbol: def.name,
                                container: def.container,
                                preview: def.preview,
                                score: 1000,
                                source: "import".into(),
                            });
                        }
                    }
                }
            }
            LangFamily::Python => {
                for (target_rel, orig_sym) in find_py_import_targets(&root_dir, &cur_rel, &cur_text, symbol, &all_files) {
                    if let Some(t) = read_target(&target_rel) {
                        for def in extract_definitions(LangFamily::Python, &t).into_iter().filter(|d| d.name == orig_sym) {
                            record_loc(NavLocation {
                                abs_path: join_abs(&root_dir, &target_rel),
                                path: target_rel.clone(),
                                line: def.line,
                                col: def.col,
                                len: def.len,
                                end_line: None,
                                kind: def.kind.into(),
                                symbol: def.name,
                                container: def.container,
                                preview: def.preview,
                                score: 1000,
                                source: "import".into(),
                            });
                        }
                    }
                }
            }
            _ => {}
        }
    }

    // 2. Same-file definitions & top-level local declarations
    if !cur_text.is_empty() && !shadowed_by_local {
        for def in cur_defs.iter().filter(|d| d.name == symbol) {
            if at_definition && overlaps_cursor(def) {
                continue;
            }
            record_loc(NavLocation {
                path: cur_rel.clone(),
                abs_path: join_abs(&root_dir, &cur_rel),
                line: def.line,
                col: def.col,
                len: def.len,
                end_line: None,
                kind: def.kind.into(),
                symbol: def.name.clone(),
                container: def.container.clone(),
                preview: def.preview.clone(),
                score: 920,
                source: "same-file".into(),
            });
        }
        if !on_import_line {
            for (_, def) in local_defs {
                if at_definition && overlaps_cursor(&def) {
                    continue;
                }
                record_loc(NavLocation {
                    path: cur_rel.clone(),
                    abs_path: join_abs(&root_dir, &cur_rel),
                    line: def.line,
                    col: def.col,
                    len: def.len,
                    end_line: None,
                    kind: def.kind.into(),
                    symbol: def.name,
                    container: def.container,
                    preview: def.preview,
                    score: 880,
                    source: "same-file".into(),
                });
            }
        }
    }

    // 3. Workspace index matches (skipping the active unsaved buffer's stale disk entry & shadowed imports)
    if !shadowed_by_local {
        let caller_is_test = is_test_path(&cur_rel);
        for (rel, lang, def) in query_index(&root_dir, symbol) {
            if has_draft_content && !cur_rel.is_empty() && norm_cmp(&rel) == norm_cmp(&cur_rel) {
                continue;
            }
            if at_definition && norm_cmp(&rel) == norm_cmp(&cur_rel) && overlaps_cursor(&def) {
                continue;
            }
            let same_lang = cur_lang == LangFamily::Other || lang == cur_lang;
            let mut score: i32 = if !same_lang {
                300
            } else if same_dir(&rel, &cur_rel) {
                800
            } else if !cur_rel.is_empty() && top_package(&rel) == top_package(&cur_rel) && def.exported {
                720
            } else if def.exported {
                640
            } else {
                540
            };

            score += (shared_dir_depth(&rel, &cur_rel) as i32 * 8).min(30);
            if !caller_is_test {
                score += if is_test_path(&rel) { -35 } else { 35 };
            }
            if let Some(cont) = effective_container.as_deref() {
                let c_low = cont.to_ascii_lowercase();
                if let Some(dc) = def.container.as_deref() {
                    let dc_low = dc.to_ascii_lowercase();
                    if dc == cont {
                        score += 240;
                    } else if dc_low == c_low || dc_low.ends_with(&c_low) || dc_low.contains(&c_low) {
                        score += 150;
                    }
                }
                let stem = Path::new(&rel).file_stem().and_then(|s| s.to_str()).unwrap_or("");
                if stem.eq_ignore_ascii_case(cont) {
                    score += 220;
                }
                if def.kind == "method" {
                    score += 40;
                }
            } else if matches!(def.kind, "function" | "class" | "struct" | "interface" | "type" | "enum") {
                score += 20;
            }

            record_loc(NavLocation {
                abs_path: join_abs(&root_dir, &rel),
                path: rel,
                line: def.line,
                col: def.col,
                len: def.len,
                end_line: None,
                kind: def.kind.into(),
                symbol: def.name,
                container: def.container,
                preview: def.preview,
                score,
                source: "index".into(),
            });
        }
    }

    let mut locations: Vec<NavLocation> = best_by_site.into_values().collect();
    locations.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.path.cmp(&b.path)).then_with(|| a.line.cmp(&b.line)));
    locations.truncate(req.limit);

    Ok(NavDefinitionResult {
        provider: provider.into(),
        kind: "symbol".into(),
        query: symbol.to_string(),
        at_definition,
        locations,
    })
}

fn find_references(provider: &str, req: &RefRequest) -> Result<NavReferencesResult> {
    let raw_root = crate::paths::expand(&req.root);
    let root_dir = expand_root(&req.root)?;
    let symbol = req.symbol.trim();
    if symbol.is_empty() {
        return Ok(NavReferencesResult {
            provider: provider.into(),
            symbol: String::new(),
            truncated: false,
            total_files: 0,
            total_hits: 0,
            hits: Vec::new(),
        });
    }

    let cur_rel = normalize_rel_path(&root_dir, Some(&raw_root), &req.path);
    let target_lang = LangFamily::from_path(&cur_rel);
    let files = crate::fsapi::file_list(&root_dir);
    let lang_match = |rel: &str, allow_any: bool| -> bool {
        if allow_any {
            return true;
        }
        let fl = LangFamily::from_path(rel);
        if target_lang.is_code() {
            fl == target_lang
        } else if target_lang == LangFamily::Css {
            fl == LangFamily::Css || fl == LangFamily::JsTs || rel.ends_with(".html") || rel.ends_with(".htm")
        } else {
            true
        }
    };

    let mut result = scan_references(provider, &root_dir, &cur_rel, symbol, req, &files, |r| lang_match(r, false));
    if result.hits.is_empty() && target_lang.is_code() {
        result = scan_references(provider, &root_dir, &cur_rel, symbol, req, &files, |r| lang_match(r, true));
    }
    Ok(result)
}

fn scan_references<F: Fn(&str) -> bool>(
    provider: &str,
    root_dir: &Path,
    cur_rel: &str,
    symbol: &str,
    req: &RefRequest,
    files: &[String],
    filter: F,
) -> NavReferencesResult {
    let mut candidates: Vec<&str> = files
        .iter()
        .map(String::as_str)
        .filter(|rel| is_safe_rel(rel) && !is_ignored_asset(rel) && filter(rel))
        .collect();
    if req.content.is_some()
        && !cur_rel.is_empty()
        && is_safe_rel(cur_rel)
        && !is_ignored_asset(cur_rel)
        && filter(cur_rel)
        && !candidates.iter().any(|r| norm_cmp(r) == norm_cmp(cur_rel))
    {
        candidates.push(cur_rel);
    }
    candidates.sort_by(|a, b| {
        let a_cur = norm_cmp(a) == norm_cmp(cur_rel);
        let b_cur = norm_cmp(b) == norm_cmp(cur_rel);
        b_cur
            .cmp(&a_cur)
            .then_with(|| shared_dir_depth(b, cur_rel).cmp(&shared_dir_depth(a, cur_rel)))
            .then_with(|| a.cmp(b))
    });

    let started = Instant::now();
    let budget = std::time::Duration::from_secs(8);
    let mut hits = Vec::new();
    let mut hit_files = HashSet::new();
    let mut truncated = false;
    let sym_utf16 = utf16_len(symbol);
    let step_utf8 = symbol.chars().next().map_or(1, char::len_utf8);
    let mut checks = 0usize;

    let root_clean = canon_clean(root_dir).unwrap_or_else(|| root_dir.to_path_buf());
    let root_handle = DirHandle::open_root(&root_clean);

    'files: for rel in candidates {
        if started.elapsed() > budget {
            truncated = true;
            break;
        }
        let is_cur = !cur_rel.is_empty() && norm_cmp(rel) == norm_cmp(cur_rel);
        let text: String = if is_cur {
            if let Some(ref draft) = req.content {
                draft.clone()
            } else {
                let Some(ref dh) = root_handle else { continue };
                let Some(t) = read_contained_text_file_with(&root_clean, dh, &root_clean.join(rel)) else { continue };
                t
            }
        } else {
            let Some(ref dh) = root_handle else { continue };
            let Some(t) = read_contained_text_file_with(&root_clean, dh, &root_clean.join(rel)) else { continue };
            t
        };
        if !text.contains(symbol) {
            continue;
        }

        let lang = LangFamily::from_path(rel);
        let mut def_spans: HashMap<usize, Vec<(usize, usize)>> = HashMap::new();
        for d in extract_definitions(lang, &text) {
            if d.name == symbol {
                def_spans.entry(d.line).or_default().push((d.col, d.col + d.len));
            }
        }
        if is_cur {
            for (_, d) in resolve_visible_local_bindings(lang, &text, symbol, req.line, req.col) {
                def_spans.entry(d.line).or_default().push((d.col, d.col + d.len));
            }
        }

        let lines = line_spans(&text);
        let mut abs_cached: Option<String> = None;

        for (i, line) in lines.iter().enumerate() {
            let ln = i + 1;
            let line_is_import = is_import_line(lang, line.trim());
            let line_def_spans = def_spans.get(&ln);
            let mut from = 0usize;
            let mut cursor_byte = 0usize;
            let mut cursor_utf16 = 0usize;
            let mut line_ctx: Option<(String, Vec<String>)> = None;

            while let Some(pos) = line[from..].find(symbol) {
                checks += 1;
                if checks & 63 == 0 && started.elapsed() > budget {
                    truncated = true;
                    break 'files;
                }
                let start = from + pos;
                let end = start + symbol.len();
                let before_ok = !line[..start].chars().next_back().is_some_and(is_word_char);
                let after_ok = !line[end..].chars().next().is_some_and(is_word_char);
                if !(before_ok && after_ok) {
                    from = start + step_utf8;
                    continue;
                }
                from = end;
                cursor_utf16 += utf16_len(&line[cursor_byte..start]);
                cursor_byte = start;
                let col = cursor_utf16;
                let hit_end = col + sym_utf16;
                let is_decl_hit = !line_is_import
                    && line_def_spans.is_some_and(|spans| spans.iter().any(|&(s, e)| col == s && hit_end == e));
                if !req.include_declaration && is_cur && ln == req.line && is_decl_hit {
                    continue;
                }
                let (text_line, context) = line_ctx.get_or_insert_with(|| {
                    let ctx_start = ln.saturating_sub(req.context_lines).max(1);
                    let ctx_end = (ln + req.context_lines).min(lines.len());
                    let ctx: Vec<String> = lines[ctx_start - 1..ctx_end].iter().map(|l| trunc_line(l, MAX_CONTEXT_CHARS)).collect();
                    (trunc_line(line, MAX_CONTEXT_CHARS), ctx)
                });
                let abs = abs_cached.get_or_insert_with(|| join_abs(root_dir, rel)).clone();
                let ctx_start = ln.saturating_sub(req.context_lines).max(1);
                hits.push(NavReferenceHit {
                    path: rel.to_string(),
                    abs_path: abs,
                    line: ln,
                    col,
                    len: sym_utf16,
                    text: text_line.clone(),
                    is_definition: is_decl_hit,
                    context_start_line: ctx_start,
                    context: context.clone(),
                });
                hit_files.insert(rel.to_string());
                if hits.len() >= req.limit {
                    truncated = true;
                    break 'files;
                }
            }
        }
    }

    NavReferencesResult {
        provider: provider.into(),
        symbol: symbol.to_string(),
        truncated,
        total_files: hit_files.len(),
        total_hits: hits.len(),
        hits,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_req(root: &Path, path: &str, line: usize, col: usize, symbol: Option<&str>) -> DefRequest {
        DefRequest {
            root: root.display().to_string(),
            path: path.to_string(),
            line,
            col,
            symbol: symbol.map(str::to_string),
            kind: None,
            path_text: None,
            container: None,
            content: None,
            limit: 30,
        }
    }

    #[test]
    fn js_imports_alias_reexport_and_tsconfig() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        std::fs::create_dir_all(root.join("src/components")).unwrap();
        std::fs::create_dir_all(root.join("src/utils")).unwrap();
        std::fs::write(
            root.join("tsconfig.json"),
            "{\n  // alias config\n  \"compilerOptions\": { \"baseUrl\": \".\", \"paths\": { \"@/*\": [\"src/*\"] } }\n}",
        )
        .unwrap();
        std::fs::write(
            root.join("src/utils/nav.ts"),
            "export function helperFn(x: number) {\n  return x + 1;\n}\nexport const dup = () => 1;\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src/utils/index.ts"),
            "export { helperFn as renamedHelper } from './nav';\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src/components/App.tsx"),
            "import { renamedHelper } from '@/utils';\nexport function App() {\n  return renamedHelper(1);\n}\n",
        )
        .unwrap();

        let mut req = mk_req(root, "src/components/App.tsx", 3, 9, Some("renamedHelper"));
        let res = BuiltinNavProvider.definition(&req).unwrap();
        assert!(!res.at_definition);
        assert_eq!(res.locations[0].path, "src/utils/nav.ts");
        assert_eq!(res.locations[0].line, 1);
        assert_eq!(res.locations[0].source, "import");

        // Path resolution with tsconfig alias
        req.kind = Some("path".into());
        req.path_text = Some("@/utils/nav".into());
        req.symbol = None;
        let path_res = BuiltinNavProvider.definition(&req).unwrap();
        assert_eq!(path_res.locations[0].path, "src/utils/nav.ts");

        // Line & col parsing on path
        req.path_text = Some("src/utils/nav.ts:2:4".into());
        let line_res = BuiltinNavProvider.definition(&req).unwrap();
        assert_eq!(line_res.locations[0].path, "src/utils/nav.ts");
        assert_eq!(line_res.locations[0].line, 2);
        assert_eq!(line_res.locations[0].col, 3);
    }

    #[test]
    fn rust_mod_use_container_ranking_and_at_definition() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n").unwrap();
        std::fs::write(
            root.join("src/hub.rs"),
            "pub struct ClientHub;\nimpl ClientHub {\n    pub async fn call(&self, m: &str) {}\n}\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src/other.rs"),
            "pub struct Other;\nimpl Other {\n    pub fn call(&self) {}\n}\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src/main.rs"),
            "mod hub;\nmod other;\nuse crate::hub::ClientHub;\nfn run(h: &ClientHub) {\n    h.call(\"x\");\n}\n",
        )
        .unwrap();

        // mod hub; path resolution
        let mut req = mk_req(root, "src/main.rs", 1, 4, None);
        req.kind = Some("path".into());
        req.path_text = Some("mod:hub".into());
        let mod_res = BuiltinNavProvider.definition(&req).unwrap();
        assert_eq!(mod_res.locations[0].path, "src/hub.rs");

        // Container-ranked call()
        let mut call_req = mk_req(root, "src/main.rs", 5, 6, Some("call"));
        call_req.container = Some("ClientHub".into());
        let call_res = BuiltinNavProvider.definition(&call_req).unwrap();
        assert_eq!(call_res.locations[0].path, "src/hub.rs");
        assert_eq!(call_res.locations[0].container.as_deref(), Some("ClientHub"));

        // atDefinition = true on definition site
        let def_site_req = mk_req(root, "src/hub.rs", 3, 17, Some("call"));
        let def_site_res = BuiltinNavProvider.definition(&def_site_req).unwrap();
        assert!(def_site_res.at_definition);

        // atDefinition = false on `use` import line
        let use_req = mk_req(root, "src/main.rs", 3, 16, Some("ClientHub"));
        let use_res = BuiltinNavProvider.definition(&use_req).unwrap();
        assert!(!use_res.at_definition);
        assert_eq!(use_res.locations[0].path, "src/hub.rs");
    }

    #[test]
    fn python_and_go_extractors() {
        let py = "class Worker:\n    def handle(self, x):\n        return x\n\ndef top_fn():\n    pass\n";
        let py_defs = extract_definitions(LangFamily::Python, py);
        assert_eq!(py_defs.len(), 3);
        assert_eq!(py_defs[0].name, "Worker");
        assert_eq!(py_defs[1].name, "handle");
        assert_eq!(py_defs[1].container.as_deref(), Some("Worker"));
        assert_eq!(py_defs[2].name, "top_fn");

        let go = "package main\ntype Server struct{}\nfunc (s *Server) Serve() {}\nfunc NewServer() {}\n";
        let go_defs = extract_definitions(LangFamily::Go, go);
        assert_eq!(go_defs[0].name, "Server");
        assert_eq!(go_defs[1].name, "Serve");
        assert_eq!(go_defs[1].container.as_deref(), Some("Server"));
        assert_eq!(go_defs[2].name, "NewServer");
    }

    #[test]
    fn references_and_incremental_cache_update() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.ts"), "export function fooBar() {}\n").unwrap();
        std::fs::write(
            root.join("src/b.ts"),
            "import { fooBar } from './a';\nconst a = 1;\nfooBar();\nconst b = 2;\n",
        )
        .unwrap();
        std::fs::write(root.join("notes.md"), "Mention fooBar in docs\n").unwrap();

        let ref_req = RefRequest {
            root: root.display().to_string(),
            path: "src/a.ts".into(),
            line: 1,
            col: 16,
            symbol: "fooBar".into(),
            content: None,
            include_declaration: false,
            context_lines: 1,
            limit: 50,
        };
        let refs = BuiltinNavProvider.references(&ref_req).unwrap();
        // Self-definition on src/a.ts:1 excluded; code-family filter skips notes.md when TS hits exist
        assert_eq!(refs.total_hits, 2);
        assert!(refs.hits.iter().all(|h| h.path == "src/b.ts"));
        let call_hit = refs.hits.iter().find(|h| h.line == 3).unwrap();
        assert_eq!(call_hit.context_start_line, 2);
        assert_eq!(call_hit.context.len(), 3);

        // Incremental cache detects modification & deletion
        let _ = BuiltinNavProvider.definition(&mk_req(root, "src/b.ts", 3, 0, Some("fooBar"))).unwrap();
        std::fs::write(root.join("src/a.ts"), "export function bazQux(v: number) { return v + 1; }\n").unwrap();
        let after_edit = BuiltinNavProvider.definition(&mk_req(root, "src/b.ts", 3, 0, Some("bazQux"))).unwrap();
        assert_eq!(after_edit.locations[0].path, "src/a.ts");
        assert_eq!(after_edit.locations[0].line, 1);

        std::fs::remove_file(root.join("src/a.ts")).unwrap();
        let after_del = BuiltinNavProvider.definition(&mk_req(root, "src/b.ts", 3, 0, Some("bazQux"))).unwrap();
        assert!(after_del.locations.is_empty());
    }

    #[test]
    fn non_bmp_utf16_cols_draft_truth_and_per_hit_decl_filter() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        let line1 = "export function emojiFn() { const s = \"🚀\"; return s.length + emojiFn(); }";
        let line2 = "export function wrap() { const p = \"🚀\"; const localSym = 1; return localSym; }";
        std::fs::write(root.join("src/emoji.ts"), format!("{line1}\n{line2}\n")).unwrap();

        let decl_u16 = utf16_col(line1, line1.find("emojiFn").unwrap());
        let call_u16 = utf16_col(line1, line1.rfind("emojiFn").unwrap());
        // "🚀" takes 2 UTF-16 code units vs 1 code point, shifting call_u16 +1 vs char count
        assert_eq!(call_u16, line1[..line1.rfind("emojiFn").unwrap()].chars().count() + 1);

        let local_decl_byte = line2.find("localSym").unwrap();
        let local_decl_u16 = utf16_col(line2, local_decl_byte);
        let local_call_u16 = utf16_col(line2, line2.rfind("localSym").unwrap());
        assert_eq!(local_decl_u16, line2[..local_decl_byte].chars().count() + 1);

        let at_def = BuiltinNavProvider
            .definition(&mk_req(root, "src/emoji.ts", 1, decl_u16, Some("emojiFn")))
            .unwrap();
        assert!(at_def.at_definition);

        let at_local_def = BuiltinNavProvider
            .definition(&mk_req(root, "src/emoji.ts", 2, local_decl_u16, Some("localSym")))
            .unwrap();
        assert!(at_local_def.at_definition);

        let from_local_call = BuiltinNavProvider
            .definition(&mk_req(root, "src/emoji.ts", 2, local_call_u16, Some("localSym")))
            .unwrap();
        assert!(!from_local_call.at_definition);
        assert_eq!(from_local_call.locations[0].col, local_decl_u16);
        assert_eq!(from_local_call.locations[0].len, 8);

        let from_call = BuiltinNavProvider
            .definition(&mk_req(root, "src/emoji.ts", 1, call_u16, Some("emojiFn")))
            .unwrap();
        assert!(!from_call.at_definition);
        assert_eq!(from_call.locations[0].col, decl_u16);
        assert_eq!(from_call.locations[0].len, 7);

        // Include declaration = false must keep the recursive call on the same line, skipping only decl span
        let refs = BuiltinNavProvider
            .references(&RefRequest {
                root: root.display().to_string(),
                path: "src/emoji.ts".into(),
                line: 1,
                col: decl_u16,
                symbol: "emojiFn".into(),
                content: None,
                include_declaration: false,
                context_lines: 0,
                limit: 50,
            })
            .unwrap();
        assert_eq!(refs.total_hits, 1);
        assert_eq!(refs.hits[0].col, call_u16);
        assert_eq!(refs.hits[0].len, 7);
        assert!(!refs.hits[0].is_definition);

        // With include_declaration = true, only declaration span is marked `is_definition: true`
        let refs_all = BuiltinNavProvider
            .references(&RefRequest {
                root: root.display().to_string(),
                path: "src/emoji.ts".into(),
                line: 1,
                col: decl_u16,
                symbol: "emojiFn".into(),
                content: None,
                include_declaration: true,
                context_lines: 0,
                limit: 50,
            })
            .unwrap();
        assert_eq!(refs_all.total_hits, 2);
        assert!(refs_all.hits[0].is_definition);
        assert_eq!(refs_all.hits[0].col, decl_u16);
        assert!(!refs_all.hits[1].is_definition);
        assert_eq!(refs_all.hits[1].col, call_u16);

        // Draft deletion hides disk definition, and draft references pick up unsaved buffer hits
        let mut draft_req = mk_req(root, "src/emoji.ts", 1, call_u16, Some("emojiFn"));
        draft_req.content = Some("export function otherFn() {}\notherFn();\n".into());
        let deleted_def = BuiltinNavProvider.definition(&draft_req).unwrap();
        assert!(deleted_def.locations.is_empty());

        let draft_refs = BuiltinNavProvider
            .references(&RefRequest {
                root: root.display().to_string(),
                path: "src/emoji.ts".into(),
                line: 1,
                col: 16,
                symbol: "otherFn".into(),
                content: draft_req.content.clone(),
                include_declaration: false,
                context_lines: 0,
                limit: 50,
            })
            .unwrap();
        assert_eq!(draft_refs.total_hits, 1);
        assert_eq!(draft_refs.hits[0].line, 2);
    }

    #[test]
    fn lexical_shadowing_default_import_and_rust_submod() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        std::fs::create_dir_all(root.join("src/a")).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n").unwrap();

        // Default export resolution (Foo exported as default, imported as Bar, plus unrelated Bar)
        std::fs::write(
            root.join("src/mod.ts"),
            "export const Bar = 'unrelated';\nexport default function RealFoo() {\n  return 1;\n}\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src/reexport.ts"),
            "export { default } from './mod';\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src/consumer.ts"),
            "import Bar from './reexport';\nfunction shadowFn(Bar: number) {\n  return Bar + 1;\n}\nfunction otherBlock() {\n  const innerOnly = 1;\n  return innerOnly;\n}\nfunction caller() {\n  const x = Bar();\n  return x + innerOnly;\n}\n",
        )
        .unwrap();

        // Non-shadowed call to Bar resolves to RealFoo in src/mod.ts (NOT const Bar)
        let def_bar = BuiltinNavProvider
            .definition(&mk_req(root, "src/consumer.ts", 10, 12, Some("Bar")))
            .unwrap();
        assert_eq!(def_bar.locations[0].path, "src/mod.ts");
        assert_eq!(def_bar.locations[0].line, 2);
        assert_eq!(def_bar.locations[0].symbol, "RealFoo");

        // Parameter `Bar` inside `shadowFn` shadows `import Bar from './reexport'`
        let shadowed = BuiltinNavProvider
            .definition(&mk_req(root, "src/consumer.ts", 3, 9, Some("Bar")))
            .unwrap();
        assert_eq!(shadowed.locations.len(), 1);
        assert_eq!(shadowed.locations[0].path, "src/consumer.ts");
        assert_eq!(shadowed.locations[0].line, 2);
        assert_eq!(shadowed.locations[0].source, "local");

        // Block-local `innerOnly` in `otherBlock` must NOT leak into `caller`
        let leaked = BuiltinNavProvider
            .definition(&mk_req(root, "src/consumer.ts", 11, 13, Some("innerOnly")))
            .unwrap();
        assert!(leaked.locations.is_empty());

        // Rust submodule: `mod b;` inside `src/a.rs` resolves to `src/a/b.rs`, not sibling `src/b.rs`
        std::fs::write(root.join("src/b.rs"), "pub fn b_top() {}\n").unwrap();
        std::fs::write(root.join("src/a/b.rs"), "pub fn b_sub() {}\n").unwrap();
        std::fs::write(
            root.join("src/a.rs"),
            "mod b;\nuse self::b::b_sub;\nuse super::b::b_top;\n",
        )
        .unwrap();

        let mut mod_req = mk_req(root, "src/a.rs", 1, 4, None);
        mod_req.kind = Some("path".into());
        mod_req.path_text = Some("mod:b".into());
        let mod_res = BuiltinNavProvider.definition(&mod_req).unwrap();
        assert_eq!(mod_res.locations[0].path, "src/a/b.rs");

        let sub_use = BuiltinNavProvider
            .definition(&mk_req(root, "src/a.rs", 2, 14, Some("b_sub")))
            .unwrap();
        assert_eq!(sub_use.locations[0].path, "src/a/b.rs");

        // Outside `src/a/b.rs`, `use super::a_sibling` / `use super::b::b_top`
        std::fs::write(root.join("src/a/c.rs"), "use super::b::b_sub;\n").unwrap();
        let sup_use = BuiltinNavProvider
            .definition(&mk_req(root, "src/a/c.rs", 1, 14, Some("b_sub")))
            .unwrap();
        assert_eq!(sup_use.locations[0].path, "src/a/b.rs");
    }

    #[test]
    fn dense_single_line_respects_limit_and_workspace_containment() {
        let ws = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = ws.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(outside.path().join("secret.ts"), "export const secretSym = 42;\n").unwrap();

        // Create symlink/junction from ws/ext -> outside when OS permits
        #[cfg(unix)]
        {
            let _ = std::os::unix::fs::symlink(outside.path(), root.join("ext"));
        }
        #[cfg(windows)]
        {
            let _ = std::process::Command::new("cmd")
                .args(["/c", "mklink", "/J"])
                .arg(root.join("ext"))
                .arg(outside.path())
                .output();
        }

        // Dense line with 10,000 occurrences stops cleanly at limit=5
        let dense = format!("const keep = {};\n", vec!["denseSym"; 10_000].join(" + "));
        std::fs::write(root.join("src/dense.ts"), dense).unwrap();

        let dense_refs = BuiltinNavProvider
            .references(&RefRequest {
                root: root.display().to_string(),
                path: "src/dense.ts".into(),
                line: 1,
                col: 0,
                symbol: "denseSym".into(),
                content: None,
                include_declaration: true,
                context_lines: 0,
                limit: 5,
            })
            .unwrap();
        assert_eq!(dense_refs.hits.len(), 5);
        assert!(dense_refs.truncated);

        // Out-of-workspace files are never indexed, resolved via suffix/escape, or read by references
        let esc_def = BuiltinNavProvider
            .definition(&mk_req(root, "src/dense.ts", 1, 0, Some("secretSym")))
            .unwrap();
        assert!(esc_def.locations.is_empty());

        let mut path_req = mk_req(root, "src/dense.ts", 1, 0, None);
        path_req.kind = Some("path".into());
        path_req.path_text = Some("ext/secret.ts".into());
        let esc_path = BuiltinNavProvider.definition(&path_req).unwrap();
        assert!(esc_path.locations.is_empty());

        let esc_refs = BuiltinNavProvider
            .references(&RefRequest {
                root: root.display().to_string(),
                path: "src/dense.ts".into(),
                line: 1,
                col: 0,
                symbol: "secretSym".into(),
                content: None,
                include_declaration: true,
                context_lines: 0,
                limit: 20,
            })
            .unwrap();
        assert_eq!(esc_refs.total_hits, 0);
    }

    #[test]
    fn handle_confined_open_blocks_escaped_file_and_dir_symlinks() {
        let ws = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root_canon = canon_clean(ws.path()).unwrap();
        std::fs::create_dir_all(root_canon.join("src")).unwrap();
        std::fs::write(root_canon.join("src/ok.ts"), "export const insideSym = 1;\n").unwrap();
        std::fs::write(outside.path().join("outside.ts"), "export const leakFileSym = 9;\n").unwrap();

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(outside.path(), root_canon.join("ext_dir")).unwrap();
            std::os::unix::fs::symlink(outside.path().join("outside.ts"), root_canon.join("src/leak.ts")).unwrap();
            std::os::unix::fs::symlink(root_canon.join("src/ok.ts"), root_canon.join("src/in_link.ts")).unwrap();
        }
        #[cfg(windows)]
        {
            let _ = std::process::Command::new("cmd")
                .args(["/c", "mklink", "/J"])
                .arg(root_canon.join("ext_dir"))
                .arg(outside.path())
                .output();
            let _ = std::os::windows::fs::symlink_file(
                outside.path().join("outside.ts"),
                root_canon.join("src/leak.ts"),
            );
        }

        let dh = DirHandle::open_root(&root_canon).unwrap();
        assert!(dh.open_contained(&root_canon, "src/ok.ts").is_some());
        if root_canon.join("ext_dir/outside.ts").exists() {
            assert!(dh.open_contained(&root_canon, "ext_dir/outside.ts").is_none());
        }
        if std::fs::symlink_metadata(root_canon.join("src/leak.ts")).is_ok() {
            assert!(dh.open_contained(&root_canon, "src/leak.ts").is_none());
        }
        #[cfg(unix)]
        assert!(dh.open_contained(&root_canon, "src/in_link.ts").is_some());

        let files = update_workspace_index(&root_canon);
        assert!(files.iter().any(|p| p == "src/ok.ts"));
        let hits = query_index(&root_canon, "leakFileSym");
        assert!(hits.is_empty());
    }

    #[test]
    fn path_strictly_under_preserves_component_case() {
        assert!(path_strictly_under(Path::new("/tmp/parent/repo"), Path::new("/tmp/parent/repo/src/a.ts")));
        assert!(!path_strictly_under(Path::new("/tmp/parent/repo"), Path::new("/tmp/parent/repo")));
        assert!(!path_strictly_under(Path::new("/tmp/parent/repo"), Path::new("/tmp/parent/REPO/secret.ts")));
        assert!(!path_strictly_under(Path::new("/tmp/parent/repo"), Path::new("/tmp/parent/repo2/a.ts")));

        #[cfg(windows)]
        {
            assert!(path_strictly_under(
                Path::new(r"\\?\D:\parent\repo"),
                Path::new(r"\\?\D:\parent\repo\src\a.ts"),
            ));
            assert!(!path_strictly_under(
                Path::new(r"\\?\D:\parent\repo"),
                Path::new(r"\\?\D:\parent\REPO\secret.ts"),
            ));
            assert!(!path_strictly_under(
                Path::new(r"D:\parent\repo"),
                Path::new(r"D:\parent\REPO\secret.ts"),
            ));
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_case_sensitive_sibling_dir_is_rejected() {
        let base = tempfile::tempdir().unwrap();
        let parent = base.path().join("cs_parent");
        std::fs::create_dir_all(&parent).unwrap();
        let cs_ok = std::process::Command::new("fsutil")
            .args(["file", "setCaseSensitiveInfo"])
            .arg(&parent)
            .arg("enable")
            .output()
            .is_ok_and(|o| o.status.success());

        let repo = parent.join("repo");
        let sibling_upper = parent.join("REPO");
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::write(repo.join("src/ok.ts"), "export const insideCaseSym = 1;\n").unwrap();

        let distinct_case_dirs = cs_ok && std::fs::create_dir(&sibling_upper).is_ok();
        let outside_dir = if distinct_case_dirs {
            assert!(!repo.join("secret.ts").exists());
            std::fs::write(sibling_upper.join("secret.ts"), "export const secretCaseSym = 9;\n").unwrap();
            assert!(!repo.join("secret.ts").exists());
            sibling_upper
        } else {
            eprintln!("note: fsutil case-sensitive directory unavailable on this host; verifying handle-based junction containment");
            let fallback = base.path().join("outside");
            std::fs::create_dir_all(&fallback).unwrap();
            std::fs::write(fallback.join("secret.ts"), "export const secretCaseSym = 9;\n").unwrap();
            fallback
        };

        let mklink_out = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(repo.join("ext"))
            .arg(&outside_dir)
            .output()
            .unwrap();
        assert!(mklink_out.status.success(), "mklink failed: {}", String::from_utf8_lossy(&mklink_out.stderr));

        let repo_canon = canon_clean(&repo).unwrap();
        let dh = DirHandle::open_root(&repo_canon).unwrap();
        assert!(dh.open_contained(&repo_canon, "src/ok.ts").is_some());
        assert!(dh.open_contained(&repo_canon, "ext/secret.ts").is_none());

        update_workspace_index(&repo_canon);
        assert!(!query_index(&repo_canon, "insideCaseSym").is_empty());
        assert!(query_index(&repo_canon, "secretCaseSym").is_empty());
    }

    #[test]
    fn concurrent_workspace_index_updates_preserve_newer_files() {
        let ws = tempfile::tempdir().unwrap();
        let root = canon_clean(ws.path()).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.ts"), "export const initialRaceSym = 1;\n").unwrap();

        update_workspace_index(&root);
        assert!(!query_index(&root, "initialRaceSym").is_empty());

        let barrier = Arc::new(std::sync::Barrier::new(2));
        let root1 = root.clone();
        let b1 = barrier.clone();
        let t1 = std::thread::spawn(move || {
            b1.wait();
            for _ in 0..4 {
                update_workspace_index(&root1);
            }
        });

        let root2 = root.clone();
        let b2 = barrier;
        let t2 = std::thread::spawn(move || {
            b2.wait();
            std::fs::write(root2.join("src/a.ts"), "export const updatedAfterRaceSym = 3;\n").unwrap();
            std::fs::write(root2.join("src/b.ts"), "export const addedRaceSym = 2;\n").unwrap();
            crate::fsapi::invalidate_file_list_cache(&root2);
            update_workspace_index(&root2);
        });

        t1.join().unwrap();
        t2.join().unwrap();

        assert!(query_index(&root, "initialRaceSym").is_empty());
        assert!(!query_index(&root, "updatedAfterRaceSym").is_empty());
        assert!(!query_index(&root, "addedRaceSym").is_empty());
    }

    #[test]
    fn evicted_workspace_refresh_preserves_unchanged_definitions() {
        let w1_tmp = tempfile::tempdir().unwrap();
        let w1 = canon_clean(w1_tmp.path()).unwrap();
        std::fs::create_dir_all(w1.join("src")).unwrap();
        std::fs::write(w1.join("src/keep.ts"), "export const keepAfterEvictSym = 1;\n").unwrap();
        std::fs::write(w1.join("src/change.ts"), "export const oldChangeSym = 1;\n").unwrap();

        update_workspace_index(&w1);
        assert!(!query_index(&w1, "keepAfterEvictSym").is_empty());
        assert!(!query_index(&w1, "oldChangeSym").is_empty());

        std::fs::write(w1.join("src/change.ts"), "export const newChangeSym = 200;\n").unwrap();
        let mut other_tmps = Vec::new();
        update_workspace_index_inner(&w1, || {
            for i in 0..MAX_CACHED_WORKSPACES {
                let d = tempfile::tempdir().unwrap();
                let r = canon_clean(d.path()).unwrap();
                std::fs::create_dir_all(r.join("src")).unwrap();
                std::fs::write(r.join("src/d.ts"), format!("export const otherWs{i}Sym = {i};\n")).unwrap();
                update_workspace_index(&r);
                other_tmps.push(d);
            }
        });

        assert!(query_index(&w1, "oldChangeSym").is_empty());
        assert!(!query_index(&w1, "newChangeSym").is_empty());
        assert!(!query_index(&w1, "keepAfterEvictSym").is_empty());
        drop(other_tmps);
    }

    #[cfg(unix)]
    #[test]
    fn unix_open_root_rejects_symlink_component() {
        let tmp = tempfile::tempdir().unwrap();
        let base = canon_clean(tmp.path()).unwrap();
        let real = base.join("real");
        let sub = real.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let link_leaf = base.join("link_leaf");
        let link_anc = base.join("link_anc");
        std::os::unix::fs::symlink(&sub, &link_leaf).unwrap();
        std::os::unix::fs::symlink(&real, &link_anc).unwrap();

        assert!(DirHandle::open_root(&sub).is_some());
        assert!(DirHandle::open_root(&link_leaf).is_none());
        assert!(DirHandle::open_root(&link_anc.join("sub")).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn unix_open_root_and_rel_allow_execute_only_dirs() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let base = canon_clean(tmp.path()).unwrap();
        let anc = base.join("anc");
        let repo = anc.join("repo");
        let sub = repo.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let target = sub.join("a.ts");
        std::fs::write(&target, "export const x = 1;\n").unwrap();

        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o111)).unwrap();
        std::fs::set_permissions(&anc, std::fs::Permissions::from_mode(0o111)).unwrap();

        let root_handle = DirHandle::open_root(&repo);
        let opened = root_handle
            .as_ref()
            .and_then(|h| h.open_contained(&repo, "sub/a.ts"));

        std::fs::set_permissions(&anc, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert!(root_handle.is_some());
        assert!(opened.is_some());
    }

    #[cfg(unix)]
    #[test]
    fn unix_backslash_filename_is_preserved_not_split() {
        let tmp = tempfile::tempdir().unwrap();
        let root = canon_clean(tmp.path()).unwrap();
        let nested_dir = root.join("src").join("a");
        std::fs::create_dir_all(&nested_dir).unwrap();
        std::fs::write(root.join("src").join(r"a\b.ts"), "export const backslashFileSym = 1;\n").unwrap();
        std::fs::write(nested_dir.join("b.ts"), "export const nestedDecoySym = 2;\n").unwrap();

        let rel_bs = normalize_rel_path(&root, None, r"src/a\b.ts");
        assert_eq!(rel_bs, r"src/a\b.ts");
        assert_eq!(join_abs(&root, &rel_bs), root.join("src").join(r"a\b.ts").display().to_string());

        let handle = DirHandle::open_root(&root).unwrap();
        let (file, _) = handle.open_contained(&root, r"src/a\b.ts").unwrap();
        assert!(read_text_from_file(file).unwrap().contains("backslashFileSym"));

        update_workspace_index(&root);
        let bs_hits = query_index(&root, "backslashFileSym");
        let decoy_hits = query_index(&root, "nestedDecoySym");
        assert_eq!(bs_hits.len(), 1);
        assert_eq!(bs_hits[0].0, r"src/a\b.ts");
        assert_eq!(decoy_hits.len(), 1);
        assert_eq!(decoy_hits[0].0, "src/a/b.ts");
    }
}
