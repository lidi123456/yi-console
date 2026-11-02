//! File server for web previews: when a browser tab previews HTML on a host, the page and the css, js and images it references by relative path
//! must all be fetchable by URL. The frontend first uses `preview.root` to exchange a directory for a random token; after that
//! `<token>/<relative path>` maps to files in that directory on the host, read through `fs.read` (raw).
//!
//! In Tauri it's served on the custom protocol `ahfs`, in yi-web under `/fsv/`. A token can only read its own directory (no ..;
//! symlinks pointing outside are rejected by yi-agentd),
//! and the preview page sits in a sandboxed iframe without allow-same-origin, so it can't touch the app itself.

use anyhow::{anyhow, bail, Result};
use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Default)]
pub struct Previews {
    /// token -> (host id, root directory)
    roots: Mutex<HashMap<String, (String, String)>>,
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

    /// `<token>/<relative path>` (URL-encoded) -> (host id, root directory, absolute file path).
    /// Only .. can be blocked here; where a symlink points is only known on the host, so yi-agentd checks against the root when reading
    pub fn resolve(&self, url_path: &str) -> Result<(String, String, String)> {
        let p = url_path.split(['?', '#']).next().unwrap_or("").trim_start_matches('/');
        let (tok, rest) = p.split_once('/').unwrap_or((p, ""));
        let (host, root) = self.roots.lock().unwrap().get(tok).cloned().ok_or_else(|| anyhow!("预览地址已失效"))?;
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
        Ok((host, root, path))
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
        assert_eq!(p.resolve(&format!("{t}/css/a%20b.css?v=1")).unwrap(), ("h".into(), "/repo/site".into(), "/repo/site/css/a b.css".into()));
        assert_eq!(p.resolve(&format!("/{t}/")).unwrap().2, "/repo/site/index.html");
        assert_eq!(p.resolve(&format!("{t}/a+b%E4%B8%AD.html")).unwrap().2, "/repo/site/a+b中.html");
        assert!(p.resolve(&format!("{t}/../etc/passwd")).is_err());
        assert!(p.resolve(&format!("{t}/%2e%2e/x")).is_err());
        assert!(p.resolve(&format!("{t}/a%2F..%2Fb")).is_err());
        assert!(p.resolve("nope/x.html").is_err());
        let w = p.token("h", r"C:\site");
        assert_eq!(p.resolve(&format!("{w}/js/app.js")).unwrap().2, r"C:\site\js\app.js");
        assert!(p.resolve(&format!("{w}/C%3A/x")).is_err());
        // % followed by multi-byte (CJK) characters must not panic
        assert_eq!(pct_decode("%a中"), "%a中");
        assert!(p.resolve(&format!("{t}/%中.html")).is_ok());
    }
}
