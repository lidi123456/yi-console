//! 端到端加密：Noise IK（配对时 IKpsk1），实现用 snow。
//!
//! - 客户端事先知道设备的 Noise 静态公钥（配对二维码里的 `apk`），第一条消息就带上自己的静态公钥（加密的），
//!   一个往返完成双向认证；双方各自的临时密钥保证前向保密。
//! - 配对时多混入一个由一次性配对码派生的 psk（放在第一条消息末尾），yi-agentd 读第一条消息时就能确认
//!   对方确实拿到了二维码，确认之后才把它加进已配对列表。
//! - 握手完成后每个 WebSocket 二进制帧是一条 Noise 传输消息，nonce 递增，篡改、重放、调换顺序都会解密失败。

use crate::auth::{self, psk_of, ticket_of};
use crate::wire::{login, AgentLink, OpenReq};
use crate::{b64, unb64_32, Credential, Identity, PairOffer};
use anyhow::{anyhow, bail, Result};
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};

const IK: &str = "Noise_IK_25519_ChaChaPoly_SHA256";
const IK_PSK: &str = "Noise_IKpsk1_25519_ChaChaPoly_SHA256";
const PROLOGUE: &[u8] = b"yi-console e2e v2";
/// Noise 单条消息上限 65535 字节（含 16 字节认证标签）
const MAX_MSG: usize = 65535;
const CHUNK: usize = 60 * 1024;

pub trait WsLike: Stream<Item = Result<Message, WsError>> + Sink<Message, Error = WsError> + Unpin + Send + 'static {}
impl<T> WsLike for T where T: Stream<Item = Result<Message, WsError>> + Sink<Message, Error = WsError> + Unpin + Send + 'static {}

/// 客户端在握手第一条消息里报给 yi-agentd 的信息
#[derive(Debug, Clone)]
pub struct Hello {
    /// 客户端的名字（只有配对时有意义）
    pub name: String,
    /// 客户端的签名公钥，应与中继报告的一致
    pub sign_pub: [u8; 32],
    /// 客户端的 Noise 静态公钥（握手里认证过的）
    pub noise_pub: [u8; 32],
}

fn prologue(device: &str) -> Vec<u8> {
    [PROLOGUE, device.as_bytes()].concat()
}

/// 握手阶段的下一帧。yi-agentd 拒绝时会发一条文本 `{"error":..}` 再断开
async fn next_frame<S: WsLike>(ws: &mut S) -> Result<Vec<u8>> {
    loop {
        match ws.next().await {
            Some(Ok(Message::Binary(b))) => return Ok(b.to_vec()),
            Some(Ok(Message::Text(t))) => {
                let v: Value = serde_json::from_str(&t).unwrap_or_default();
                match v["error"].as_str() {
                    Some(e) => bail!("{e}"),
                    None => continue,
                }
            }
            Some(Ok(Message::Close(_))) | None => bail!("连接被对方关闭"),
            Some(Ok(_)) => continue,
            Some(Err(e)) => return Err(e.into()),
        }
    }
}

async fn initiate<S: WsLike>(
    ws: &mut S,
    me: &Identity,
    apk: &[u8; 32],
    device: &str,
    name: &str,
    psk: Option<&[u8; 32]>,
) -> Result<snow::TransportState> {
    let pro = prologue(device);
    let mut b = snow::Builder::new(if psk.is_some() { IK_PSK } else { IK }.parse()?)
        .local_private_key(&me.noise)?
        .remote_public_key(apk)?
        .prologue(&pro)?;
    if let Some(p) = psk {
        b = b.psk(1, p)?;
    }
    let mut hs = b.build_initiator()?;
    let payload = json!({"name": name, "sign": b64(&me.sign_pub())}).to_string();
    let mut buf = vec![0u8; MAX_MSG];
    let n = hs.write_message(payload.as_bytes(), &mut buf)?;
    ws.send(Message::Binary(buf[..n].to_vec().into())).await?;
    let reply = tokio::time::timeout(Duration::from_secs(20), next_frame(ws))
        .await
        .map_err(|_| anyhow!("等待设备响应超时"))??;
    let mut pl = vec![0u8; MAX_MSG];
    hs.read_message(&reply, &mut pl)
        .map_err(|_| anyhow!("设备身份校验失败：对方不是配对时的那台电脑（电脑重置过身份的话请重新配对）"))?;
    Ok(hs.into_transport_mode()?)
}

async fn respond<S: WsLike, F>(
    ws: &mut S,
    me: &Identity,
    device: &str,
    psk: Option<&[u8; 32]>,
    authorize: F,
) -> Result<(snow::TransportState, Hello)>
where
    F: FnOnce(&Hello) -> Result<()>,
{
    let pro = prologue(device);
    let mut b = snow::Builder::new(if psk.is_some() { IK_PSK } else { IK }.parse()?)
        .local_private_key(&me.noise)?
        .prologue(&pro)?;
    if let Some(p) = psk {
        b = b.psk(1, p)?;
    }
    let mut hs = b.build_responder()?;
    let msg1 = tokio::time::timeout(Duration::from_secs(20), next_frame(ws))
        .await
        .map_err(|_| anyhow!("等待客户端握手超时"))??;
    let mut pl = vec![0u8; MAX_MSG];
    let n = match hs.read_message(&msg1, &mut pl) {
        Ok(n) => n,
        Err(_) => {
            let e = if psk.is_some() { "配对码不对或已失效，请重新生成配对二维码" } else { "握手失败" };
            reject(ws, e).await;
            bail!("{e}");
        }
    };
    let remote: [u8; 32] = hs
        .get_remote_static()
        .and_then(|k| k.try_into().ok())
        .ok_or_else(|| anyhow!("握手里没有客户端公钥"))?;
    let v: Value = serde_json::from_slice(&pl[..n]).unwrap_or_default();
    let hello = Hello {
        name: v["name"].as_str().unwrap_or("").to_string(),
        sign_pub: unb64_32(v["sign"].as_str().unwrap_or(""))?,
        noise_pub: remote,
    };
    if let Err(e) = authorize(&hello) {
        reject(ws, &e.to_string()).await;
        return Err(e);
    }
    let mut buf = vec![0u8; MAX_MSG];
    let n = hs.write_message(&[], &mut buf)?;
    ws.send(Message::Binary(buf[..n].to_vec().into())).await?;
    Ok((hs.into_transport_mode()?, hello))
}

async fn reject<S: WsLike>(ws: &mut S, e: &str) {
    let _ = ws.send(Message::Text(json!({"error": e}).to_string().into())).await;
    let _ = ws.close().await;
}

async fn stopped(rx: &mut Option<watch::Receiver<bool>>) {
    match rx {
        Some(rx) => loop {
            if *rx.borrow() {
                return;
            }
            if rx.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        },
        None => std::future::pending().await,
    }
}

/// 把加密的 WebSocket 变成明文字节流（DuplexStream），后台任务负责加解密。
/// `stop` 置为 true 时断开（吊销客户端用）。
fn pump<S: WsLike>(ws: S, ts: snow::TransportState, stop: Option<watch::Receiver<bool>>) -> DuplexStream {
    let ts = Arc::new(Mutex::new(ts));
    let (ours, theirs) = tokio::io::duplex(256 * 1024);
    let (mut sink, mut stream) = ws.split();
    let (mut rd, mut wr) = tokio::io::split(ours);
    let (ts1, mut stop1) = (ts.clone(), stop.clone());
    tokio::spawn(async move {
        // 明文 -> 密文帧，同时定时 ping 保活（手机网络 NAT 超时短）
        let mut buf = vec![0u8; CHUNK];
        let mut out = vec![0u8; MAX_MSG];
        let mut ping = tokio::time::interval(Duration::from_secs(20));
        ping.tick().await;
        loop {
            tokio::select! {
                n = rd.read(&mut buf) => {
                    let n = match n { Ok(0) | Err(_) => break, Ok(n) => n };
                    let m = match ts1.lock().unwrap().write_message(&buf[..n], &mut out) { Ok(m) => m, Err(_) => break };
                    if sink.send(Message::Binary(out[..m].to_vec().into())).await.is_err() { break; }
                }
                _ = ping.tick() => {
                    if sink.send(Message::Ping(Vec::new().into())).await.is_err() { break; }
                }
                _ = stopped(&mut stop1) => break,
            }
        }
        let _ = sink.close().await;
    });
    let mut stop2 = stop;
    tokio::spawn(async move {
        let mut plain = vec![0u8; MAX_MSG];
        loop {
            let msg = tokio::select! {
                m = stream.next() => m,
                _ = stopped(&mut stop2) => break,
            };
            match msg {
                Some(Ok(Message::Binary(b))) => {
                    let n = match ts.lock().unwrap().read_message(&b, &mut plain) {
                        Ok(n) => n,
                        Err(_) => break,
                    };
                    if wr.write_all(&plain[..n]).await.is_err() {
                        break;
                    }
                }
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => {}
            }
        }
        let _ = wr.shutdown().await;
    });
    theirs
}

// ---------------- 对外接口 ----------------

/// 客户端：经中继连到已配对的设备，返回明文字节流
pub async fn client_connect(c: &Credential) -> Result<DuplexStream> {
    let mut ws = login(&c.relay, "/v1/client", &c.fp, &c.me, auth::ROLE_CLIENT, &c.device, json!({})).await?;
    let ts = initiate(&mut ws, &c.me, &c.apk, &c.device, "", None).await?;
    Ok(pump(ws, ts, None))
}

/// 客户端：用配对二维码完成配对，返回要保存的凭据。`my_name` 显示在电脑的已配对列表里
/// `me` 是这台设备以前和同一台电脑配对时用的身份：沿用它，电脑那边按客户端 id 覆盖旧记录，不会多出一条；
/// 没配对过时传 None，生成新的
pub async fn client_pair(o: &PairOffer, my_name: &str, me: Option<Identity>) -> Result<Credential> {
    let me = me.unwrap_or_else(Identity::generate);
    let extra = json!({"ticket": b64(&ticket_of(&o.code))});
    let mut ws = login(&o.relay, "/v1/client", &o.fp, &me, auth::ROLE_CLIENT, &o.device, extra)
        .await
        .map_err(|e| {
            if e.to_string().contains("4403") {
                anyhow!("配对二维码已失效（10 分钟内有效、只能用一次），请在电脑上重新生成：{e}")
            } else {
                e
            }
        })?;
    initiate(&mut ws, &me, &o.apk, &o.device, my_name, Some(&psk_of(&o.code))).await?;
    let _ = ws.close(None).await;
    Ok(Credential { relay: o.relay.clone(), fp: o.fp.clone(), device: o.device.clone(), apk: o.apk, name: o.name.clone(), me })
}

/// yi-agentd：响应一次 open，建立数据流并完成握手。
/// `psk` 是配对码派生的（配对连接时），`authorize` 在读完客户端第一条消息后调用，返回错误就拒绝。
pub async fn agent_accept<F>(
    a: &AgentLink,
    req: &OpenReq,
    psk: Option<[u8; 32]>,
    stop: Option<watch::Receiver<bool>>,
    authorize: F,
) -> Result<(DuplexStream, Hello)>
where
    F: FnOnce(&Hello) -> Result<()>,
{
    let device = a.device();
    let mut ws = login(&a.relay, "/v1/stream", &a.fp, &a.me, auth::ROLE_STREAM, &device, json!({"stream": req.stream})).await?;
    let (ts, hello) = respond(&mut ws, &a.me, &device, psk.as_ref(), authorize).await?;
    Ok((pump(ws, ts, stop), hello))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::{TcpListener, TcpStream};
    use tokio_tungstenite::WebSocketStream;

    async fn pair() -> (WebSocketStream<TcpStream>, WebSocketStream<TcpStream>) {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (s, _) = l.accept().await.unwrap();
            tokio_tungstenite::accept_async(s).await.unwrap()
        });
        let c = TcpStream::connect(addr).await.unwrap();
        let (cws, _) = tokio_tungstenite::client_async(format!("ws://{addr}/"), c).await.unwrap();
        (cws, server.await.unwrap())
    }

    async fn run(
        client: &Identity,
        apk: [u8; 32],
        agent: &Identity,
        cpsk: Option<[u8; 32]>,
        apsk: Option<[u8; 32]>,
        allow: bool,
    ) -> (Result<snow::TransportState>, Result<(snow::TransportState, Hello)>, WebSocketStream<TcpStream>, WebSocketStream<TcpStream>) {
        let (mut c, mut s) = pair().await;
        let agent = agent.clone();
        let srv = tokio::spawn(async move {
            let r = respond(&mut s, &agent, "dev", apsk.as_ref(), |_| if allow { Ok(()) } else { Err(anyhow!("未授权")) }).await;
            (r, s)
        });
        let cr = initiate(&mut c, client, &apk, "dev", "phone", cpsk.as_ref()).await;
        let (sr, s) = srv.await.unwrap();
        (cr, sr, c, s)
    }

    #[tokio::test]
    async fn ik_handshake_and_transport() {
        let (cl, ag) = (Identity::generate(), Identity::generate());
        let (cr, sr, c, s) = run(&cl, ag.noise_pub(), &ag, None, None, true).await;
        let (sts, hello) = sr.unwrap();
        assert_eq!(hello.noise_pub, cl.noise_pub());
        assert_eq!(hello.sign_pub, cl.sign_pub());
        assert_eq!(hello.name, "phone");
        let mut a = pump(c, cr.unwrap(), None);
        let mut b = pump(s, sts, None);
        a.write_all(b"hello agent").await.unwrap();
        let mut buf = [0u8; 11];
        b.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hello agent");
        // 大于一个分块的数据
        let big = vec![7u8; 200 * 1024];
        let big2 = big.clone();
        tokio::spawn(async move { b.write_all(&big2).await.unwrap() });
        let mut got = vec![0u8; big.len()];
        a.read_exact(&mut got).await.unwrap();
        assert_eq!(got, big);
    }

    #[tokio::test]
    async fn wrong_device_key_fails() {
        let (cl, ag, other) = (Identity::generate(), Identity::generate(), Identity::generate());
        let (cr, sr, _, _) = run(&cl, other.noise_pub(), &ag, None, None, true).await;
        assert!(sr.is_err());
        assert!(cr.is_err());
    }

    #[tokio::test]
    async fn unauthorized_client_rejected_with_reason() {
        let (cl, ag) = (Identity::generate(), Identity::generate());
        let (cr, sr, _, _) = run(&cl, ag.noise_pub(), &ag, None, None, false).await;
        assert!(sr.is_err());
        assert!(cr.unwrap_err().to_string().contains("未授权"));
    }

    #[tokio::test]
    async fn psk_must_match() {
        let (cl, ag) = (Identity::generate(), Identity::generate());
        let code = crate::random_bytes::<32>();
        let (cr, sr, _, _) = run(&cl, ag.noise_pub(), &ag, Some(psk_of(&code)), Some(psk_of(&code)), true).await;
        assert!(cr.is_ok() && sr.is_ok());
        // psk 不对时，yi-agentd 读第一条消息就失败，authorize 不会被调用
        let wrong = crate::random_bytes::<32>();
        let (cr, sr, _, _) = run(&cl, ag.noise_pub(), &ag, Some(psk_of(&wrong)), Some(psk_of(&code)), true).await;
        assert!(sr.is_err());
        assert!(cr.unwrap_err().to_string().contains("配对码"));
    }

    #[tokio::test]
    async fn stop_closes_stream() {
        let (cl, ag) = (Identity::generate(), Identity::generate());
        let (cr, sr, c, s) = run(&cl, ag.noise_pub(), &ag, None, None, true).await;
        let (tx, rx) = watch::channel(false);
        let mut a = pump(c, cr.unwrap(), None);
        let mut b = pump(s, sr.unwrap().0, Some(rx));
        tx.send(true).unwrap();
        let mut buf = [0u8; 1];
        assert_eq!(tokio::time::timeout(Duration::from_secs(5), b.read(&mut buf)).await.unwrap().unwrap(), 0);
        assert_eq!(tokio::time::timeout(Duration::from_secs(5), a.read(&mut buf)).await.unwrap().unwrap(), 0);
    }
}
