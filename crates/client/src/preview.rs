//! File server for web previews: when a browser tab previews HTML on a host, the page and the css, js and images it references by relative path
//! must all be fetchable by URL. The frontend first uses `preview.root` to exchange a directory for a random token; after that
//! `<token>/<relative path>` maps to files in that directory on the host, read through `fs.read` (raw).
//!
//! In Tauri it's served on the custom protocol `ahfs`, in yi-web under `/fsv/`. A token can only read its own directory (no ..;
//! symlinks pointing outside are rejected by yi-agentd),
//! and the preview page sits in a sandboxed iframe without allow-same-origin, so it can't touch the app itself.

use anyhow::{bail, Result};
use std::collections::HashMap;
use std::sync::Mutex;

const MAX_FILE_TOKENS: usize = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileIdentity {
    pub dev: u64,
    pub ino: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedPreview {
    pub host: String,
    pub root: Option<String>,
    pub path: String,
    pub token: String,
    pub is_file_token: bool,
    pub identity: Option<FileIdentity>,
    pub max_bytes: Option<u64>,
}

struct FileEntry {
    token: String,
    host: String,
    path: String,
    identity: Option<FileIdentity>,
    max_bytes: u64,
}

#[derive(Default)]
pub struct Previews {
    /// token -> (host id, root directory)
    roots: Mutex<HashMap<String, (String, String)>>,
    /// Bounded LRU list of single-file tokens (for direct PDF/image streaming without directory exposure)
    files: Mutex<Vec<FileEntry>>,
}

impl Previews {
    /// The same host + directory reuses one token, so URLs are stable and the browser cache works
    pub fn token(&self, host: &str, root: &str) -> String {
        let mut m = self.roots.lock().unwrap();
        if let Some((t, _)) = m.iter().find(|(_, v)| v.0 == host && v.1 == root) {
            return t.clone();
        }
        let t = uuid::Uuid::new_v4().simple().to_string();
        m.insert(t.clone(), (host.into(), root.into()));
        t
    }

    /// Issue (or reuse) a single-file preview token in a bounded 128-entry LRU list
    pub fn file_token(&self, host: &str, path: &str, identity: Option<FileIdentity>, max_bytes: u64) -> String {
        let mut list = self.files.lock().unwrap();
        if let Some(pos) = list.iter().position(|e| e.host == host && e.path == path) {
            let entry = list.remove(pos);
            if entry.identity == identity && entry.max_bytes == max_bytes {
                let tok = entry.token.clone();
                list.push(entry);
                return tok;
            }
        }
        if list.len() >= MAX_FILE_TOKENS {
            list.remove(0);
        }
        let token = uuid::Uuid::new_v4().simple().to_string();
        list.push(FileEntry {
            token: token.clone(),
            host: host.into(),
            path: path.into(),
            identity,
            max_bytes,
        });
        token
    }

    /// Drop a single-file preview token when its underlying file identity no longer matches
    pub fn invalidate_file_token(&self, token: &str) {
        let mut list = self.files.lock().unwrap();
        list.retain(|e| e.token != token);
    }

    /// `<token>/<relative path>` (URL-encoded) -> resolved target info.
    /// Only .. can be blocked here for root tokens; symlink escapes are checked on the host (or locally when Local).
    pub fn resolve(&self, url_path: &str) -> Result<ResolvedPreview> {
        let p = url_path.split(['?', '#']).next().unwrap_or("").trim_start_matches('/');
        let (tok, rest) = p.split_once('/').unwrap_or((p, ""));
        if let Some((host, root)) = self.roots.lock().unwrap().get(tok).cloned() {
            let mut segs = Vec::new();
            for s in rest.split('/') {
                let s = pct_decode(s);
                if s.is_empty() || s == "." {
                    continue;
                }
                if s == ".." || s.contains(['/', '\\']) || s.contains(':') {
                    bail!("路径无效");
                }
                segs.push(s);
            }
            if rest.is_empty() || rest.ends_with('/') {
                segs.push("index.html".into());
            }
            let sep = if root.contains('\\') && !root.contains('/') { "\\" } else { "/" };
            let base = root.trim_end_matches(['/', '\\']);
            let path = format!("{base}{sep}{}", segs.join(sep));
            return Ok(ResolvedPreview {
                host,
                root: Some(root),
                path,
                token: tok.into(),
                is_file_token: false,
                identity: None,
                max_bytes: None,
            });
        }
        let mut files = self.files.lock().unwrap();
        if let Some(pos) = files.iter().position(|e| e.token == tok) {
            for s in rest.split('/') {
                let s = pct_decode(s);
                if s == ".." || s.contains(['/', '\\']) {
                    bail!("路径无效");
                }
            }
            let entry = files.remove(pos);
            let out = ResolvedPreview {
                host: entry.host.clone(),
                root: None,
                path: entry.path.clone(),
                token: entry.token.clone(),
                is_file_token: true,
                identity: entry.identity,
                max_bytes: Some(entry.max_bytes),
            };
            files.push(entry);
            return Ok(out);
        }
        bail!("预览地址已失效")
    }
}

/// %xx decoding (+ is not a space; in paths + is just +)
pub fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        // Take two hex digits byte-wise rather than slicing &str (which would panic before a multi-byte character)
        if b[i] == b'%' {
            if let Some(v) = b.get(i + 1..i + 3).and_then(yi_link::hex_byte) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn mime(path: &str) -> &'static str {
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" | "cjs" => "text/javascript; charset=utf-8",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "bmp" => "image/bmp",
        "avif" => "image/avif",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "wasm" => "application/wasm",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "pdf" => "application/pdf",
        "txt" | "md" | "csv" | "log" => "text/plain; charset=utf-8",
        "xml" => "application/xml",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_inside_root_only() {
        let p = Previews::default();
        let t = p.token("h", "/repo/site");
        assert_eq!(p.token("h", "/repo/site"), t);
        let r1 = p.resolve(&format!("{t}/css/a%20b.css?v=1")).unwrap();
        assert_eq!((r1.host.as_str(), r1.root.as_deref(), r1.path.as_str(), r1.is_file_token), ("h", Some("/repo/site"), "/repo/site/css/a b.css", false));
        assert_eq!(p.resolve(&format!("/{t}/")).unwrap().path, "/repo/site/index.html");
        assert_eq!(p.resolve(&format!("{t}/a+b%E4%B8%AD.html")).unwrap().path, "/repo/site/a+b中.html");
        assert!(p.resolve(&format!("{t}/../etc/passwd")).is_err());
        assert!(p.resolve(&format!("{t}/%2e%2e/x")).is_err());
        assert!(p.resolve(&format!("{t}/a%2F..%2Fb")).is_err());
        assert!(p.resolve("nope/x.html").is_err());
        let w = p.token("h", r"C:\site");
        assert_eq!(p.resolve(&format!("{w}/js/app.js")).unwrap().path, r"C:\site\js\app.js");
        assert!(p.resolve(&format!("{w}/C%3A/x")).is_err());
        // % followed by multi-byte (CJK) characters must not panic
        assert_eq!(pct_decode("%a中"), "%a中");
        assert!(p.resolve(&format!("{t}/%中.html")).is_ok());
    }

    #[test]
    fn resolves_file_tokens_and_evicts_lru() {
        let p = Previews::default();
        let id0 = FileIdentity { dev: 1, ino: 100 };
        let id0_replaced = FileIdentity { dev: 1, ino: 200 };
        let first = p.file_token("local", "/tmp/file-0.pdf", Some(id0), 50 * 1024 * 1024);
        assert_eq!(p.file_token("local", "/tmp/file-0.pdf", Some(id0), 50 * 1024 * 1024), first);
        let resolved = p.resolve(&format!("{first}/file-0.pdf?v=1")).unwrap();
        assert_eq!(
            (resolved.host.as_str(), resolved.root.as_deref(), resolved.path.as_str(), resolved.is_file_token, resolved.identity, resolved.max_bytes),
            ("local", None, "/tmp/file-0.pdf", true, Some(id0), Some(50 * 1024 * 1024))
        );

        let replaced = p.file_token("local", "/tmp/file-0.pdf", Some(id0_replaced), 50 * 1024 * 1024);
        assert_ne!(replaced, first);
        assert!(p.resolve(&format!("{first}/file-0.pdf")).is_err());
        assert_eq!(p.resolve(&format!("{replaced}/file-0.pdf")).unwrap().identity, Some(id0_replaced));

        let temp = p.file_token("local", "/tmp/temp.pdf", Some(FileIdentity { dev: 1, ino: 300 }), 1024);
        p.invalidate_file_token(&temp);
        assert!(p.resolve(&format!("{temp}/temp.pdf")).is_err());

        for i in 1..=MAX_FILE_TOKENS {
            p.file_token("local", &format!("/tmp/file-{i}.pdf"), Some(FileIdentity { dev: 1, ino: 1000 + i as u64 }), 50 * 1024 * 1024);
        }
        // Replaced token for file-0 should now be evicted
        assert!(p.resolve(&format!("{replaced}/file-0.pdf")).is_err());
    }
}
