//! SSH：连接（可经跳板机）、主机密钥校验、认证、exec、SFTP。

use crate::hosts::SshTarget;
use anyhow::{anyhow, bail, Result};
use russh::client::{self, Handle};
use russh::keys::{self, HashAlg, PrivateKeyWithHashAlg, PublicKey};
use russh::ChannelMsg;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 主机密钥问题：UI 需要据此弹窗让用户确认
#[derive(Debug, Clone)]
pub enum HostKeyIssue {
    Unknown { fingerprint: String },
    Changed { fingerprint: String },
}

pub const ERR_HOSTKEY_UNKNOWN: &str = "HOSTKEY_UNKNOWN";
pub const ERR_HOSTKEY_CHANGED: &str = "HOSTKEY_CHANGED";
pub const ERR_AUTH: &str = "AUTH_FAILED";
/// 私钥读不出来（文件不存在、格式不支持、口令错误），重试没有意义
pub const ERR_KEY: &str = "KEY_INVALID";

pub struct KnownHosts {
    /// 客户端自己的 known_hosts（与 OpenSSH 同格式）
    pub app_file: PathBuf,
    /// 用户刚在弹窗里确认过的指纹：host:port -> fingerprint
    pub approved: Mutex<std::collections::HashMap<String, String>>,
}

struct H {
    host: String,
    port: u16,
    known: Arc<KnownHosts>,
    issue: Arc<Mutex<Option<HostKeyIssue>>>,
}

impl client::Handler for H {
    type Error = russh::Error;

    async fn check_server_key(&mut self, key: &PublicKey) -> Result<bool, Self::Error> {
        let fp = key.fingerprint(HashAlg::Sha256).to_string();
        // 1) 用户自己的 ~/.ssh/known_hosts（桌面端有）
        match keys::check_known_hosts(&self.host, self.port, key) {
            Ok(true) => return Ok(true),
            Err(keys::Error::KeyChanged { .. }) => {
                *self.issue.lock().unwrap() = Some(HostKeyIssue::Changed { fingerprint: fp });
                return Ok(false);
            }
            _ => {}
        }
        // 2) 客户端自己的 known_hosts
        match keys::check_known_hosts_path(&self.host, self.port, key, &self.known.app_file) {
            Ok(true) => return Ok(true),
            Err(keys::Error::KeyChanged { .. }) => {
                *self.issue.lock().unwrap() = Some(HostKeyIssue::Changed { fingerprint: fp });
                return Ok(false);
            }
            _ => {}
        }
        // 3) 用户刚确认过这个指纹
        let id = format!("{}:{}", self.host, self.port);
        let approved = self.known.approved.lock().unwrap().get(&id).cloned();
        if approved.as_deref() == Some(fp.as_str()) {
            if let Some(d) = self.known.app_file.parent() {
                let _ = std::fs::create_dir_all(d);
            }
            let _ = keys::known_hosts::learn_known_hosts_path(
                &self.host,
                self.port,
                key,
                &self.known.app_file,
            );
            return Ok(true);
        }
        *self.issue.lock().unwrap() = Some(HostKeyIssue::Unknown { fingerprint: fp });
        Ok(false)
    }
}

pub struct SshSession {
    handle: Handle<H>,
    /// 经跳板机时持有跳板会话，保证隧道不被释放
    _jump: Option<Box<SshSession>>,
}

fn config() -> Arc<client::Config> {
    Arc::new(client::Config {
        keepalive_interval: Some(Duration::from_secs(15)),
        keepalive_max: 3,
        inactivity_timeout: None,
        // 流式回复是一连串小包，开着 Nagle 会攒着等对端确认再发
        nodelay: true,
        ..Default::default()
    })
}

pub async fn connect(
    t: &SshTarget,
    jump: Option<&SshTarget>,
    known: Arc<KnownHosts>,
) -> Result<SshSession> {
    let issue = Arc::new(Mutex::new(None));
    let handler = H {
        host: t.host.clone(),
        port: t.port,
        known: known.clone(),
        issue: issue.clone(),
    };
    let (handle, jump_sess) = match jump {
        Some(j) => {
            let js = Box::pin(connect(j, None, known.clone())).await?;
            let ch = js
                .handle
                .channel_open_direct_tcpip(t.host.clone(), t.port as u32, "127.0.0.1", 0)
                .await
                .map_err(|e| anyhow!("跳板机无法连到 {}:{}: {e}", t.host, t.port))?;
            let r = client::connect_stream(config(), ch.into_stream(), handler).await;
            (r, Some(Box::new(js)))
        }
        None => {
            // 自动模式下公网地址会经代理（CONNECT 隧道），内网地址直连
            let r = tokio::time::timeout(Duration::from_secs(20), async {
                match yi_link::proxy::proxy_for_host(&t.host, true) {
                    Some(p) => {
                        // 公司代理常只放行 443，隧道建不起来时退回直连
                        let tcp = match yi_link::proxy::connect(&p, &t.host, t.port).await {
                            Ok(s) => s,
                            Err(pe) => tokio::net::TcpStream::connect((t.host.as_str(), t.port))
                                .await
                                .map_err(|e| anyhow!("{pe:#}；直连也失败: {e}"))?,
                        };
                        // connect_stream 不会替我们设 nodelay
                        let _ = tcp.set_nodelay(true);
                        Ok(client::connect_stream(config(), tcp, handler).await)
                    }
                    None => Ok::<_, anyhow::Error>(
                        client::connect(config(), (t.host.as_str(), t.port), handler).await,
                    ),
                }
            })
            .await
            .map_err(|_| anyhow!("连接 {}:{} 超时", t.host, t.port))??;
            (r, None)
        }
    };
    let mut handle = match handle {
        Ok(h) => h,
        Err(e) => {
            if let Some(i) = issue.lock().unwrap().clone() {
                return Err(match i {
                    HostKeyIssue::Unknown { fingerprint } => {
                        anyhow!("{ERR_HOSTKEY_UNKNOWN}:{fingerprint}")
                    }
                    HostKeyIssue::Changed { fingerprint } => {
                        anyhow!("{ERR_HOSTKEY_CHANGED}:{fingerprint}")
                    }
                });
            }
            return Err(anyhow!("SSH 连接失败: {e}"));
        }
    };
    authenticate(&mut handle, t).await?;
    Ok(SshSession {
        handle,
        _jump: jump_sess,
    })
}

async fn authenticate(h: &mut Handle<H>, t: &SshTarget) -> Result<()> {
    let user = t.user.clone();
    let pass = (!t.passphrase.is_empty()).then_some(t.passphrase.as_str());
    let ok = match t.auth.as_str() {
        "password" => h.authenticate_password(&user, &t.password).await?.success(),
        "key" => {
            let text = if !t.key_text.trim().is_empty() {
                t.key_text.clone()
            } else {
                let p = expand(&t.key_path);
                std::fs::read_to_string(&p)
                    .map_err(|e| anyhow!("{ERR_KEY}:读取私钥失败: {}: {e}", p.display()))?
            };
            let key = decode_key(&text, pass).map_err(|e| anyhow!("{ERR_KEY}:读取私钥失败: {e}"))?;
            let hash = h.best_supported_rsa_hash().await?.flatten();
            h.authenticate_publickey(&user, PrivateKeyWithHashAlg::new(Arc::new(key), hash))
                .await?
                .success()
        }
        _ => auth_agent(h, &user).await? || auth_default_keys(h, &user).await?,
    };
    if !ok {
        bail!("{ERR_AUTH}:认证失败（用户 {user}，方式 {}）", t.auth);
    }
    Ok(())
}

#[cfg(unix)]
async fn auth_agent(h: &mut Handle<H>, user: &str) -> Result<bool> {
    let Ok(mut agent) = keys::agent::client::AgentClient::connect_env().await else {
        return Ok(false);
    };
    let ids = agent.request_identities().await.unwrap_or_default();
    let hash = h.best_supported_rsa_hash().await?.flatten();
    for key in ids {
        if let Ok(r) = h
            .authenticate_publickey_with(user, key, hash, &mut agent)
            .await
        {
            if r.success() {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

#[cfg(windows)]
async fn auth_agent(h: &mut Handle<H>, user: &str) -> Result<bool> {
    // Windows 自带 OpenSSH 的 ssh-agent 走命名管道
    let Ok(mut agent) =
        keys::agent::client::AgentClient::connect_named_pipe(r"\\.\pipe\openssh-ssh-agent").await
    else {
        return Ok(false);
    };
    let ids = agent.request_identities().await.unwrap_or_default();
    let hash = h.best_supported_rsa_hash().await?.flatten();
    for key in ids {
        if let Ok(r) = h
            .authenticate_publickey_with(user, key, hash, &mut agent)
            .await
        {
            if r.success() {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

#[cfg(not(any(unix, windows)))]
async fn auth_agent(_h: &mut Handle<H>, _user: &str) -> Result<bool> {
    Ok(false)
}

/// 没有 agent 时，像 OpenSSH 一样试默认私钥（无口令的）
async fn auth_default_keys(h: &mut Handle<H>, user: &str) -> Result<bool> {
    let Some(home) = dirs::home_dir() else {
        return Ok(false);
    };
    for name in ["id_ed25519", "id_ecdsa", "id_rsa"] {
        let p = home.join(".ssh").join(name);
        if !p.is_file() {
            continue;
        }
        let Some(key) = std::fs::read_to_string(&p).ok().and_then(|t| decode_key(&t, None).ok()) else {
            continue;
        };
        let hash = h.best_supported_rsa_hash().await?.flatten();
        if h
            .authenticate_publickey(user, PrivateKeyWithHashAlg::new(Arc::new(key), hash))
            .await?
            .success()
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// 解析私钥。Windows 上 ssh-keygen 生成的密钥，注释（默认是"用户名@计算机名"）可能是 GBK 编码，
/// 解析库要求注释是 UTF-8 会直接报错；注释登录时用不到，遇到这种情况去掉注释再解析
fn decode_key(text: &str, pass: Option<&str>) -> Result<keys::PrivateKey, keys::Error> {
    match keys::decode_secret_key(text, pass) {
        Err(e) if format!("{e:?}").contains("CharacterEncoding") => {
            match strip_openssh_comment(text) {
                Some(fixed) => keys::decode_secret_key(&fixed, pass),
                None => Err(e),
            }
        }
        r => r,
    }
}

/// 把未加密的 OpenSSH 私钥（openssh-key-v1）里的注释换成空串。加密的私钥注释也在密文里，处理不了
fn strip_openssh_comment(text: &str) -> Option<String> {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    const BEGIN: &str = "-----BEGIN OPENSSH PRIVATE KEY-----";
    const END: &str = "-----END OPENSSH PRIVATE KEY-----";
    let body = text.split(BEGIN).nth(1)?.split(END).next()?;
    let b64: String = body.chars().filter(|c| !c.is_whitespace()).collect();
    let d = STANDARD.decode(b64).ok()?;

    struct R<'a>(&'a [u8], usize);
    impl<'a> R<'a> {
        fn u32(&mut self) -> Option<usize> {
            let b = self.0.get(self.1..self.1 + 4)?;
            self.1 += 4;
            Some(u32::from_be_bytes(b.try_into().ok()?) as usize)
        }
        fn bytes(&mut self) -> Option<&'a [u8]> {
            let n = self.u32()?;
            let b = self.0.get(self.1..self.1 + n)?;
            self.1 += n;
            Some(b)
        }
    }
    let magic = b"openssh-key-v1\0";
    if !d.starts_with(magic) {
        return None;
    }
    let mut r = R(&d, magic.len());
    if r.bytes()? != b"none" {
        return None;
    }
    r.bytes()?; // kdfname
    r.bytes()?; // kdfoptions
    if r.u32()? != 1 {
        return None;
    }
    r.bytes()?; // 公钥
    let head_end = r.1;
    let private = r.bytes()?;

    let mut p = R(private, 8); // 两个 checkint
    let kind = p.bytes()?;
    let fields = match kind {
        b"ssh-rsa" => 6,
        b"ssh-ed25519" => 2,
        k if k.starts_with(b"ecdsa-sha2-") => 3,
        _ => return None,
    };
    for _ in 0..fields {
        p.bytes()?;
    }
    let mut block = private[..p.1].to_vec();
    block.extend_from_slice(&0u32.to_be_bytes()); // 空注释
    let mut pad = 1u8;
    while block.len() % 8 != 0 {
        block.push(pad);
        pad += 1;
    }
    let mut out = d[..head_end].to_vec();
    out.extend_from_slice(&(block.len() as u32).to_be_bytes());
    out.extend_from_slice(&block);
    let enc = STANDARD.encode(out);
    let lines: Vec<&str> = enc
        .as_bytes()
        .chunks(70)
        .map(|c| std::str::from_utf8(c).unwrap_or(""))
        .collect();
    Some(format!("{BEGIN}\n{}\n{END}\n", lines.join("\n")))
}

fn expand(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(h) = dirs::home_dir() {
            return h.join(rest);
        }
    }
    PathBuf::from(p)
}

pub struct ExecOutput {
    pub code: Option<u32>,
    pub stdout: String,
    pub stderr: String,
}

impl SshSession {
    pub async fn exec(&self, cmd: &str) -> Result<ExecOutput> {
        let mut ch = self.handle.channel_open_session().await?;
        ch.exec(true, cmd).await?;
        let (mut out, mut err, mut code) = (vec![], vec![], None);
        let fut = async {
            while let Some(msg) = ch.wait().await {
                match msg {
                    ChannelMsg::Data { data } => out.extend_from_slice(&data),
                    ChannelMsg::ExtendedData { data, .. } => err.extend_from_slice(&data),
                    ChannelMsg::ExitStatus { exit_status } => code = Some(exit_status),
                    _ => {}
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(120), fut)
            .await
            .map_err(|_| anyhow!("远程命令超时: {cmd}"))?;
        Ok(ExecOutput {
            code,
            stdout: String::from_utf8_lossy(&out).to_string(),
            stderr: String::from_utf8_lossy(&err).to_string(),
        })
    }

    /// exec 一个长期运行的命令，返回其 stdin/stdout 字节流
    pub async fn exec_stream(
        &self,
        cmd: &str,
    ) -> Result<russh::ChannelStream<client::Msg>> {
        let ch = self.handle.channel_open_session().await?;
        ch.exec(true, cmd).await?;
        Ok(ch.into_stream())
    }

    /// 经 SSH 连远程机器上的 127.0.0.1:port（端口转发；sshd 关了 AllowTcpForwarding 时会失败）
    pub async fn direct_tcpip(&self, port: u16) -> Result<russh::ChannelStream<client::Msg>> {
        let ch = self
            .handle
            .channel_open_direct_tcpip("127.0.0.1", port as u32, "127.0.0.1", 0)
            .await
            .map_err(|e| anyhow!("SSH 端口转发被拒绝: {e}"))?;
        Ok(ch.into_stream())
    }

    pub async fn sftp(&self) -> Result<russh_sftp::client::SftpSession> {
        let ch = self.handle.channel_open_session().await?;
        ch.request_subsystem(true, "sftp").await?;
        Ok(russh_sftp::client::SftpSession::new(ch.into_stream()).await?)
    }

    pub fn is_closed(&self) -> bool {
        self.handle.is_closed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试专用密钥（ssh-keygen -t ed25519 -C <GBK 编码的"测试@电脑">），不对应任何主机
    const GBK_COMMENT_KEY: &str = "\
-----BEGIN OPENSSH PRIVATE KEY-----\n\
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW\n\
QyNTUxOQAAACCdEA+E41wgiEtJMuoe+LBSbD2SRSTWUsj8AHgYAiZVCwAAAJCq6uhQquro\n\
UAAAAAtzc2gtZWQyNTUxOQAAACCdEA+E41wgiEtJMuoe+LBSbD2SRSTWUsj8AHgYAiZVCw\n\
AAAEBFtlVUVD14fFm+2t0rko+Q3HN0Ew6aLuXwKoIyg0ACcJ0QD4TjXCCIS0ky6h74sFJs\n\
PZJFJNZSyPwAeBgCJlULAAAACbLiytRAtefE1AECAwQ=\n\
-----END OPENSSH PRIVATE KEY-----\n";

    #[test]
    fn key_with_gbk_comment() {
        assert!(keys::decode_secret_key(GBK_COMMENT_KEY, None).is_err());
        let k = decode_key(GBK_COMMENT_KEY, None).unwrap();
        assert_eq!(k.algorithm().to_string(), "ssh-ed25519");
    }

    /// 手动跑：AH_SSH_TEST=host:port:user:私钥路径:要转发的远程端口 cargo test -p yi-client direct_tcpip -- --ignored
    /// 连上后经 direct-tcpip 读远程端口的第一段数据（转发到对方的 sshd 时应该读到 SSH- 开头的握手）
    #[tokio::test]
    #[ignore]
    async fn direct_tcpip_live() {
        let Ok(spec) = std::env::var("AH_SSH_TEST") else { return };
        let f: Vec<&str> = spec.split(':').collect();
        let t = SshTarget {
            host: f[0].into(),
            port: f[1].parse().unwrap(),
            user: f[2].into(),
            auth: "key".into(),
            key_path: f[3].into(),
            key_text: String::new(),
            passphrase: String::new(),
            password: String::new(),
            jump_host_id: String::new(),
        };
        let dir = tempfile::tempdir().unwrap();
        let known = Arc::new(KnownHosts { app_file: dir.path().join("kh"), approved: Mutex::new(Default::default()) });
        let s = match connect(&t, None, known.clone()).await {
            Ok(s) => s,
            Err(e) => {
                let m = e.to_string();
                let fp = m.strip_prefix(&format!("{ERR_HOSTKEY_UNKNOWN}:")).unwrap_or_else(|| panic!("{m}")).to_string();
                known.approved.lock().unwrap().insert(format!("{}:{}", t.host, t.port), fp);
                connect(&t, None, known).await.unwrap()
            }
        };
        let mut st = s.direct_tcpip(f[4].parse().unwrap()).await.unwrap();
        let mut b = [0u8; 32];
        use tokio::io::AsyncReadExt;
        let n = tokio::time::timeout(Duration::from_secs(10), st.read(&mut b)).await.unwrap().unwrap();
        let got = String::from_utf8_lossy(&b[..n]).to_string();
        println!("got: {got:?}");
        assert!(got.starts_with("SSH-"), "{got}");
    }
}
