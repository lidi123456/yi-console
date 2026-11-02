//! Port forwarding (host side): list listening TCP ports, and connect a connection to a local port.
//!
//! Like `proxy.connect`, `port.connect` is the first message on a connection: after a one-line reply, the connection's bytes
//! are relayed as is to 127.0.0.1:port. Only local loopback is reachable, so it can't be used to reach other machines. yi-agentd itself still listens on no ports;
//! the local port is opened by the client (see crates/client/src/forward.rs).

use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use tokio::io::{AsyncBufRead, AsyncRead, AsyncWrite, AsyncWriteExt};

/// A listening port
#[derive(Debug, Clone, PartialEq)]
pub struct Listen {
    pub port: u16,
    /// Listen address: 127.0.0.1, 0.0.0.0, ::, ::1, etc.
    pub addr: String,
    pub pid: Option<u32>,
    pub process: Option<String>,
}

/// ports.list: TCP ports listening on this machine (multiple addresses for one port merged into one entry)
pub fn list() -> Result<Value> {
    let mut v = listening()?;
    v.sort_by_key(|l| l.port);
    let mut by: BTreeMap<u16, Listen> = BTreeMap::new();
    for l in v {
        by.entry(l.port)
            .and_modify(|e| {
                if e.process.is_none() {
                    e.process = l.process.clone();
                    e.pid = l.pid;
                }
                // If any address is externally reachable, show it as external
                if !is_loopback(&l.addr) {
                    e.addr = l.addr.clone();
                }
            })
            .or_insert(l);
    }
    let ports: Vec<Value> = by
        .into_values()
        .map(|l| {
            json!({
                "port": l.port,
                "addr": l.addr,
                "pid": l.pid,
                "process": l.process,
            })
        })
        .collect();
    Ok(json!({ "ports": ports }))
}

fn is_loopback(a: &str) -> bool {
    a.starts_with("127.") || a == "::1" || a == "localhost"
}

#[cfg(target_os = "linux")]
fn listening() -> Result<Vec<Listen>> {
    use std::collections::HashMap;
    let mut out = Vec::new();
    let mut inodes: HashMap<u64, usize> = HashMap::new();
    for (file, v6) in [("/proc/net/tcp", false), ("/proc/net/tcp6", true)] {
        let Ok(text) = std::fs::read_to_string(file) else { continue };
        for l in text.lines().skip(1) {
            if let Some((listen, inode)) = parse_proc_line(l, v6) {
                inodes.insert(inode, out.len());
                out.push(listen);
            }
        }
    }
    // Find processes by socket inode: only our own user's processes are visible; others are left blank
    if let Ok(rd) = std::fs::read_dir("/proc") {
        for e in rd.flatten() {
            let Ok(pid) = e.file_name().to_string_lossy().parse::<u32>() else { continue };
            let Ok(fds) = std::fs::read_dir(e.path().join("fd")) else { continue };
            for fd in fds.flatten() {
                let Ok(t) = std::fs::read_link(fd.path()) else { continue };
                let t = t.to_string_lossy();
                let Some(n) = t.strip_prefix("socket:[").and_then(|s| s.strip_suffix(']')) else { continue };
                let Ok(n) = n.parse::<u64>() else { continue };
                if let Some(&i) = inodes.get(&n) {
                    if out[i].pid.is_none() {
                        out[i].pid = Some(pid);
                        out[i].process = proc_name(pid);
                    }
                }
            }
        }
    }
    Ok(out)
}

#[cfg(target_os = "linux")]
fn proc_name(pid: u32) -> Option<String> {
    let cmd = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let args: Vec<String> = cmd
        .split(|b| *b == 0)
        .filter(|a| !a.is_empty())
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect();
    let first = args.first()?;
    let base = first.rsplit('/').next().unwrap_or(first).to_string();
    // For interpreters like node / python, include the script name so it's easier to recognize
    let script = args.iter().skip(1).find(|a| !a.starts_with('-')).map(|a| a.rsplit('/').next().unwrap_or(a).to_string());
    Some(match script {
        Some(s) if ["node", "python", "python3", "deno", "bun", "ruby", "java"].contains(&base.as_str()) => format!("{base} {s}"),
        _ => base,
    })
}

/// A line of /proc/net/tcp: `sl local_address rem_address st ... inode`; st=0A is LISTEN
#[cfg(any(target_os = "linux", test))]
fn parse_proc_line(l: &str, v6: bool) -> Option<(Listen, u64)> {
    let f: Vec<&str> = l.split_whitespace().collect();
    if f.len() < 10 || f[3] != "0A" {
        return None;
    }
    let (ip, port) = f[1].split_once(':')?;
    let port = u16::from_str_radix(port, 16).ok()?;
    let addr = if v6 {
        let b: Vec<u8> = (0..16).filter_map(|i| u8::from_str_radix(ip.get(i * 2..i * 2 + 2)?, 16).ok()).collect();
        if b.len() != 16 {
            return None;
        }
        // Every 4 bytes is a little-endian 32-bit word
        let mut o = [0u8; 16];
        for w in 0..4 {
            for k in 0..4 {
                o[w * 4 + k] = b[w * 4 + 3 - k];
            }
        }
        let a = std::net::Ipv6Addr::from(o);
        match a.to_ipv4_mapped() {
            Some(v4) => v4.to_string(),
            None => a.to_string(),
        }
    } else {
        let n = u32::from_str_radix(ip, 16).ok()?;
        std::net::Ipv4Addr::from(n.swap_bytes()).to_string()
    };
    let inode = f[9].parse().ok()?;
    Some((Listen { port, addr, pid: None, process: None }, inode))
}

#[cfg(target_os = "macos")]
fn listening() -> Result<Vec<Listen>> {
    // lsof -F: p<pid> c<command> n<addr:port>
    let o = std::process::Command::new("lsof").args(["-nP", "-iTCP", "-sTCP:LISTEN", "-FpcnT"]).output()?;
    let mut out = Vec::new();
    let (mut pid, mut cmd) = (None, None);
    for l in String::from_utf8_lossy(&o.stdout).lines() {
        let (k, v) = l.split_at(1.min(l.len()));
        match k {
            "p" => pid = v.parse().ok(),
            "c" => cmd = Some(v.to_string()),
            "n" => {
                if let Some((a, p)) = v.rsplit_once(':') {
                    if let Ok(port) = p.parse() {
                        let addr = a.trim_matches(['[', ']']).replace('*', "0.0.0.0");
                        out.push(Listen { port, addr, pid, process: cmd.clone() });
                    }
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

#[cfg(windows)]
fn listening() -> Result<Vec<Listen>> {
    use std::collections::HashMap;
    use std::os::windows::process::CommandExt;
    let run = |args: &[&str]| -> Result<String> {
        let o = std::process::Command::new(args[0]).args(&args[1..]).creation_flags(0x0800_0000).output()?;
        Ok(String::from_utf8_lossy(&o.stdout).into_owned())
    };
    let mut names: HashMap<u32, String> = HashMap::new();
    if let Ok(t) = run(&["tasklist", "/fo", "csv", "/nh"]) {
        for l in t.lines() {
            let f: Vec<&str> = l.split("\",\"").map(|x| x.trim_matches('"')).collect();
            if f.len() >= 2 {
                if let Ok(p) = f[1].parse() {
                    names.insert(p, f[0].trim_end_matches(".exe").to_string());
                }
            }
        }
    }
    let mut out = Vec::new();
    // netstat's state column is localized, so recognize listeners by "remote address is 0.0.0.0:0 / [::]:0"
    for l in run(&["netstat", "-ano", "-p", "TCP"])?.lines().chain(run(&["netstat", "-ano", "-p", "TCPv6"])?.lines()) {
        let f: Vec<&str> = l.split_whitespace().collect();
        if f.len() < 5 || !f[0].starts_with("TCP") || !(f[2] == "0.0.0.0:0" || f[2] == "[::]:0") {
            continue;
        }
        let Some((a, p)) = f[1].rsplit_once(':') else { continue };
        let Ok(port) = p.parse() else { continue };
        let pid = f[4].parse().ok();
        out.push(Listen {
            port,
            addr: a.trim_matches(['[', ']']).to_string(),
            pid,
            process: pid.and_then(|p| names.get(&p).cloned()),
        });
    }
    Ok(out)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn listening() -> Result<Vec<Listen>> {
    Ok(Vec::new())
}

/// Connect to a local port: try IPv4 loopback first, then IPv6 (some dev servers only listen on ::1)
pub async fn dial(port: u16) -> Result<tokio::net::TcpStream> {
    if port == 0 {
        bail!("端口无效");
    }
    match tokio::net::TcpStream::connect(("127.0.0.1", port)).await {
        Ok(s) => Ok(s),
        Err(e4) => match tokio::net::TcpStream::connect(("::1", port)).await {
            Ok(s) => Ok(s),
            Err(_) => bail!("主机上的 {port} 端口连不上（没有程序在监听？）：{e4}"),
        },
    }
}

/// Handle port.connect: reply with one result line first, then relay both ways until either side disconnects
pub async fn serve<R, W>(mut r: R, mut w: W, id: u64, port: u16)
where
    R: AsyncBufRead + AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let tcp = match dial(port).await {
        Ok(s) => s,
        Err(e) => {
            let line = json!({"id": id, "error": {"code": -32000, "message": format!("{e:#}")}});
            let _ = w.write_all(format!("{line}\n").as_bytes()).await;
            let _ = w.flush().await;
            return;
        }
    };
    let _ = tcp.set_nodelay(true);
    let ok = json!({"id": id, "result": {"ok": true}});
    if w.write_all(format!("{ok}\n").as_bytes()).await.is_err() || w.flush().await.is_err() {
        return;
    }
    let (mut tr, mut tw) = tcp.into_split();
    tokio::select! {
        _ = tokio::io::copy(&mut r, &mut tw) => {}
        _ = async {
            let _ = tokio::io::copy(&mut tr, &mut w).await;
            let _ = w.flush().await;
        } => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_proc_net_tcp() {
        // 127.0.0.1:3000 LISTEN
        let l = "   0: 0100007F:0BB8 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 12345 1 0000000000000000 100 0 0 10 0";
        let (x, ino) = parse_proc_line(l, false).unwrap();
        assert_eq!((x.port, x.addr.as_str(), ino), (3000, "127.0.0.1", 12345));
        // Established connections don't count
        assert!(parse_proc_line(&l.replace(" 0A ", " 01 "), false).is_none());
        // [::1]:5173
        let l6 = "   0: 00000000000000000000000001000000:1435 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 999 1";
        let (x, _) = parse_proc_line(l6, true).unwrap();
        assert_eq!((x.port, x.addr.as_str()), (5173, "::1"));
    }

    #[tokio::test]
    async fn lists_and_connects() {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        #[cfg(any(target_os = "linux", windows, target_os = "macos"))]
        {
            let v = list().unwrap();
            assert!(v["ports"].as_array().unwrap().iter().any(|p| p["port"] == port), "{v}");
        }
        tokio::spawn(async move {
            let (mut s, _) = l.accept().await.unwrap();
            let _ = s.write_all(b"hello").await;
        });
        let (a, b) = tokio::io::duplex(1024);
        let (br, bw) = tokio::io::split(b);
        tokio::spawn(serve(tokio::io::BufReader::new(br), bw, 7, port));
        let (ar, _aw) = tokio::io::split(a);
        let mut ar = tokio::io::BufReader::new(ar);
        let mut line = String::new();
        use tokio::io::{AsyncBufReadExt, AsyncReadExt};
        ar.read_line(&mut line).await.unwrap();
        assert!(line.contains("\"ok\":true"), "{line}");
        let mut buf = [0u8; 5];
        ar.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hello");
    }
}
