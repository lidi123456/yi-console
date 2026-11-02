//! WebSocket connections, certificate fingerprint pinning, relay-layer login (client side) and yi-agentd's control connection.

use crate::auth;
use crate::{b64, device_id_of, unb64_32, Identity};
use anyhow::{anyhow, bail, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

pub type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

pub(crate) fn url(base: &str, path: &str) -> String {
    format!("{}{path}", base.trim_end_matches('/'))
}

/// Accept only the certificate with the given fingerprint. Signatures are still verified as usual, proving the peer holds that certificate's private key.
#[derive(Debug)]
struct PinnedCert {
    fp: String,
    algs: rustls::crypto::WebPkiSupportedAlgorithms,
}

impl rustls::client::danger::ServerCertVerifier for PinnedCert {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        let got = crate::cert_fp(end_entity.as_ref());
        if got == self.fp {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(format!(
                "中继证书指纹不匹配（期望 {}，实际 {got}），可能被中间人攻击，或中继换了证书",
                self.fp
            )))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algs)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algs)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.algs.supported_schemes()
    }
}

fn pinned_connector(fp: &str) -> Result<tokio_tungstenite::Connector> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = PinnedCert { fp: fp.to_string(), algs: provider.signature_verification_algorithms };
    let cfg = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    Ok(tokio_tungstenite::Connector::Rustls(Arc::new(cfg)))
}

pub async fn ws_connect(u: &str, fp: &str) -> Result<Ws> {
    let connector = if !fp.is_empty() && u.starts_with("wss://") { Some(pinned_connector(fp)?) } else { None };
    let via = crate::proxy::proxy_for(u);
    let fut = async {
        match &via {
            Some(p) => {
                let (host, port) = crate::proxy::target_of(u)?;
                let tcp = crate::proxy::connect(p, &host, port).await?;
                let _ = tcp.set_nodelay(true);
                Ok(tokio_tungstenite::client_async_tls_with_config(u, tcp, None, connector).await)
            }
            None => Ok(tokio_tungstenite::connect_async_tls_with_config(u, None, false, connector).await),
        }
    };
    let (ws, _) = tokio::time::timeout(Duration::from_secs(20), fut)
        .await
        .map_err(|_| match &via {
            Some(p) => anyhow!("经代理 {}:{} 连接中继超时", p.host, p.port),
            None => anyhow!("连接中继超时"),
        })?
        .map_err(|e: anyhow::Error| e)?
        .map_err(|e| match e {
            tokio_tungstenite::tungstenite::Error::Http(resp) => {
                let body = resp.body().as_ref().map(|b| String::from_utf8_lossy(b).to_string()).unwrap_or_default();
                anyhow!("中继拒绝连接（HTTP {}）{}", resp.status(), body)
            }
            e => {
                let m = e.to_string();
                if m.contains("UnknownIssuer") || m.contains("invalid peer certificate") {
                    anyhow!("中继用的是自签名证书，需要提供证书指纹（yi-relay 启动日志里有）：{m}")
                } else {
                    anyhow!("连接中继失败: {m}")
                }
            }
        })?;
    Ok(ws)
}

/// Reason given by the relay when it closes the connection
fn close_error(f: Option<tokio_tungstenite::tungstenite::protocol::CloseFrame>) -> anyhow::Error {
    match f {
        Some(f) => {
            let code: u16 = f.code.into();
            anyhow!("中继拒绝：{}（{code}）", f.reason)
        }
        None => anyhow!("中继关闭了连接"),
    }
}

/// Connect to the relay and complete relay-layer authentication. `extra` carries stream / invite / ticket
pub(crate) async fn login(relay: &str, path: &str, fp: &str, me: &Identity, role: &str, device: &str, extra: Value) -> Result<Ws> {
    let mut ws = ws_connect(&url(relay, path), fp).await?;
    let challenge = tokio::time::timeout(Duration::from_secs(10), next_text(&mut ws))
        .await
        .map_err(|_| anyhow!("等待中继鉴权超时（中继版本太旧？）"))??;
    let v: Value = serde_json::from_str(&challenge).map_err(|_| anyhow!("中继的鉴权消息格式错误（中继版本太旧？）"))?;
    if v["type"] != "challenge" {
        bail!("中继的鉴权消息格式错误（中继版本太旧？）");
    }
    let nonce = unb64_32(v["nonce"].as_str().unwrap_or(""))?;
    let stream = extra["stream"].as_str().unwrap_or("");
    let sig = me.sign_msg(&auth::signed_payload(role, device, stream, &nonce));
    let mut msg = json!({"type": "auth", "role": role, "device": device, "pub": b64(&me.sign_pub()), "sig": b64(&sig)});
    if let Value::Object(m) = extra {
        for (k, v) in m {
            msg[k] = v;
        }
    }
    ws.send(Message::Text(msg.to_string().into())).await?;
    let reply = tokio::time::timeout(Duration::from_secs(15), next_text(&mut ws))
        .await
        .map_err(|_| anyhow!("等待中继鉴权结果超时"))??;
    let v: Value = serde_json::from_str(&reply).unwrap_or_default();
    if v["type"] != "ok" {
        bail!("中继鉴权失败：{reply}");
    }
    Ok(ws)
}

/// Next text message; when closed, includes the reason from the relay
async fn next_text(ws: &mut Ws) -> Result<String> {
    loop {
        match ws.next().await {
            Some(Ok(Message::Text(t))) => return Ok(t.to_string()),
            Some(Ok(Message::Close(f))) => return Err(close_error(f)),
            Some(Ok(_)) => continue,
            Some(Err(e)) => return Err(e.into()),
            None => bail!("中继连接已断开"),
        }
    }
}

// ---------------- yi-agentd's control connection ----------------

/// What yi-agentd needs to connect to the relay
#[derive(Debug, Clone)]
pub struct AgentLink {
    pub relay: String,
    pub fp: String,
    pub me: Identity,
}

impl AgentLink {
    pub fn device(&self) -> String {
        device_id_of(&self.me.sign_pub())
    }
}

/// Establish the control connection. `invite` is only needed for the first registration
pub async fn agent_control(a: &AgentLink, invite: &str) -> Result<Ws> {
    let extra = if invite.is_empty() { json!({}) } else { json!({"invite": invite}) };
    login(&a.relay, "/v1/agent", &a.fp, &a.me, auth::ROLE_AGENT, &a.device(), extra).await
}

/// The relay asks yi-agentd to open a stream for a client
#[derive(Debug, Clone)]
pub struct OpenReq {
    pub stream: String,
    /// The client's signing public key (verified by the relay)
    pub client: [u8; 32],
    /// Came in with a pairing ticket: the ticket itself, which yi-agentd uses to find the matching pairing code
    pub ticket: Option<[u8; 32]>,
}

#[derive(Debug)]
pub enum Control {
    Open(OpenReq),
    /// Result of a device-issued invite: Ok(invite code) / Err(reason)
    Invite(std::result::Result<String, String>),
    /// Heartbeats etc.
    Other,
}

/// Read the next message on the control connection. Callers wrap each read in a timeout and reconnect when heartbeats stop for too long.
pub async fn next_control(ws: &mut Ws) -> Result<Control> {
    match ws.next().await {
        Some(Ok(Message::Text(t))) => {
            let v: Value = serde_json::from_str(&t).unwrap_or_default();
            match v["type"].as_str() {
                Some("open") => {
                    let stream = v["stream"].as_str().unwrap_or("").to_string();
                    let client = unb64_32(v["client"].as_str().unwrap_or(""))?;
                    let ticket = match v["ticket"].as_str() {
                        Some(t) if !t.is_empty() => Some(unb64_32(t)?),
                        _ => None,
                    };
                    Ok(Control::Open(OpenReq { stream, client, ticket }))
                }
                Some("invite") => Ok(Control::Invite(match v["code"].as_str() {
                    Some(c) => Ok(c.to_string()),
                    None => Err(v["error"].as_str().unwrap_or("中继没有返回邀请码").to_string()),
                })),
                _ => Ok(Control::Other),
            }
        }
        Some(Ok(Message::Ping(_))) => {
            // tungstenite has already queued the Pong; flush so it actually goes out
            let _ = ws.flush().await;
            Ok(Control::Other)
        }
        Some(Ok(Message::Close(f))) => Err(close_error(f)),
        Some(Ok(_)) => Ok(Control::Other),
        Some(Err(e)) => Err(e.into()),
        None => bail!("中继控制连接断开"),
    }
}

/// Send a message on the control connection (acl allow list, invite request)
pub async fn send_control(ws: &mut Ws, v: &Value) -> Result<()> {
    ws.send(Message::Text(v.to_string().into())).await?;
    Ok(())
}
