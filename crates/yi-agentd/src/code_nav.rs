//! Workspace code navigation: go-to-definition, import/path resolution, and references lookup.
//! Designed behind a `NavProvider` trait so an optional LSP provider can be plugged in later without changing RPC callers.

use anyhow::{bail, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
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
    #[serde(default)]
    /// Reserved for future LSP position-based providers.
    #[allow(dead_code)]
    pub col: usize,
    pub symbol: String,
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
        let base = path.rsplit(['/', '\\']).next().unwrap_or(path);
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
    files: HashMap<String, IndexedFile>,
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
static TSCONFIG_CACHE: Mutex<Option<HashMap<String, CachedTsConfig>>> = Mutex::new(None);

#[derive(Clone)]
struct CachedTsConfig {
    mtime: i64,
    base_url: PathBuf,
    paths: Vec<(String, Vec<String>)>,
}

fn expand_root(raw: &str) -> Result<PathBuf> {
    let p = crate::paths::expand(raw);
    let canon = p
        .canonicalize()
        .map(|c| PathBuf::from(crate::fsapi::strip_verbatim(&c.display().to_string())))
        .unwrap_or(p);
    if !canon.is_dir() {
        bail!("目录不存在: {}", canon.display());
    }
    Ok(canon)
}

fn norm_slashes(p: &str) -> String {
    p.replace('\\', "/")
}

fn norm_cmp(p: &str) -> String {
    let s = norm_slashes(p).trim_end_matches('/').to_string();
    if cfg!(windows) {
        s.to_lowercase()
    } else {
        s
    }
}

fn normalize_rel_path(root: &Path, path: &str) -> String {
    if path.is_empty() {
        return String::new();
    }
    let root_str = norm_slashes(&crate::fsapi::strip_verbatim(&root.display().to_string())).trim_end_matches('/').to_string();
    let p_str = norm_slashes(&crate::fsapi::strip_verbatim(path));
    let root_cmp = norm_cmp(&root_str);
    let p_cmp = norm_cmp(&p_str);
    if p_cmp == root_cmp {
        return String::new();
    }
    let prefix = format!("{root_cmp}/");
    if p_cmp.starts_with(&prefix) && p_str.is_char_boundary(root_str.len() + 1) {
        return p_str[root_str.len() + 1..].to_string();
    }
    p_str.trim_start_matches("./").trim_start_matches('/').to_string()
}

fn join_abs(root: &Path, rel: &str) -> String {
    let mut pb = root.to_path_buf();
    for seg in rel.split(['/', '\\']).filter(|s| !s.is_empty() && *s != ".") {
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

fn read_text_file(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_FILE_BYTES {
        return None;
    }
    let buf = std::fs::read(path).ok()?;
    if buf.iter().take(8000).any(|&b| b == 0) {
        return None;
    }
    Some(String::from_utf8_lossy(&buf).into_owned())
}

fn char_col(line: &str, byte_idx: usize) -> usize {
    line[..byte_idx].chars().count()
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
    let col = char_col(line, m.start());
    let len = name.chars().count();
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

/// Extract local declarations (`let`/`const`/`var`/params) in the current file up to `max_line`.
fn extract_local_defs(lang: LangFamily, text: &str, symbol: &str, max_line: usize) -> Vec<IndexedDef> {
    let mut out = Vec::new();
    let esc = regex::escape(symbol);
    let pat = match lang {
        LangFamily::JsTs => format!(r"\b(?:const|let|var)\s+(?:\{{[^}}]*\b|\[\s*[^]]*\b)?({esc})\b"),
        LangFamily::Rust => format!(r"\blet\s+(?:mut\s+)?(?:\([^)]*\b)?({esc})\b"),
        LangFamily::Go => format!(r"\b({esc})\s*:="),
        LangFamily::Python => format!(r"^\s*({esc})\s*(?::[^=]+)?="),
        _ => return out,
    };
    let Ok(re) = Regex::new(&pat) else {
        return out;
    };
    for (idx, line) in text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).enumerate() {
        let ln = idx + 1;
        if ln > max_line {
            break;
        }
        let trimmed = line.trim();
        if trimmed.starts_with("//") || trimmed.starts_with('#') || is_import_line(lang, trimmed) {
            continue;
        }
        if let Some(c) = re.captures(line) {
            if let Some(m) = c.get(1) {
                // Avoid flagging comparisons (`==`, `===`, `!=`, `<=`, `>=`) or property keys
                let after = line[m.end()..].trim_start();
                if lang == LangFamily::Python && (after.starts_with("==") || after.is_empty()) {
                    continue;
                }
                push_def(&mut out, ln, line, m, "const", None, false);
            }
        }
    }
    out
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
    let root_key = norm_cmp(&root_dir.display().to_string());
    let file_list = crate::fsapi::file_list(root_dir);
    let started = Instant::now();

    let mut guard = INDEX_CACHE.lock().unwrap();
    let (seq, cache) = guard.get_or_insert_with(|| (0, HashMap::new()));
    *seq += 1;
    let now_seq = *seq;

    if !cache.contains_key(&root_key) && cache.len() >= MAX_CACHED_WORKSPACES {
        if let Some(oldest_k) = cache.iter().min_by_key(|(_, v)| v.last_used).map(|(k, _)| k.clone()) {
            cache.remove(&oldest_k);
        }
    }

    let ws = cache.entry(root_key).or_default();
    ws.last_used = now_seq;

    let mut live_set = HashSet::with_capacity(file_list.len());
    let mut changed = false;

    for rel in file_list.iter() {
        if is_ignored_asset(rel) {
            continue;
        }
        let lang = LangFamily::from_path(rel);
        if lang == LangFamily::Other {
            continue;
        }
        if started.elapsed() > std::time::Duration::from_secs(6) {
            live_set.insert(rel.clone());
            continue;
        }
        let full = root_dir.join(rel);
        let Ok(meta) = std::fs::metadata(&full) else { continue };
        if !meta.is_file() || meta.len() > MAX_FILE_BYTES {
            continue;
        }
        live_set.insert(rel.clone());
        let mt = mtime_ms(&meta);
        let sz = meta.len();
        if let Some(prev) = ws.files.get(rel) {
            if prev.mtime == mt && prev.size == sz {
                continue;
            }
        }
        let Ok(buf) = std::fs::read(&full) else { continue };
        if buf.iter().take(8000).any(|&b| b == 0) {
            continue;
        }
        let text = String::from_utf8_lossy(&buf);
        let defs = extract_definitions(lang, &text);
        ws.files.insert(rel.clone(), IndexedFile { mtime: mt, size: sz, lang, defs });
        changed = true;
    }

    let before_len = ws.files.len();
    ws.files.retain(|k, _| live_set.contains(k));
    if ws.files.len() != before_len {
        changed = true;
    }

    if changed || (ws.by_name.is_empty() && !ws.files.is_empty()) {
        ws.rebuild_lookup();
    }

    file_list.as_ref().clone()
}

fn query_index(root_dir: &Path, symbol: &str) -> Vec<(String, LangFamily, IndexedDef)> {
    let root_key = norm_cmp(&root_dir.display().to_string());
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
    let mut dir = if from_rel.is_empty() {
        root_dir.to_path_buf()
    } else {
        let p = root_dir.join(from_rel);
        p.parent().unwrap_or(root_dir).to_path_buf()
    };
    let mut found: Option<PathBuf> = None;
    loop {
        for name in ["tsconfig.json", "jsconfig.json"] {
            let cand = dir.join(name);
            if cand.is_file() {
                found = Some(cand);
                break;
            }
        }
        if found.is_some() || norm_cmp(&dir.display().to_string()) == norm_cmp(&root_dir.display().to_string()) {
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
    let raw = std::fs::read_to_string(&cfg_path).ok()?;
    let val: Value = serde_json::from_str(&strip_jsonc(&raw)).ok()?;
    let co = &val["compilerOptions"];
    let cfg_dir = cfg_path.parent().unwrap_or(root_dir);
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

fn check_contained_file(root_dir: &Path, candidate: &Path) -> Option<String> {
    let canon = candidate.canonicalize().ok()?;
    if !canon.is_file() {
        return None;
    }
    let root_canon = root_dir.canonicalize().ok().unwrap_or_else(|| root_dir.to_path_buf());
    if !canon.starts_with(&root_canon) {
        return None;
    }
    let rel = canon.strip_prefix(&root_canon).ok()?;
    Some(norm_slashes(&rel.to_string_lossy()))
}

fn resolve_import_specifier(root_dir: &Path, from_rel: &str, spec: &str, all_files: &[String]) -> Option<String> {
    let clean = spec.trim().trim_matches(|c| matches!(c, '\'' | '"' | '`' | '<' | '>'));
    if clean.is_empty() {
        return None;
    }
    let from_dir = if from_rel.is_empty() {
        root_dir.to_path_buf()
    } else {
        root_dir.join(from_rel).parent().unwrap_or(root_dir).to_path_buf()
    };

    // Rust `mod foo;`
    if let Some(mod_name) = clean.strip_prefix("mod:") {
        let stem = Path::new(from_rel).file_stem().and_then(|s| s.to_str()).unwrap_or("");
        let mut cands = vec![from_dir.join(format!("{mod_name}.rs")), from_dir.join(mod_name).join("mod.rs")];
        if !stem.is_empty() && stem != "mod" && stem != "lib" && stem != "main" {
            cands.push(from_dir.join(stem).join(format!("{mod_name}.rs")));
            cands.push(from_dir.join(stem).join(mod_name).join("mod.rs"));
        }
        for c in cands {
            if let Some(rel) = check_contained_file(root_dir, &c) {
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
            if let Some(rel) = check_contained_file(root_dir, &found) {
                return Some(rel);
            }
        }
        for ext in [".py", "/__init__.py", ".rs", "/mod.rs"] {
            let cand = PathBuf::from(format!("{}{ext}", joined.to_string_lossy()));
            if let Some(rel) = check_contained_file(root_dir, &cand) {
                return Some(rel);
            }
        }
    }

    // tsconfig.json / jsconfig.json path aliases & baseUrl
    if let Some(cfg) = load_tsconfig(root_dir, from_rel) {
        for (pat, targets) in &cfg.paths {
            if let Some(star) = pat.find('*') {
                let (pre, suf) = (&pat[..star], &pat[star + 1..]);
                if clean.starts_with(pre) && clean.ends_with(suf) && clean.len() >= pre.len() + suf.len() {
                    let captured = &clean[pre.len()..clean.len() - suf.len()];
                    for tgt in targets {
                        let replaced = tgt.replace('*', captured);
                        if let Some(found) = probe_js_file(&cfg.base_url.join(replaced)) {
                            if let Some(rel) = check_contained_file(root_dir, &found) {
                                return Some(rel);
                            }
                        }
                    }
                }
            } else if pat == clean {
                for tgt in targets {
                    if let Some(found) = probe_js_file(&cfg.base_url.join(tgt)) {
                        if let Some(rel) = check_contained_file(root_dir, &found) {
                            return Some(rel);
                        }
                    }
                }
            }
        }
        if let Some(found) = probe_js_file(&cfg.base_url.join(clean)) {
            if let Some(rel) = check_contained_file(root_dir, &found) {
                return Some(rel);
            }
        }
    }

    // Direct workspace-relative file or probe
    let direct = root_dir.join(clean);
    if let Some(rel) = check_contained_file(root_dir, &direct) {
        return Some(rel);
    }
    if let Some(found) = probe_js_file(&direct) {
        if let Some(rel) = check_contained_file(root_dir, &found) {
            return Some(rel);
        }
    }
    let rel_from_dir = from_dir.join(clean);
    if let Some(rel) = check_contained_file(root_dir, &rel_from_dir) {
        return Some(rel);
    }
    if let Some(found) = probe_js_file(&rel_from_dir) {
        if let Some(rel) = check_contained_file(root_dir, &found) {
            return Some(rel);
        }
    }

    // Suffix match in workspace file list (e.g. `docs/internals/file-panel.md` or `crates/proto/src/lib.rs`)
    let norm_q = norm_slashes(clean).trim_start_matches("./").trim_start_matches('/').to_string();
    if !norm_q.is_empty() {
        if let Some(exact) = all_files.iter().find(|f| norm_cmp(f) == norm_cmp(&norm_q)) {
            return Some(exact.clone());
        }
        let suffix = format!("/{norm_q}");
        let mut matches: Vec<&String> = all_files.iter().filter(|f| norm_cmp(f).ends_with(&norm_cmp(&suffix))).collect();
        matches.sort_by_key(|f| f.len());
        if let Some(best) = matches.first() {
            return Some((*best).clone());
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
    let from_dir = root_dir.join(from_rel).parent().unwrap_or(root_dir).to_path_buf();
    let (base, rest): (PathBuf, &[&str]) = match segments[0] {
        "crate" => (find_crate_src_dir(root_dir, from_rel), &segments[1..]),
        "self" => (from_dir, &segments[1..]),
        "super" => {
            let mut d = from_dir;
            let mut idx = 0;
            while idx < segments.len() && segments[idx] == "super" {
                d = d.parent().unwrap_or(root_dir).to_path_buf();
                idx += 1;
            }
            (d, &segments[idx..])
        }
        _ => (from_dir, segments),
    };
    if rest.is_empty() {
        for entry in ["lib.rs", "main.rs", "mod.rs"] {
            if let Some(r) = check_contained_file(root_dir, &base.join(entry)) {
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
        if let Some(r) = check_contained_file(root_dir, &cand) {
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
                    out.push(ImportTarget { spec: spec.clone(), imported_name: symbol.to_string(), is_star: false });
                }
            }
            for item in c[2].split(',') {
                let item = item.trim().trim_start_matches("type ").trim();
                if item.is_empty() {
                    continue;
                }
                if let Some((orig, alias)) = item.split_once(" as ") {
                    if alias.trim() == symbol {
                        out.push(ImportTarget { spec: spec.clone(), imported_name: orig.trim().to_string(), is_star: false });
                    }
                } else if item == symbol {
                    out.push(ImportTarget { spec: spec.clone(), imported_name: symbol.to_string(), is_star: false });
                }
            }
        } else if let Some(c) = def_re.captures(stmt) {
            if &c[1] == symbol {
                out.push(ImportTarget { spec: c[2].to_string(), imported_name: symbol.to_string(), is_star: false });
            }
        } else if let Some(c) = ns_re.captures(stmt) {
            if container == Some(&c[1]) {
                out.push(ImportTarget { spec: c[2].to_string(), imported_name: symbol.to_string(), is_star: true });
            }
        } else if let Some(c) = req_destr_re.captures(stmt) {
            let spec = c[2].to_string();
            for item in c[1].split(',') {
                let item = item.trim();
                if let Some((orig, alias)) = item.split_once(':') {
                    if alias.trim() == symbol {
                        out.push(ImportTarget { spec: spec.clone(), imported_name: orig.trim().to_string(), is_star: false });
                    }
                } else if item == symbol {
                    out.push(ImportTarget { spec: spec.clone(), imported_name: symbol.to_string(), is_star: false });
                }
            }
        } else if let Some(c) = req_def_re.captures(stmt) {
            if &c[1] == symbol || container == Some(&c[1]) {
                out.push(ImportTarget { spec: c[2].to_string(), imported_name: symbol.to_string(), is_star: false });
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

fn resolve_js_reexport_chain(
    root_dir: &Path,
    target_rel: &str,
    sym: &str,
    all_files: &[String],
    depth: u8,
) -> Option<(String, IndexedDef)> {
    let text = read_text_file(&root_dir.join(target_rel))?;
    let defs = extract_definitions(LangFamily::JsTs, &text);
    if let Some(d) = defs.into_iter().find(|d| d.name == sym) {
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
                if let Some(next_rel) = resolve_import_specifier(root_dir, target_rel, spec, all_files) {
                    if let Some(found) = resolve_js_reexport_chain(root_dir, &next_rel, orig_name, all_files, depth - 1) {
                        return Some(found);
                    }
                }
            }
        }
    }
    for c in re_star.captures_iter(&text) {
        if let Some(next_rel) = resolve_import_specifier(root_dir, target_rel, &c[1], all_files) {
            if let Some(found) = resolve_js_reexport_chain(root_dir, &next_rel, sym, all_files, depth - 1) {
                return Some(found);
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
    let root_dir = expand_root(&req.root)?;
    let all_files = update_workspace_index(&root_dir);
    let cur_rel = normalize_rel_path(&root_dir, &req.path);
    let req_kind = req.kind.as_deref().unwrap_or("auto");

    if req_kind == "path" || (req_kind == "auto" && req.path_text.as_deref().is_some_and(|s| !s.trim().is_empty())) {
        let raw_path = req.path_text.as_deref().or(req.symbol.as_deref()).unwrap_or("").trim();
        let (clean, target_line, target_col) = parse_path_and_line(raw_path);
        let mut locations = Vec::new();
        if let Some(rel) = resolve_import_specifier(&root_dir, &cur_rel, &clean, &all_files) {
            let abs = join_abs(&root_dir, &rel);
            let preview = read_text_file(&root_dir.join(&rel))
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
    let cur_text = req
        .content
        .clone()
        .or_else(|| (!cur_rel.is_empty()).then(|| read_text_file(&root_dir.join(&cur_rel))).flatten())
        .unwrap_or_default();

    let cur_lines = line_spans(&cur_text);
    let cur_line_text = cur_lines.get(req.line.saturating_sub(1)).copied().unwrap_or("");
    let on_import_line = is_import_line(cur_lang, cur_line_text.trim());
    let cur_defs = if !cur_text.is_empty() {
        extract_definitions(cur_lang, &cur_text)
    } else {
        Vec::new()
    };
    let local_defs = if !cur_text.is_empty() {
        extract_local_defs(cur_lang, &cur_text, symbol, req.line)
    } else {
        Vec::new()
    };

    let overlaps_cursor = |d: &IndexedDef| {
        d.name == symbol && d.line == req.line && req.col >= d.col && req.col <= d.col + d.len
    };
    let at_definition = !on_import_line && (cur_defs.iter().any(overlaps_cursor) || local_defs.iter().any(overlaps_cursor));

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

    // 1. Import-tracked target in current file (highest confidence)
    if !cur_text.is_empty() {
        match cur_lang {
            LangFamily::JsTs => {
                for imp in find_js_import_for_symbol(&cur_text, symbol, req.container.as_deref()) {
                    if let Some(target_rel) = resolve_import_specifier(&root_dir, &cur_rel, &imp.spec, &all_files) {
                        if let Some((final_rel, def)) =
                            resolve_js_reexport_chain(&root_dir, &target_rel, &imp.imported_name, &all_files, 2)
                        {
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
                        } else if imp.is_star || imp.imported_name == symbol {
                            // Imported module resolved, fallback to top of module if symbol wasn't explicitly extracted
                            let abs = join_abs(&root_dir, &target_rel);
                            let preview = read_text_file(&root_dir.join(&target_rel))
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
                                symbol: imp.imported_name,
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
                    if let Some(t) = read_text_file(&root_dir.join(&target_rel)) {
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
                    if let Some(t) = read_text_file(&root_dir.join(&target_rel)) {
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

        // 2. Same-file definitions & local bindings (excluding self when already on the definition site)
        for def in cur_defs.iter().filter(|d| d.name == symbol) {
            if at_definition && def.line == req.line {
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
            for def in local_defs {
                if at_definition && def.line == req.line {
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

    // 3. Workspace index matches
    let caller_is_test = is_test_path(&cur_rel);
    for (rel, lang, def) in query_index(&root_dir, symbol) {
        if at_definition && norm_cmp(&rel) == norm_cmp(&cur_rel) && def.line == req.line {
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

fn find_whole_word_cols(line: &str, symbol: &str) -> Vec<usize> {
    let mut cols = Vec::new();
    let mut from = 0usize;
    while let Some(pos) = line[from..].find(symbol) {
        let start = from + pos;
        let end = start + symbol.len();
        let before_ok = !line[..start].chars().next_back().is_some_and(is_word_char);
        let after_ok = !line[end..].chars().next().is_some_and(is_word_char);
        if before_ok && after_ok {
            cols.push(char_col(line, start));
        }
        from = start + symbol.chars().next().map_or(1, char::len_utf8);
    }
    cols
}

fn find_references(provider: &str, req: &RefRequest) -> Result<NavReferencesResult> {
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

    let cur_rel = normalize_rel_path(&root_dir, &req.path);
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
    let mut candidates: Vec<&String> = files
        .iter()
        .filter(|rel| !is_ignored_asset(rel) && filter(rel))
        .collect();
    candidates.sort_by(|a, b| {
        let a_cur = norm_cmp(a) == norm_cmp(cur_rel);
        let b_cur = norm_cmp(b) == norm_cmp(cur_rel);
        b_cur
            .cmp(&a_cur)
            .then_with(|| shared_dir_depth(b, cur_rel).cmp(&shared_dir_depth(a, cur_rel)))
            .then_with(|| a.cmp(b))
    });

    let started = Instant::now();
    let mut hits = Vec::new();
    let mut hit_files = HashSet::new();
    let mut truncated = false;
    let sym_chars = symbol.chars().count();

    'files: for rel in candidates {
        if started.elapsed() > std::time::Duration::from_secs(8) {
            truncated = true;
            break;
        }
        let Some(text) = read_text_file(&root_dir.join(rel)) else { continue };
        if !text.contains(symbol) {
            continue;
        }
        let lang = LangFamily::from_path(rel);
        let defs = extract_definitions(lang, &text);
        let def_lines: HashSet<usize> = defs.iter().filter(|d| d.name == symbol).map(|d| d.line).collect();
        let lines = line_spans(&text);
        let is_cur = !cur_rel.is_empty() && norm_cmp(rel) == norm_cmp(cur_rel);

        for (i, line) in lines.iter().enumerate() {
            let ln = i + 1;
            let cols = find_whole_word_cols(line, symbol);
            if cols.is_empty() {
                continue;
            }
            let is_def = def_lines.contains(&ln) && !is_import_line(lang, line.trim());
            if !req.include_declaration && is_cur && ln == req.line && is_def {
                continue;
            }
            let ctx_start = ln.saturating_sub(req.context_lines).max(1);
            let ctx_end = (ln + req.context_lines).min(lines.len());
            let context: Vec<String> = lines[ctx_start - 1..ctx_end].iter().map(|l| trunc_line(l, MAX_CONTEXT_CHARS)).collect();
            for col in cols {
                hits.push(NavReferenceHit {
                    path: rel.clone(),
                    abs_path: join_abs(root_dir, rel),
                    line: ln,
                    col,
                    len: sym_chars,
                    text: trunc_line(line, MAX_CONTEXT_CHARS),
                    is_definition: is_def,
                    context_start_line: ctx_start,
                    context: context.clone(),
                });
                hit_files.insert(rel.clone());
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
}
