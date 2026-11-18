//! Key presses through `Input.dispatchKeyEvent`: names as in Playwright / agent-browser (`Enter`, `Control+a`).

use crate::browser::{Browser, Tab};
use anyhow::{bail, Result};
use serde_json::json;

pub(crate) struct Key {
    pub key: String,
    pub code: String,
    pub key_code: u32,
    pub text: Option<String>,
}

pub(crate) fn key(name: &str) -> Option<Key> {
    let k = |key: &str, code: &str, kc: u32, text: Option<&str>| Some(Key { key: key.into(), code: code.into(), key_code: kc, text: text.map(String::from) });
    match name {
        "Enter" | "Return" => k("Enter", "Enter", 13, Some("\r")),
        "Tab" => k("Tab", "Tab", 9, None),
        "Escape" | "Esc" => k("Escape", "Escape", 27, None),
        "Backspace" => k("Backspace", "Backspace", 8, None),
        "Delete" => k("Delete", "Delete", 46, None),
        "Space" | " " => k(" ", "Space", 32, Some(" ")),
        "ArrowUp" | "Up" => k("ArrowUp", "ArrowUp", 38, None),
        "ArrowDown" | "Down" => k("ArrowDown", "ArrowDown", 40, None),
        "ArrowLeft" | "Left" => k("ArrowLeft", "ArrowLeft", 37, None),
        "ArrowRight" | "Right" => k("ArrowRight", "ArrowRight", 39, None),
        "Home" => k("Home", "Home", 36, None),
        "End" => k("End", "End", 35, None),
        "PageUp" => k("PageUp", "PageUp", 33, None),
        "PageDown" => k("PageDown", "PageDown", 34, None),
        "Insert" => k("Insert", "Insert", 45, None),
        _ => {
            if let Some(n) = name.strip_prefix('F').and_then(|n| n.parse::<u32>().ok()).filter(|n| (1..=12).contains(n)) {
                return Some(Key { key: name.into(), code: name.into(), key_code: 111 + n, text: None });
            }
            let mut chars = name.chars();
            let c = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            Some(char_key(c))
        }
    }
}

fn char_key(c: char) -> Key {
    let (code, kc) = if c.is_ascii_alphabetic() {
        (format!("Key{}", c.to_ascii_uppercase()), c.to_ascii_uppercase() as u32)
    } else if c.is_ascii_digit() {
        (format!("Digit{c}"), c as u32)
    } else {
        let (code, kc) = match c {
            '-' => ("Minus", 189),
            '=' => ("Equal", 187),
            ',' => ("Comma", 188),
            '.' => ("Period", 190),
            '/' => ("Slash", 191),
            ';' => ("Semicolon", 186),
            '\'' => ("Quote", 222),
            '[' => ("BracketLeft", 219),
            ']' => ("BracketRight", 221),
            '\\' => ("Backslash", 220),
            '`' => ("Backquote", 192),
            _ => ("", 0),
        };
        (code.to_string(), kc)
    };
    Key { key: c.to_string(), code, key_code: kc, text: Some(c.to_string()) }
}

/// "Control+Shift+a" -> (modifier bits, key). Alt=1, Control=2, Meta=4, Shift=8
pub(crate) fn parse(combo: &str) -> Result<(u32, Key)> {
    let parts: Vec<&str> = if combo == "+" { vec!["+"] } else { combo.split('+').map(str::trim).collect() };
    let (last, mods) = parts.split_last().ok_or_else(|| anyhow::anyhow!("缺少按键"))?;
    let mut bits = 0;
    for m in mods {
        bits |= match m.to_ascii_lowercase().as_str() {
            "alt" | "option" => 1,
            "control" | "ctrl" => 2,
            "meta" | "cmd" | "command" | "super" => 4,
            "shift" => 8,
            other => bail!("不认识的修饰键 {other}"),
        };
    }
    let k = match key(last) {
        Some(k) => k,
        None => bail!("不认识的按键 {last}"),
    };
    Ok((bits, k))
}

async fn send(b: &Browser, tab: &Tab, kind: &str, k: &Key, mods: u32, with_text: bool) -> Result<bool> {
    let mut p = json!({"type": kind, "modifiers": mods, "key": k.key, "code": k.code, "windowsVirtualKeyCode": k.key_code});
    if with_text {
        if let Some(t) = &k.text {
            p["text"] = json!(t);
            p["unmodifiedText"] = json!(t);
        }
    }
    b.input(tab, "Input.dispatchKeyEvent", p).await
}

/// Press a combination. Returns true when a dialog opened
pub(crate) async fn press(b: &Browser, tab: &Tab, combo: &str) -> Result<bool> {
    let (mods, k) = parse(combo)?;
    // Shortcuts with Control/Meta type no text
    let text = mods & (2 | 4) == 0 && k.text.is_some();
    if send(b, tab, if text { "keyDown" } else { "rawKeyDown" }, &k, mods, text).await? {
        return Ok(true);
    }
    send(b, tab, "keyUp", &k, mods, false).await
}

/// Type one character: keys that exist on a keyboard as key events, everything else (CJK, emoji) as inserted text
pub(crate) async fn type_char(b: &Browser, tab: &Tab, c: char) -> Result<bool> {
    if c == '\n' {
        return press(b, tab, "Enter").await;
    }
    if c.is_ascii_graphic() || c == ' ' {
        let mut k = char_key(c);
        if c == ' ' {
            k.code = "Space".into();
            k.key_code = 32;
        }
        if send(b, tab, "keyDown", &k, 0, true).await? {
            return Ok(true);
        }
        return send(b, tab, "keyUp", &k, 0, false).await;
    }
    b.cdp().call(Some(&tab.session), "Input.insertText", json!({"text": c.to_string()})).await?;
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combos() {
        let (m, k) = parse("Control+a").unwrap();
        assert_eq!((m, k.key.as_str(), k.code.as_str(), k.key_code), (2, "a", "KeyA", 65));
        let (m, k) = parse("Shift+Tab").unwrap();
        assert_eq!((m, k.key.as_str()), (8, "Tab"));
        let (_, k) = parse("Enter").unwrap();
        assert_eq!(k.text.as_deref(), Some("\r"));
        assert_eq!(parse("F5").unwrap().1.key_code, 116);
        assert_eq!(parse("+").unwrap().1.key, "+");
        assert!(parse("Hyper+x").is_err());
        assert!(parse("Foo").is_err());
    }
}
