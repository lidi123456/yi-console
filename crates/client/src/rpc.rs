//! 与 yi-agentd 的 JSON-RPC 连接（按行 JSON），与具体传输（stdio / SSH channel / 中继）无关。

use anyhow::{anyhow, Result};
use yi_proto::{Message, Notification, Request};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot, watch};

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

pub struct RpcConn {
    tx: mpsc::UnboundedSender<String>,
    pending: Pending,
    next: AtomicU64,
    closed: watch::Receiver<bool>,
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

        let c1 = closed_tx.clone();
        tokio::spawn(async move {
            while let Some(mut line) = rx.recv().await {
                line.push('\n');
                if w.write_all(line.as_bytes()).await.is_err() || w.flush().await.is_err() {
                    break;
                }
            }
            let _ = c1.send(true);
        });

        let p2 = pending.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(r).lines();
            while let Ok(Some(line)) = lines.next_line().await {
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
            // 连接断了：所有等待中的请求都失败
            for (_, tx) in p2.lock().unwrap().drain() {
                let _ = tx.send(Err("连接已断开".into()));
            }
            let _ = closed_tx.send(true);
        });

        Arc::new(RpcConn {
            tx,
            pending,
            next: AtomicU64::new(1),
            closed: closed_rx,
        })
    }

    pub async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let timeout = match method {
            "history.list" | "session.create" | "hello" | "tools.detect" => 120,
            _ => 60,
        };
        self.call_timeout(method, params, Duration::from_secs(timeout)).await
    }

    pub async fn call_timeout(&self, method: &str, params: Value, t: Duration) -> Result<Value> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let req = Request {
            id,
            method: method.to_string(),
            params,
        };
        self.tx
            .send(serde_json::to_string(&req)?)
            .map_err(|_| anyhow!("连接已断开"))?;
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
        *self.closed.borrow()
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
