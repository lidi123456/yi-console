fn main() {
    // QR pairing opens the camera in the page via getUserMedia (QrScanner.tsx); when the WebView needs the camera, wry asks the system for
    // the CAMERA permission, which is denied outright if the manifest doesn't declare it. gen/android is generated and not checked in, so we add it here:
    // during tauri android build, TAURI_ANDROID_PROJECT_PATH points to gen/android; running again replaces the same block.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("android") {
        println!("cargo:rerun-if-env-changed=TAURI_ANDROID_PROJECT_PATH");
        if let Some(dir) = std::env::var_os("TAURI_ANDROID_PROJECT_PATH") {
            // After re-running tauri android init the manifest is new, so the cached build-script result can't be reused
            let manifest = std::path::Path::new(&dir).join("app/src/main/AndroidManifest.xml");
            println!("cargo:rerun-if-changed={}", manifest.display());
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
    }
    tauri_build::build()
}
