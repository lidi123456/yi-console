//! 端口转发（客户端这一端）：在本机 127.0.0.1 上开一个端口，每来一条连接，就经主机的连接通道
//! 接到主机上的 127.0.0.1:port。通道怎么开由 `ClientHub::open_port` 决定（SSH 用 direct-tcpip，
//! 中继和经电脑转发的主机新开一条连接发 `port.connect`）。
//!
//! 只监听回环地址，别的机器连不进来。

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
    /// 主机上的端口
    pub port: u16,
    /// 本机监听的端口
    pub local_port: u16,
    /// 本机主机不用转发，直接就是那个端口
    pub direct: bool,
    /// 当前打开的连接数
    pub conns: usize,
    /// 最近一次连接失败的原因（比如主机上那个端口没在监听）
    pub error: Option<String>,
    pub label: Option<String>,
}

pub struct Forward {
    pub info: std::sync::Mutex<ForwardInfo>,
    conns: Arc<AtomicUsize>,
    task: Option<tokio::task::JoinHandle<()>>,
    /// 停止转发时连带断开已经打开的连接
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

/// 本机主机：端口本来就在本机，不开监听
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

/// 开本地监听。先试和主机上相同的端口号（地址好记，前端开发服务器的回调地址也对得上），被占了再随便选一个
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
    // 任一方向结束就关掉另一边的写端，让对方知道
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
        // 「主机上」的服务：回显
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
