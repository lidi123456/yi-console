//! Reads the paths of "copied files" from the system clipboard (Ctrl+C in Explorer / Finder / a file manager).
//! The WebView paste event only gives file contents, not paths; with the path, a local session can @-reference the original file instead of uploading a copy.
//! Returns an empty list when nothing can be read, and the frontend uploads as usual.

use std::path::Path;

/// Files copied to the clipboard (existing regular files only)
#[tauri::command]
pub fn clipboard_files() -> Vec<String> {
    read().into_iter().filter(|p| Path::new(p).is_file()).collect()
}

#[cfg(windows)]
fn read() -> Vec<String> {
    // CF_HDROP: the file list Explorer puts on the clipboard when copying files
    clipboard_win::get_clipboard::<Vec<String>, _>(clipboard_win::formats::FileList).unwrap_or_default()
}

#[cfg(target_os = "macos")]
fn read() -> Vec<String> {
    // When Finder copies files the clipboard holds NSFilenamesPboardType (an array of paths)
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
    // File managers put text/uri-list (file:// links, one per line) when copying files
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

/// Decode %XX escapes in a file:// link
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
    /// Manual check: copy a few files in the system first, then cargo test -- --ignored --nocapture clipboard_now
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
