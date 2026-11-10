//! Plugin packages: a `.yiplugin` file is a zip with `plugin.json`, the component (`main`, default `plugin.wasm`) and
//! optionally `prompt.md`, `LICENSE`, `README.md`. A plain directory with the same layout installs too (handy for
//! plugins built locally, e.g. by a Bot's task).
//!
//! Installing is two steps so people see what they grant: `stage` reads and checks a package, the frontend shows its
//! capabilities and permissions, then `install` writes it to `<app data>/plugins/<id>/`.

use super::wasm::{check_manifest, read_manifest};
use super::{Manifest, Permissions};
use anyhow::{anyhow, bail, Context as _, Result};
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};

const MAX_PACKAGE: u64 = 50 << 20;
const MAX_FILES: usize = 200;

/// A package read and checked, not installed yet
#[derive(Clone, Debug)]
pub struct Staged {
    pub manifest: Manifest,
    /// Relative path → content
    pub files: Vec<(String, Vec<u8>)>,
}

impl Staged {
    pub fn main(&self) -> Result<&[u8]> {
        let main = self.manifest.main.clone().unwrap_or_else(|| "plugin.wasm".into());
        self.files.iter().find(|(p, _)| *p == main).map(|(_, b)| b.as_slice()).ok_or_else(|| anyhow!("插件包里没有 {main}"))
    }
}

/// Check plugin.json and the component among loose files (relative path → content)
pub fn finish(files: Vec<(String, Vec<u8>)>) -> Result<Staged> {
    let raw = files.iter().find(|(p, _)| p == "plugin.json").map(|(_, b)| b.clone()).ok_or_else(|| anyhow!("插件包里没有 plugin.json"))?;
    let mut manifest: Manifest = serde_json::from_slice(&raw).map_err(|e| anyhow!("plugin.json 格式不对: {e}"))?;
    check_manifest(&manifest)?;
    if manifest.prompt.is_none() {
        if let Some((_, p)) = files.iter().find(|(p, _)| p == "prompt.md") {
            manifest.prompt = Some(String::from_utf8_lossy(p).trim().to_string()).filter(|p| !p.is_empty());
        }
    }
    let s = Staged { manifest, files };
    s.main()?;
    Ok(s)
}

/// Read a `.yiplugin` (zip)
pub fn from_zip(bytes: &[u8]) -> Result<Staged> {
    if bytes.len() as u64 > MAX_PACKAGE {
        bail!("插件包太大");
    }
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| anyhow!("不是有效的插件包（zip）: {e}"))?;
    if zip.len() > MAX_FILES {
        bail!("插件包里的文件太多");
    }
    let mut files = vec![];
    let mut total = 0u64;
    for i in 0..zip.len() {
        let mut f = zip.by_index(i)?;
        if f.is_dir() {
            continue;
        }
        let Some(name) = f.enclosed_name() else { bail!("插件包里有不安全的路径: {}", f.name()) };
        let name = name.to_string_lossy().replace('\\', "/");
        total += f.size();
        if total > MAX_PACKAGE {
            bail!("插件包解压后太大");
        }
        let mut buf = vec![];
        f.by_ref().take(MAX_PACKAGE).read_to_end(&mut buf)?;
        files.push((name, buf));
    }
    // A zip made of a folder: strip the single top-level directory
    if !files.iter().any(|(p, _)| p == "plugin.json") {
        if let Some((top, _)) = files.iter().find(|(p, _)| p.ends_with("/plugin.json") && p.matches('/').count() == 1) {
            let prefix = top.trim_end_matches("plugin.json").to_string();
            files = files.into_iter().filter_map(|(p, b)| p.strip_prefix(&prefix).map(|p| (p.to_string(), b))).collect();
        }
    }
    finish(files)
}

/// Read an unpacked plugin directory: plugin.json, the component, and the optional extras
pub fn from_dir(dir: &Path) -> Result<Staged> {
    let m = read_manifest(dir)?;
    let main = m.main.clone().unwrap_or_else(|| "plugin.wasm".into());
    let mut files = vec![("plugin.json".to_string(), std::fs::read(dir.join("plugin.json"))?)];
    files.push((main.clone(), std::fs::read(dir.join(&main)).with_context(|| format!("找不到 {main}"))?));
    for extra in ["prompt.md", "LICENSE", "README.md"] {
        if let Ok(b) = std::fs::read(dir.join(extra)) {
            files.push((extra.to_string(), b));
        }
    }
    finish(files)
}

/// A file path (package or directory), as given by the frontend
pub fn from_path(path: &Path) -> Result<Staged> {
    if path.is_dir() {
        from_dir(path)
    } else {
        let bytes = std::fs::read(path).with_context(|| format!("读取 {} 失败", path.display()))?;
        from_zip(&bytes)
    }
}

/// Download a package. Blocking: call from spawn_blocking
pub fn download(url: &str) -> Result<Vec<u8>> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        bail!("只支持 http(s) 链接");
    }
    let mut resp = ureq::get(url).call().map_err(|e| anyhow!("下载失败: {e}"))?;
    resp.body_mut().with_config().limit(MAX_PACKAGE).read_to_vec().map_err(|e| anyhow!("下载失败: {e}"))
}

/// Write a staged package to `<plugins>/<id>/`, replacing an installed version
pub fn install(s: &Staged, plugins: &Path) -> Result<PathBuf> {
    let dest = plugins.join(&s.manifest.id);
    let tmp = plugins.join(format!(".{}.new", s.manifest.id));
    let _ = std::fs::remove_dir_all(&tmp);
    for (rel, bytes) in &s.files {
        let p = tmp.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&p, bytes)?;
    }
    if dest.exists() {
        let old = plugins.join(format!(".{}.old", s.manifest.id));
        let _ = std::fs::remove_dir_all(&old);
        std::fs::rename(&dest, &old)?;
        let _ = std::fs::remove_dir_all(&old);
    }
    std::fs::rename(&tmp, &dest)?;
    Ok(dest)
}

/// What `new` asks for beyond `old`: an upgrade that adds permissions needs a new confirmation
pub fn added(old: &Permissions, new: &Permissions) -> Permissions {
    Permissions {
        net: new.net.iter().filter(|n| !old.net.contains(n)).cloned().collect(),
        exec: new.exec.iter().filter(|n| !old.exec.contains(n)).cloned().collect(),
    }
}

/// Build a `.yiplugin` from staged files (used by tests and plugins built by tasks)
pub fn pack(files: &[(String, Vec<u8>)]) -> Result<Vec<u8>> {
    use std::io::Write;
    let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    for (name, bytes) in files {
        w.start_file(name.as_str(), opts)?;
        w.write_all(bytes)?;
    }
    Ok(w.finish()?.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> Vec<u8> {
        br#"{"id":"demo","version":"1.0.0","api":"0.1","runtime":"wasm","name":{"zh":"demo","en":"Demo"},"capabilities":["trigger"],"permissions":{"net":["api.example.com"]}}"#.to_vec()
    }

    #[test]
    fn packages_round_trip_and_install() {
        let bytes = pack(&[("plugin.json".into(), manifest()), ("plugin.wasm".into(), b"\0asm".to_vec()), ("prompt.md".into(), b"Be nice".to_vec())]).unwrap();
        let s = from_zip(&bytes).unwrap();
        assert_eq!(s.manifest.id, "demo");
        assert_eq!(s.manifest.prompt.as_deref(), Some("Be nice"));
        let dir = tempfile::tempdir().unwrap();
        let p = install(&s, dir.path()).unwrap();
        assert!(p.join("plugin.wasm").is_file());
        // Upgrading replaces it
        install(&s, dir.path()).unwrap();
        let again = from_dir(&p).unwrap();
        assert_eq!(again.manifest.permissions.net, vec!["api.example.com".to_string()]);
    }

    #[test]
    fn bad_packages_are_refused() {
        assert!(from_zip(b"not a zip").is_err());
        let no_main = pack(&[("plugin.json".into(), manifest())]).unwrap();
        assert!(from_zip(&no_main).unwrap_err().to_string().contains("plugin.wasm"));
        // A folder zipped as a whole still works
        let nested = pack(&[("demo/plugin.json".into(), manifest()), ("demo/plugin.wasm".into(), b"\0asm".to_vec())]).unwrap();
        assert_eq!(from_zip(&nested).unwrap().manifest.id, "demo");
        let node = br#"{"id":"n","version":"1","api":"0.1","runtime":"node","name":{},"capabilities":[]}"#.to_vec();
        let p = pack(&[("plugin.json".into(), node), ("plugin.wasm".into(), vec![])]).unwrap();
        assert!(from_zip(&p).unwrap_err().to_string().contains("Node"));
    }

    #[test]
    fn added_permissions() {
        let old = Permissions { net: vec!["a.com".into()], exec: vec![] };
        let new = Permissions { net: vec!["a.com".into(), "b.com".into()], exec: vec!["gh".into()] };
        let a = added(&old, &new);
        assert_eq!(a.net, vec!["b.com".to_string()]);
        assert_eq!(a.exec, vec!["gh".to_string()]);
        assert!(added(&new, &old).net.is_empty());
    }
}
