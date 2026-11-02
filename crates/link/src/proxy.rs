//! Connect to the relay through an HTTP proxy (CONNECT tunnel). Corporate networks often only get out through a proxy.
//!
//! In auto mode (default) the proxy is looked up in order: `HTTPS_PROXY` (wss) / `HTTP_PROXY` (ws) → `ALL_PROXY`
//! → Windows system proxy settings; hosts listed in `NO_PROXY` connect directly. Clients can also use [`set_mode`]
//! to disable the proxy or set one manually. Only http:// proxies are supported; socks proxies are ignored and the connection goes direct.

use anyhow::{anyhow, bail, Context, Result};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use std::sync::RwLock;
use tokio::net::TcpStream;

/// Proxy settings (process-wide). yi-agentd always uses Auto; desktop / mobile clients follow the user's setting
#[derive(Debug, Clone, PartialEq)]
pub enum Mode {
    Auto,
    Off,
    /// e.g. `http://127.0.0.1:7897`
    Manual(String),
}

static MODE: RwLock<Option<Mode>> = RwLock::new(None);

pub fn set_mode(m: Mode) {
    *MODE.write().unwrap() = Some(m);
}

fn mode() -> Mode {
    MODE.read().unwrap().clone().unwrap_or(Mode::Auto)
}

#[derive(Debug, Clone, PartialEq)]
pub struct Proxy {
    pub host: String,
    pub port: u16,
    /// `user:pass`; sent as Basic auth if present
    pub auth: Option<String>,
}

/// Extract host and port from a ws(s):// URL
pub fn target_of(url: &str) -> Result<(String, u16)> {
    let (secure, rest) = if let Some(r) = url.strip_prefix("wss://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("ws://") {
        (false, r)
    } else {
        bail!("中继地址要以 ws:// 或 wss:// 开头");
    };
    let authority = rest.split(['/', '?']).next().unwrap_or("");
    let default = if secure { 443 } else { 80 };
    split_host_port(authority, default)
}

fn split_host_port(s: &str, default: u16) -> Result<(String, u16)> {
    if let Some(r) = s.strip_prefix('[') {
        // IPv6 literal
        let (h, after) = r.split_once(']').ok_or_else(|| anyhow!("地址格式错误: {s}"))?;
        let port = match after.strip_prefix(':') {
            Some(p) => p.parse().context("端口格式错误")?,
            None => default,
        };
        return Ok((h.to_string(), port));
    }
    match s.rsplit_once(':') {
        Some((h, p)) => Ok((h.to_string(), p.parse().context("端口格式错误")?)),
        None => Ok((s.to_string(), default)),
    }
}

/// Parse a proxy URL; returns None for unsupported formats
pub fn parse_proxy(v: &str) -> Option<Proxy> {
    let v = v.trim();
    if v.is_empty() {
        return None;
    }
    let rest = match v.split_once("://") {
        Some(("http", r)) | Some(("HTTP", r)) => r,
        Some(_) => return None, // socks5:// etc. not supported
        None => v,
    };
    let rest = rest.trim_end_matches('/');
    let (auth, hp) = match rest.rsplit_once('@') {
        Some((a, hp)) => (Some(a.to_string()), hp),
        None => (None, rest),
    };
    let (host, port) = split_host_port(hp, 80).ok()?;
    if host.is_empty() {
        return None;
    }
    Some(Proxy { host, port, auth })
}

fn no_proxy_matches(list: &str, host: &str) -> bool {
    let host = host.to_lowercase();
    list.split(',').map(|s| s.trim().to_lowercase()).any(|p| {
        if p.is_empty() {
            return false;
        }
        if p == "*" {
            return true;
        }
        let p = p.trim_start_matches('*').trim_start_matches('.');
        let p = p.split(':').next().unwrap_or(p);
        host == p || host.ends_with(&format!(".{p}"))
    })
}

fn env(names: &[&str]) -> Option<String> {
    names
        .iter()
        .find_map(|n| std::env::var(n).ok().filter(|v| !v.trim().is_empty()))
}

/// Which proxy this ws(s):// URL should use; None means direct
pub fn proxy_for(url: &str) -> Option<Proxy> {
    let (host, _) = target_of(url).ok()?;
    proxy_for_host(&host, url.starts_with("wss://"))
}

/// Which proxy to use when connecting to host. `secure` decides whether HTTPS_PROXY or HTTP_PROXY is read first;
/// non-HTTP connections such as SSH are treated as secure=true too (they're all CONNECT tunnels)
pub fn proxy_for_host(host: &str, secure: bool) -> Option<Proxy> {
    lookup(host, secure).map(|(p, _)| p)
}

fn is_local(host: &str) -> bool {
    host == "localhost" || host.starts_with("127.") || host == "::1"
}

/// Private addresses (10/8, 172.16/12, 192.168/16, 169.254/16, fc00::/7, fe80::/10).
/// Direct in auto mode: corporate proxies usually can't reach the internal network and often don't allow CONNECT to ports other than 443
fn is_private(host: &str) -> bool {
    match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(a)) => a.is_private() || a.is_link_local(),
        Ok(std::net::IpAddr::V6(a)) => {
            let s = a.segments()[0];
            (s & 0xfe00) == 0xfc00 || (s & 0xffc0) == 0xfe80
        }
        Err(_) => false,
    }
}

/// Returns the proxy and a description of where it came from (for the UI)
fn lookup(host: &str, secure: bool) -> Option<(Proxy, String)> {
    if is_local(host) {
        return None;
    }
    match mode() {
        Mode::Off => None,
        Mode::Manual(v) => parse_proxy(&v).map(|p| (p, "手动设置".to_string())),
        Mode::Auto => {
            if is_private(host) {
                return None;
            }
            if let Some(np) = env(&["NO_PROXY", "no_proxy"]) {
                if no_proxy_matches(&np, host) {
                    return None;
                }
            }
            let names: &[&str] = if secure {
                &["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"]
            } else {
                &["HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"]
            };
            if let Some((name, v)) = names
                .iter()
                .find_map(|n| std::env::var(n).ok().filter(|v| !v.trim().is_empty()).map(|v| (*n, v)))
            {
                return parse_proxy(&v).map(|p| (p, format!("环境变量 {name}")));
            }
            system_proxy(secure).map(|p| (p, "系统代理设置".to_string()))
        }
    }
}

/// Current proxy status for the UI: e.g. "127.0.0.1:7897（环境变量 HTTPS_PROXY）"; None when direct
pub fn describe() -> Option<String> {
    // Probe with a public address that isn't in NO_PROXY
    lookup("example.com", true).map(|(p, src)| format!("{}:{}（{src}）", p.host, p.port))
}

/// Windows' "system proxy" (tools like Clash often only change this, without setting environment variables)
#[cfg(windows)]
fn system_proxy(secure: bool) -> Option<Proxy> {
    let out = std::process::Command::new("reg")
        .args([
            "query",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings",
        ])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let value = |name: &str| {
        text.lines().find_map(|l| {
            let mut it = l.split_whitespace();
            (it.next() == Some(name)).then(|| it.nth(1).unwrap_or("").to_string())
        })
    };
    if value("ProxyEnable").as_deref() != Some("0x1") {
        return None;
    }
    let server = value("ProxyServer")?;
    // May be "host:port" or "http=h:p;https=h:p"
    let pick = if server.contains('=') {
        let want = if secure { "https=" } else { "http=" };
        server
            .split(';')
            .find_map(|p| p.strip_prefix(want))
            .or_else(|| server.split(';').find_map(|p| p.strip_prefix("http=")))?
            .to_string()
    } else {
        server
    };
    parse_proxy(&pick)
}

#[cfg(not(windows))]
fn system_proxy(_secure: bool) -> Option<Proxy> {
    None
}

/// Connect to the proxy and set up a CONNECT tunnel to host:port; the returned TcpStream is then used like a direct connection
pub async fn connect(p: &Proxy, host: &str, port: u16) -> Result<TcpStream> {
    let mut s = TcpStream::connect((p.host.as_str(), p.port))
        .await
        .with_context(|| format!("连接代理 {}:{} 失败", p.host, p.port))?;
    let target = if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    let mut req = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n");
    if let Some(a) = &p.auth {
        req.push_str(&format!("Proxy-Authorization: Basic {}\r\n", STANDARD.encode(a)));
    }
    req.push_str("\r\n");
    s.write_all(req.as_bytes()).await?;
    // Read byte by byte up to the end of the response headers, never further: the following bytes belong to TLS
    let mut head = Vec::with_capacity(256);
    let mut b = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() > 8192 {
            bail!("代理响应头过长");
        }
        if s.read(&mut b).await? == 0 {
            bail!("代理关闭了连接");
        }
        head.push(b[0]);
    }
    let head = String::from_utf8_lossy(&head);
    let status = head.lines().next().unwrap_or("");
    if status.split_whitespace().nth(1) != Some("200") {
        bail!("代理拒绝建立隧道：{status}");
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets() {
        assert_eq!(target_of("wss://1.2.3.4:7878").unwrap(), ("1.2.3.4".into(), 7878));
        assert_eq!(target_of("wss://r.example.com/v1/agent?x=1").unwrap(), ("r.example.com".into(), 443));
        assert_eq!(target_of("ws://[::1]:9/").unwrap(), ("::1".into(), 9));
    }

    #[test]
    fn proxies() {
        assert_eq!(
            parse_proxy("http://u:p@127.0.0.1:7897/"),
            Some(Proxy { host: "127.0.0.1".into(), port: 7897, auth: Some("u:p".into()) })
        );
        assert_eq!(parse_proxy("127.0.0.1:7897").unwrap().port, 7897);
        assert_eq!(parse_proxy("socks5://127.0.0.1:1080"), None);
        assert!(no_proxy_matches("localhost, .corp.com", "a.corp.com"));
        assert!(no_proxy_matches("corp.com", "corp.com"));
        assert!(!no_proxy_matches("corp.com", "notcorp.com"));
        assert!(no_proxy_matches("*", "x"));
        assert!(is_private("10.1.2.3") && is_private("192.168.0.1") && is_private("fe80::1"));
        assert!(!is_private("43.110.40.75") && !is_private("example.com"));
    }

    #[test]
    fn modes() {
        set_mode(Mode::Manual("http://10.0.0.1:3128".into()));
        assert_eq!(proxy_for_host("example.com", true).unwrap().port, 3128);
        assert_eq!(proxy_for_host("127.0.0.1", true), None);
        set_mode(Mode::Off);
        assert_eq!(proxy_for_host("example.com", true), None);
        set_mode(Mode::Auto);
    }

    #[tokio::test]
    async fn connect_tunnel() {
        // Fake proxy: after replying 200, echo the following bytes back
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut s, _) = l.accept().await.unwrap();
            let mut buf = vec![0u8; 1024];
            let n = s.read(&mut buf).await.unwrap();
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            assert!(req.starts_with("CONNECT relay.test:7878 HTTP/1.1\r\n"));
            assert!(req.contains("Proxy-Authorization: Basic dTpw\r\n"));
            s.write_all(b"HTTP/1.1 200 Connection established\r\n\r\nhello").await.unwrap();
        });
        let p = Proxy { host: "127.0.0.1".into(), port, auth: Some("u:p".into()) };
        let mut s = connect(&p, "relay.test", 7878).await.unwrap();
        let mut got = [0u8; 5];
        s.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"hello");
    }
}
