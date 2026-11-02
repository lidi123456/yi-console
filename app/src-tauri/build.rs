fn main() {
    // 扫码配对在网页里用 getUserMedia 开相机（QrScanner.tsx），WebView 要相机时 wry 会向系统申请
    // CAMERA 权限，清单里没声明就直接拒绝。gen/android 是生成的、不进仓库，所以在这里写进去：
    // tauri android build 时 TAURI_ANDROID_PROJECT_PATH 指向 gen/android，重复执行会替换同一块。
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("android") {
        println!("cargo:rerun-if-env-changed=TAURI_ANDROID_PROJECT_PATH");
        if let Some(dir) = std::env::var_os("TAURI_ANDROID_PROJECT_PATH") {
            // 重新 tauri android init 后清单是新的，build 脚本的缓存结果不能再用
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
