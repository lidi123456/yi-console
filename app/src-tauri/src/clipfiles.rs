//! 读系统剪贴板里"复制的文件"的路径（资源管理器 / 访达 / 文件管理器里 Ctrl+C）。
//! WebView 的粘贴事件只给文件内容不给路径；本机会话里有了路径就直接 @引用原文件，不用上传一份。
//! 读不到时返回空列表，前端照常上传。

use std::path::Path;

/// 剪贴板里复制的文件（只要存在的普通文件）
#[tauri::command]
pub fn clipboard_files() -> Vec<String> {
    read().into_iter().filter(|p| Path::new(p).is_file()).collect()
}

#[cfg(windows)]
fn read() -> Vec<String> {
    // CF_HDROP：资源管理器复制文件时放的文件列表
    clipboard_win::get_clipboard::<Vec<String>, _>(clipboard_win::formats::FileList).unwrap_or_default()
}

#[cfg(target_os = "macos")]
fn read() -> Vec<String> {
    // 访达复制文件时剪贴板里有 NSFilenamesPboardType（路径数组）
    let script = "ObjC.import('AppKit');\
        var r = ObjC.deepUnwrap($.NSPasteboard.generalPasteboard.propertyListForType('NSFilenamesPboardType'));\
        Array.isArray(r) ? r.join('\\n') : ''";
    std::process::Command::new("osascript")
        .args(["-l", "JavaScript", "-e", script])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).lines().map(str::to_string).filter(|l| !l.is_empty()).collect())
        .unwrap_or_default()
}

#[cfg(all(unix, not(target_os = "macos"), not(target_os = "android"), not(target_os = "ios")))]
fn read() -> Vec<String> {
    // 文件管理器复制文件时放 text/uri-list（file:// 链接，一行一个）
    let tries: [(&str, &[&str]); 2] = [
        ("wl-paste", &["--no-newline", "--type", "text/uri-list"]),
        ("xclip", &["-selection", "clipboard", "-t", "text/uri-list", "-o"]),
    ];
    for (cmd, args) in tries {
        if cmd == "wl-paste" && std::env::var_os("WAYLAND_DISPLAY").is_none() {
            continue;
        }
        let Ok(o) = std::process::Command::new(cmd).args(args).output() else { continue };
        if !o.status.success() {
            continue;
        }
        let list: Vec<String> = String::from_utf8_lossy(&o.stdout)
            .lines()
            .filter_map(|l| l.trim().strip_prefix("file://"))
            .map(|p| unpct(p.strip_prefix("localhost").unwrap_or(p)))
            .collect();
        if !list.is_empty() {
            return list;
        }
    }
    vec![]
}

#[cfg(any(target_os = "android", target_os = "ios"))]
fn read() -> Vec<String> {
    vec![]
}

/// file:// 链接里的 %XX 转回原字符
#[allow(dead_code)]
fn unpct(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Some(v) = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) {
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

#[cfg(test)]
mod tests {
    /// 手动验证：先在系统里复制几个文件，再 cargo test -- --ignored --nocapture clipboard_now
    #[test]
    #[ignore]
    fn clipboard_now() {
        println!("CLIPBOARD_FILES={:?}", super::clipboard_files());
    }

    #[test]
    fn unpct() {
        assert_eq!(super::unpct("/home/me/a%20b.txt"), "/home/me/a b.txt");
        assert_eq!(super::unpct("/tmp/%E4%B8%AD.md"), "/tmp/中.md");
        assert_eq!(super::unpct("/tmp/100%"), "/tmp/100%");
        assert_eq!(super::unpct("/tmp/x%2"), "/tmp/x%2");
    }
}
