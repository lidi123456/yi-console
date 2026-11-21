fn main() {
    // QR pairing opens the camera in the page via getUserMedia (QrScanner.tsx); when the WebView needs the camera, wry asks the system for
    // the CAMERA permission, which is denied outright if the manifest doesn't declare it. gen/android is generated and not checked in, so we add it here:
    // during tauri android build, TAURI_ANDROID_PROJECT_PATH points to gen/android; running again replaces the same block.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("android") {
        println!("cargo:rerun-if-env-changed=TAURI_ANDROID_PROJECT_PATH");
        if let Some(dir) = std::env::var_os("TAURI_ANDROID_PROJECT_PATH") {
            let root = std::path::Path::new(&dir);
            // After re-running tauri android init the manifest is new, so the cached build-script result can't be reused
            let manifest = root.join("app/src/main/AndroidManifest.xml");
            println!("cargo:rerun-if-changed={}", manifest.display());

            // Allow cleartext http:// traffic in the in-app WebView (e.g. forwarded localhost dev servers and internal HTTP links)
            let xml_dir = root.join("app/src/main/res/xml");
            let nsc_path = xml_dir.join("network_security_config.xml");
            println!("cargo:rerun-if-changed={}", nsc_path.display());
            let nsc_xml = concat!(
                "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n",
                "<network-security-config>\n",
                "  <base-config cleartextTrafficPermitted=\"true\" />\n",
                "</network-security-config>\n",
            );
            if std::fs::read_to_string(&nsc_path).ok().as_deref() != Some(nsc_xml) {
                std::fs::create_dir_all(&xml_dir).expect("创建 res/xml 目录失败");
                std::fs::write(&nsc_path, nsc_xml).expect("写入 network_security_config.xml 失败");
            }
        }
        tauri_utils::build::update_android_manifest(
            "yi-console-camera",
            "manifest",
            [
                r#"<uses-permission android:name="android.permission.CAMERA" />"#,
                r#"<uses-feature android:name="android.hardware.camera.any" android:required="false" />"#,
            ]
            .join("\n"),
        )
        .expect("更新 AndroidManifest.xml 失败");
        // Self-update (MainActivity.kt) hands the downloaded APK to the system installer, which needs this permission
        tauri_utils::build::update_android_manifest(
            "yi-console-update",
            "manifest",
            r#"<uses-permission android:name="android.permission.REQUEST_INSTALL_PACKAGES" />"#.to_string(),
        )
        .expect("更新 AndroidManifest.xml 失败");

        if let Some(dir) = std::env::var_os("TAURI_ANDROID_PROJECT_PATH") {
            let manifest_path = std::path::Path::new(&dir).join("app/src/main/AndroidManifest.xml");
            if manifest_path.exists() {
                let manifest = std::fs::read_to_string(&manifest_path).expect("读取 AndroidManifest.xml 失败");
                if !manifest.contains("android:networkSecurityConfig=") {
                    if let Some(app_start) = manifest.find("<application") {
                        if let Some(rel_end) = manifest[app_start..].find('>') {
                            let tag_end = app_start + rel_end;
                            let mut patched = String::with_capacity(manifest.len() + 80);
                            patched.push_str(&manifest[..tag_end]);
                            patched.push_str("\n        android:networkSecurityConfig=\"@xml/network_security_config\"");
                            patched.push_str(&manifest[tag_end..]);
                            std::fs::write(&manifest_path, patched).expect("写入 AndroidManifest.xml 失败");
                        }
                    }
                }
            }
        }
    }
    tauri_build::build()
}
