fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let root = std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap())
            .join("../../runtime/computer-use/windows-x64");
        for name in ["cua-driver.exe", "LICENSE.md"] {
            let path = root.join(name);
            assert!(
                path.is_file(),
                "Run python scripts/prepare-computer-use.py before a Windows build (missing {})",
                path.display()
            );
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
    println!("cargo:rerun-if-changed=../../runtime/computer-use/manifest.json");
}
