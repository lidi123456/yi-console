use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 不引入 chrono，只为日志和日期分组手写一个 UTC 格式化
pub fn now_rfc3339() -> String {
    let ms = now_ms();
    let (y, mo, d, h, mi, s) = civil(ms / 1000);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{:03}Z", ms % 1000)
}

/// 毫秒时间戳 -> "YYYY-MM-DD"（按给定时区偏移分钟）
pub fn day_of(ms: i64, tz_offset_min: i64) -> String {
    let (y, mo, d, ..) = civil(ms / 1000 + tz_offset_min * 60);
    format!("{y:04}-{mo:02}-{d:02}")
}

fn civil(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    // Howard Hinnant 的 civil_from_days
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (
        y,
        m,
        d,
        (rem / 3600) as u32,
        (rem % 3600 / 60) as u32,
        (rem % 60) as u32,
    )
}

/// 解析 RFC3339 / ISO8601 时间（只处理 UTC 的 `Z` 和 `+hh:mm` 偏移），失败返回 None
pub fn parse_iso(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> { s.get(r)?.parse().ok() };
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    let mut i = 19;
    let mut ms = 0i64;
    if b.get(i) == Some(&b'.') {
        let start = i + 1;
        i = start;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        let frac = &s[start..i];
        let frac3: String = frac.chars().chain("000".chars()).take(3).collect();
        ms = frac3.parse().unwrap_or(0);
    }
    let mut offset = 0i64;
    if let Some(&c) = b.get(i) {
        if c == b'+' || c == b'-' {
            let oh = num(i + 1..i + 3)?;
            let om = num(i + 4..i + 6).unwrap_or(0);
            offset = (oh * 60 + om) * if c == b'+' { 1 } else { -1 };
        }
    }
    let m = if mo <= 2 { mo + 9 } else { mo - 3 };
    let yy = if mo <= 2 { y - 1 } else { y };
    let era = yy.div_euclid(400);
    let yoe = yy - era * 400;
    let doy = (153 * m + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(((days * 86400 + h * 3600 + mi * 60 + sec) - offset * 60) * 1000 + ms)
}

/// 含密钥的文件只给自己读写
pub fn restrict_file(p: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    let _ = p;
}

/// 截断过长的字符串（按字符边界）
pub fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_roundtrip() {
        assert_eq!(parse_iso("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso("2026-10-27T12:38:04.012Z"), Some(1793104684012));
        assert_eq!(
            parse_iso("2026-10-27T20:38:04.012+08:00"),
            Some(1793104684012)
        );
        assert_eq!(day_of(1793104684012, 0), "2026-10-27");
        assert_eq!(day_of(1793104684012, 12 * 60), "2026-10-28");
    }

    #[test]
    fn truncate_cjk() {
        assert_eq!(truncate("你好世界", 7), "你好…");
    }
}
