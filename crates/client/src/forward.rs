//! Port forwarding (client side): open a port on local 127.0.0.1, and for each incoming connection, go through the host's connection channel
//! to 127.0.0.1:port on the host. How the channel is opened is up to `ClientHub::open_port` (SSH uses direct-tcpip;
//! relay hosts and hosts forwarded through a computer open a new connection and send `port.connect`).
//!
//! Only listens on loopback, so other machines can't connect.

use anyhow::{Context, Result};
use serde::Serialize;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;

pub type BoxR = Box<dyn AsyncRead + Unpin + Send>;
pub type BoxW = Box<dyn AsyncWrite + Unpin + Send>;
pub type Opener = Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Result<(BoxR, BoxW)>> + Send>> + Send + Sync>;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ForwardInfo {
    pub host_id: String,
    /// Port on the host
    pub port: u16,
    /// Port listening on this machine
    pub local_port: u16,
    /// The local host needs no forwarding; it's just that port
    pub direct: bool,
    /// Number of currently open connections
    pub conns: usize,
    /// Reason for the last failed connection (e.g. nothing listening on that port on the host)
    pub error: Option<String>,
    pub label: Option<String>,
}

pub struct Forward {
    pub info: std::sync::Mutex<ForwardInfo>,
    conns: Arc<AtomicUsize>,
    task: Option<tokio::task::JoinHandle<()>>,
    /// Stopping the forward also closes connections already open
    stop: Option<tokio::sync::watch::Sender<bool>>,
}

impl Forward {
    pub fn snapshot(&self) -> ForwardInfo {
        let mut i = self.info.lock().unwrap().clone();
        i.conns = self.conns.load(Ordering::Relaxed);
        i
    }
}

impl Drop for Forward {
    fn drop(&mut self) {
        if let Some(t) = self.task.take() {
            t.abort();
        }
        if let Some(s) = self.stop.take() {
            let _ = s.send(true);
        }
    }
}

/// Local host: the port is already on this machine, so don't listen
pub fn direct(host_id: &str, port: u16, label: Option<String>) -> Forward {
    Forward {
        info: std::sync::Mutex::new(ForwardInfo {
            host_id: host_id.into(),
            port,
            local_port: port,
            direct: true,
            conns: 0,
            error: None,
            label,
        }),
        conns: Arc::new(AtomicUsize::new(0)),
        task: None,
        stop: None,
    }
}

/// Start the local listener. Try the same port number as on the host first (easy to remember, and dev-server callback URLs still match); if taken, pick any
pub async fn start(host_id: &str, port: u16, local: Option<u16>, label: Option<String>, open: Opener) -> Result<Arc<Forward>> {
    let l = match local {
        Some(p) => TcpListener::bind(("127.0.0.1", p)).await.with_context(|| format!("本机 {p} 端口已被占用"))?,
        None => match TcpListener::bind(("127.0.0.1", port)).await {
            Ok(l) => l,
            Err(_) => TcpListener::bind(("127.0.0.1", 0)).await?,
        },
    };
    let local_port = l.local_addr()?.port();
    let conns = Arc::new(AtomicUsize::new(0));
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let fwd = Arc::new_cyclic(|weak: &std::sync::Weak<Forward>| {
        let weak = weak.clone();
        let conns2 = conns.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((tcp, _)) = l.accept().await else { break };
                let _ = tcp.set_nodelay(true);
                let open = open.clone();
                let conns = conns2.clone();
                let weak = weak.clone();
                let mut stop = stop_rx.clone();
                tokio::spawn(async move {
                    conns.fetch_add(1, Ordering::Relaxed);
                    let r = tokio::select! {
                        r = pipe(tcp, open) => r,
                        _ = stop.changed() => Ok(()),
                    };
                    conns.fetch_sub(1, Ordering::Relaxed);
                    if let Some(f) = weak.upgrade() {
                        f.info.lock().unwrap().error = r.err().map(|e| format!("{e:#}"));
                    }
                });
            }
        });
        Forward {
            info: std::sync::Mutex::new(ForwardInfo {
                host_id: host_id.into(),
                port,
                local_port,
                direct: false,
                conns: 0,
                error: None,
                label,
            }),
            conns,
            task: Some(task),
            stop: Some(stop_tx),
        }
    });
    Ok(fwd)
}

async fn pipe(tcp: tokio::net::TcpStream, open: Opener) -> Result<()> {
    let (mut rr, mut rw) = open().await?;
    let (mut tr, mut tw) = tcp.into_split();
    // When either direction ends, close the other side's write half so the peer knows
    let up = async {
        let _ = tokio::io::copy(&mut tr, &mut rw).await;
        let _ = rw.shutdown().await;
    };
    let down = async {
        let _ = tokio::io::copy(&mut rr, &mut tw).await;
        let _ = tw.shutdown().await;
    };
    tokio::join!(up, down);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn forwards_bytes() {
        // The service "on the host": echo
        let srv = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let sp = srv.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let (mut s, _) = srv.accept().await.unwrap();
                tokio::spawn(async move {
                    let mut b = [0u8; 64];
                    let n = s.read(&mut b).await.unwrap();
                    s.write_all(&b[..n]).await.unwrap();
                });
            }
        });
        let open: Opener = Arc::new(move || {
            Box::pin(async move {
                let s = tokio::net::TcpStream::connect(("127.0.0.1", sp)).await?;
                let (r, w) = s.into_split();
                Ok((Box::new(r) as BoxR, Box::new(w) as BoxW))
            })
        });
        let f = start("h", sp, None, None, open).await.unwrap();
        let lp = f.snapshot().local_port;
        assert_ne!(lp, sp, "同号端口被服务本身占着，应该换一个");
        let mut c = tokio::net::TcpStream::connect(("127.0.0.1", lp)).await.unwrap();
        c.write_all(b"ping").await.unwrap();
        let mut b = [0u8; 4];
        c.read_exact(&mut b).await.unwrap();
        assert_eq!(&b, b"ping");
        drop(f);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(tokio::net::TcpStream::connect(("127.0.0.1", lp)).await.is_err(), "停止后端口应该关掉");
    }
}
