//! Packs plugins built for wasm32-wasip2 into installable form.
//!
//! ```text
//! cargo build --release --target wasm32-wasip2 -p yi-plugin-github
//! cargo run -p yi-plugin-pack -- github [feishu ...]
//! ```
//!
//! For each plugin directory it copies `plugin.json`, `prompt.md`, `README*`, `icon.*` and the built component
//! (named after the manifest's `main`) into `dist/<id>/`, and zips the same files into `dist/<id>-<version>.yiplugin`.
//! The wasm is looked up in `$CARGO_TARGET_DIR` (or `target/`) under `wasm32-wasip2/release/<crate name>.wasm`.

use std::io::Write;
use std::path::{Path, PathBuf};

fn main() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: yi-plugin-pack <plugin dir>...");
        std::process::exit(2);
    }
    let target = std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from).unwrap_or_else(|| root.join("target"));
    let dist = root.join("dist");
    let mut failed = false;
    for a in &args {
        let dir = if Path::new(a).is_absolute() { PathBuf::from(a) } else { root.join(a) };
        match pack(&dir, &target, &dist) {
            Ok(out) => println!("packed {}", out.display()),
            Err(e) => {
                eprintln!("{a}: {e}");
                failed = true;
            }
        }
    }
    if failed {
        std::process::exit(1);
    }
}

fn crate_name(dir: &Path) -> Result<String, String> {
    let toml = std::fs::read_to_string(dir.join("Cargo.toml")).map_err(|e| format!("Cargo.toml: {e}"))?;
    toml.lines()
        .find_map(|l| {
            let (k, v) = l.split_once('=')?;
            (k.trim() == "name").then(|| v.trim().trim_matches('"').to_string())
        })
        .ok_or_else(|| "no package name in Cargo.toml".into())
}

fn pack(dir: &Path, target: &Path, dist: &Path) -> Result<PathBuf, String> {
    let manifest_text = std::fs::read_to_string(dir.join("plugin.json")).map_err(|e| format!("plugin.json: {e}"))?;
    let m: serde_json::Value = serde_json::from_str(&manifest_text).map_err(|e| format!("plugin.json: {e}"))?;
    let id = m["id"].as_str().ok_or("plugin.json has no id")?;
    let version = m["version"].as_str().ok_or("plugin.json has no version")?;
    let main = m["main"].as_str().unwrap_or("plugin.wasm");
    let wasm = target.join("wasm32-wasip2").join("release").join(format!("{}.wasm", crate_name(dir)?.replace('-', "_")));
    let wasm_bytes = std::fs::read(&wasm).map_err(|e| format!("{}: {e} (build it first)", wasm.display()))?;

    let mut files: Vec<(String, Vec<u8>)> = vec![("plugin.json".into(), manifest_text.into_bytes()), (main.into(), wasm_bytes)];
    for entry in std::fs::read_dir(dir).map_err(|e| e.to_string())?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == "prompt.md" || name.starts_with("README") || name.starts_with("icon.") {
            files.push((name, std::fs::read(entry.path()).map_err(|e| e.to_string())?));
        }
    }

    let out_dir = dist.join(id);
    let _ = std::fs::remove_dir_all(&out_dir);
    std::fs::create_dir_all(&out_dir).map_err(|e| e.to_string())?;
    for (name, bytes) in &files {
        let p = out_dir.join(name);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(&p, bytes).map_err(|e| e.to_string())?;
    }

    let zip_path = dist.join(format!("{id}-{version}.yiplugin"));
    let f = std::fs::File::create(&zip_path).map_err(|e| e.to_string())?;
    let mut z = zip::ZipWriter::new(f);
    let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    for (name, bytes) in &files {
        z.start_file(name.as_str(), opts).map_err(|e| e.to_string())?;
        z.write_all(bytes).map_err(|e| e.to_string())?;
    }
    z.finish().map_err(|e| e.to_string())?;
    Ok(zip_path)
}
