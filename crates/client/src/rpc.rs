//! JSON-RPC connection to yi-agentd (line-delimited JSON), independent of the transport (stdio / SSH channel / relay).

use anyhow::{anyhow, Result};
use yi_proto::{Message, Notification, Request};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot, watch};

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

async fn wait_cancelled(rx: &mut watch::Receiver<bool>) {
    if *rx.borrow_and_update() {
        return;
    }
    while rx.changed().await.is_ok() {
        if *rx.borrow_and_update() {
            return;
        }
    }
}

pub struct RpcConn {
    tx: mpsc::UnboundedSender<String>,
    pending: Pending,
    next: AtomicU64,
    closed: watch::Receiver<bool>,
    cancel_tx: watch::Sender<bool>,
}

impl RpcConn {
    pub fn start<R, W>(r: R, mut w: W, on_notify: Arc<dyn Fn(Notification) + Send + Sync>) -> Arc<RpcConn>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (closed_tx, closed_rx) = watch::channel(false);
        let closed_tx = Arc::new(closed_tx);
        let (cancel_tx, mut cancel_r) = watch::channel(false);
        let remaining = Arc::new(AtomicU8::new(2));

        let p1 = pending.clone();
        let c1 = closed_tx.clone();
        let rem1 = remaining.clone();
        let cancel_tx_w = cancel_tx.clone();
        let mut cancel_w = cancel_r.clone();
        tokio::spawn(async move {
            loop {
                let mut line = tokio::select! {
                    biased;
                    _ = wait_cancelled(&mut cancel_w) => break,
                    msg = rx.recv() => match msg {
                        Some(m) => m,
                        None => break,
                    },
                };
                line.push('\n');
                let write_res = tokio::select! {
                    biased;
                    _ = wait_cancelled(&mut cancel_w) => break,
                    res = w.write_all(line.as_bytes()) => res,
                };
                if write_res.is_err() {
                    break;
                }
                if rx.is_empty() {
                    let flush_res = tokio::select! {
                        biased;
                        _ = wait_cancelled(&mut cancel_w) => break,
                        res = w.flush() => res,
                    };
                    if flush_res.is_err() {
                        break;
                    }
                }
            }
            drop(w);
            let _ = cancel_tx_w.send(true);
            for (_, tx) in p1.lock().unwrap().drain() {
                let _ = tx.send(Err("连接已断开".into()));
            }
            if rem1.fetch_sub(1, Ordering::AcqRel) == 1 {
                let _ = c1.send(true);
            }
        });

        let p2 = pending.clone();
        let rem2 = remaining;
        let cancel_tx_r = cancel_tx.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(r).lines();
            loop {
                let line = tokio::select! {
                    biased;
                    _ = wait_cancelled(&mut cancel_r) => break,
                    res = lines.next_line() => match res {
                        Ok(Some(l)) => l,
                        _ => break,
                    },
                };
                match serde_json::from_str::<Message>(&line) {
                    Ok(Message::Response(resp)) => {
                        if let Some(tx) = p2.lock().unwrap().remove(&resp.id) {
                            let _ = tx.send(match resp.error {
                                Some(e) => Err(e.message),
                                None => Ok(resp.result.unwrap_or(Value::Null)),
                            });
                        }
                    }
                    Ok(Message::Notification(n)) => on_notify(n),
                    _ => {}
                }
            }
            drop(lines);
            let _ = cancel_tx_r.send(true);
            // Connection lost: fail every pending request
            for (_, tx) in p2.lock().unwrap().drain() {
                let _ = tx.send(Err("连接已断开".into()));
            }
            if rem2.fetch_sub(1, Ordering::AcqRel) == 1 {
                let _ = closed_tx.send(true);
            }
        });

        Arc::new(RpcConn {
            tx,
            pending,
            next: AtomicU64::new(1),
            closed: closed_rx,
            cancel_tx,
        })
    }

    /// Explicitly tear down both reader and writer tasks and drop the underlying IO halves.
    pub fn close(&self) {
        let _ = self.cancel_tx.send(true);
        for (_, tx) in self.pending.lock().unwrap().drain() {
            let _ = tx.send(Err("连接已断开".into()));
        }
    }

    pub async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let timeout = match method {
            // Model lists may start the tool to ask it (`probe::MODELS_BUDGET`, 100 s)
            "history.list" | "history.read" | "session.history" | "session.create" | "hello" | "tools.detect"
            | "tools.models" | "session.models" => 120,
            _ => 60,
        };
        self.call_timeout(method, params, Duration::from_secs(timeout)).await
    }

    pub async fn call_timeout(&self, method: &str, params: Value, t: Duration) -> Result<Value> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let req = Request {
            id,
            method: method.to_string(),
            params,
        };
        let payload = serde_json::to_string(&req)?;
        let (tx, rx) = oneshot::channel();
        {
            let mut guard = self.pending.lock().unwrap();
            if self.is_closed() {
                return Err(anyhow!("连接已断开"));
            }
            guard.insert(id, tx);
        }
        if self.tx.send(payload).is_err() {
            self.pending.lock().unwrap().remove(&id);
            return Err(anyhow!("连接已断开"));
        }
        match tokio::time::timeout(t, rx).await {
            Ok(Ok(Ok(v))) => Ok(v),
            Ok(Ok(Err(e))) => Err(anyhow!(e)),
            Ok(Err(_)) => Err(anyhow!("连接已断开")),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                Err(anyhow!("请求超时: {method}"))
            }
        }
    }

    pub fn is_closed(&self) -> bool {
        *self.closed.borrow() || *self.cancel_tx.borrow()
    }

    pub async fn wait_closed(&self) {
        let mut c = self.closed.clone();
        while !*c.borrow() {
            if c.changed().await.is_err() {
                return;
            }
        }
    }
}

impl Drop for RpcConn {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn close_stops_both_halves_and_fails_pending() {
        let (client_stream, mut peer_stream) = tokio::io::duplex(4096);
        let (r, w) = tokio::io::split(client_stream);
        let rpc = RpcConn::start(r, w, Arc::new(|_| {}));

        let rpc_call = rpc.clone();
        let pending = tokio::spawn(async move { rpc_call.call("ping", serde_json::json!({})).await });

        // Drain the written request frame so the peer writer isn't backpressured
        let mut buf = [0u8; 128];
        let n = tokio::time::timeout(Duration::from_secs(2), peer_stream.read(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert!(n > 0);

        rpc.close();

        let err = tokio::time::timeout(Duration::from_secs(2), pending)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(err.to_string().contains("连接已断开"));

        tokio::time::timeout(Duration::from_secs(2), rpc.wait_closed())
            .await
            .unwrap();

        // Both ReadHalf and WriteHalf must be dropped by RpcConn tasks so peer sees EOF.
        let mut eof_buf = [0u8; 16];
        let eof = tokio::time::timeout(Duration::from_secs(2), peer_stream.read(&mut eof_buf))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(eof, 0);
    }

    #[tokio::test]
    async fn close_releases_stream_when_writer_is_blocked() {
        let (client_stream, mut peer_stream) = tokio::io::duplex(64);
        let (r, w) = tokio::io::split(client_stream);
        let rpc = RpcConn::start(r, w, Arc::new(|_| {}));

        // Fill the 64-byte duplex buffer without reading from peer_stream so write_all blocks.
        let rpc_call = rpc.clone();
        let pending = tokio::spawn(async move {
            rpc_call
                .call("big", serde_json::json!({ "pad": "x".repeat(4096) }))
                .await
        });

        tokio::time::sleep(Duration::from_millis(30)).await;
        rpc.close();
        assert!(rpc.is_closed());

        let err = tokio::time::timeout(Duration::from_secs(2), pending)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(err.to_string().contains("连接已断开"));

        tokio::time::timeout(Duration::from_secs(2), rpc.wait_closed())
            .await
            .unwrap();

        let mut buf = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), peer_stream.read_to_end(&mut buf))
            .await
            .unwrap()
            .unwrap();
    }
}
