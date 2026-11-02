//! 网页预览的文件服务：浏览器标签里预览主机上的 HTML 时，页面和它用相对路径引用的 css、js、图片
//! 都要能按 URL 取到。前端先用 `preview.root` 给一个目录换一个随机令牌，之后
//! `<令牌>/<相对路径>` 就映射到主机上这个目录里的文件，经 `fs.read`（raw）读出来。
//!
//! Tauri 里挂在自定义协议 `ahfs` 上，yi-web 里挂在 `/fsv/` 下。令牌只能读它那个目录（不允许 ..，
//! 指到目录外的符号链接由 yi-agentd 拒绝），
//! 预览页放在没有 allow-same-origin 的 sandbox iframe 里，碰不到应用本身。

use anyhow::{anyhow, bail, Result};
use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Default)]
pub struct Previews {
    /// 令牌 -> (主机 id, 根目录)
    roots: Mutex<HashMap<String, (String, String)>>,
}

impl Previews {
    /// 同一个主机 + 目录复用同一个令牌，地址稳定，浏览器缓存也能用上
    pub fn token(&self, host: &str, root: &str) -> String {
        let mut m = self.roots.lock().unwrap();
        if let Some((t, _)) = m.iter().find(|(_, v)| v.0 == host && v.1 == root) {
            return t.clone();
        }
        let t = uuid::Uuid::new_v4().simple().to_string();
        m.insert(t.clone(), (host.into(), root.into()));
        t
    }

    /// `<令牌>/<相对路径>`（URL 编码过）-> (主机 id, 根目录, 文件绝对路径)。
    /// 这里只能挡住 ..，符号链接要到主机上才看得出指向哪里，由 yi-agentd 读的时候按根目录检查
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

/// %xx 解码（不把 + 当空格，路径里 + 就是 +）
pub fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        // 按字节取两位十六进制，不切 &str（后面是多字节字符时会 panic）
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
        // % 后面跟中文不能 panic
        assert_eq!(pct_decode("%a中"), "%a中");
        assert!(p.resolve(&format!("{t}/%中.html")).is_ok());
    }
}
