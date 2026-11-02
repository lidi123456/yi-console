//! Local IPC: ~/.yi-console/yi-agentd.sock (0600) on Unix, a named pipe on Windows.
//! yi-agentd listens on no TCP ports; remote access goes through SSH exec `yi-agentd bridge` or the relay.

use crate::hub::Hub;
use anyhow::Result;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};

pub trait Duplex: AsyncRead + AsyncWrite + Unpin + Send + 'static {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> Duplex for T {}

#[cfg(unix)]
pub async fn connect() -> std::io::Result<impl Duplex> {
    tokio::net::UnixStream::connect(crate::paths::socket_path()).await
}

#[cfg(windows)]
pub async fn connect() -> std::io::Result<impl Duplex> {
    use tokio::net::windows::named_pipe::ClientOptions;
    let name = crate::paths::pipe_name();
    for _ in 0..20 {
        match ClientOptions::new().open(&name) {
            Ok(c) => return Ok(c),
            // All instances busy; wait a moment
            Err(e) if e.raw_os_error() == Some(231) => {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await
            }
            Err(e) if e.raw_os_error() == Some(5) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "守护进程以另一种权限运行（例如管理员），当前进程无权连接。\
                     请先结束已有的 yi-agentd.exe，再重新连接",
                ))
            }
            Err(e) => return Err(e),
        }
    }
    ClientOptions::new().open(&name)
}

/// The pipe is accessible only to the current user (by SID, regardless of running as admin) and SYSTEM;
/// the integrity level is set to Medium, so a daemon running as admin can still be reached by a normally privileged app
#[cfg(windows)]
mod pipe_sec {
    use std::ffi::c_void;
    use std::io;
    use std::ptr::null_mut;
    use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, HANDLE};
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{GetTokenInformation, TokenUser, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    pub struct PipeSecurity {
        sd: *mut c_void,
        pub attrs: SECURITY_ATTRIBUTES,
    }

    // The security descriptor is read-only after creation, so using it across threads is fine
    unsafe impl Send for PipeSecurity {}
    unsafe impl Sync for PipeSecurity {}

    impl Drop for PipeSecurity {
        fn drop(&mut self) {
            unsafe { LocalFree(self.sd) };
        }
    }

    fn user_sid() -> io::Result<String> {
        unsafe {
            let mut tok: HANDLE = null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut tok) == 0 {
                return Err(io::Error::last_os_error());
            }
            let mut len = 0u32;
            GetTokenInformation(tok, TokenUser, null_mut(), 0, &mut len);
            let mut buf = vec![0u8; len as usize];
            let ok = GetTokenInformation(tok, TokenUser, buf.as_mut_ptr().cast(), len, &mut len);
            CloseHandle(tok);
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            let tu = &*(buf.as_ptr() as *const TOKEN_USER);
            let mut s: *mut u16 = null_mut();
            if ConvertSidToStringSidW(tu.User.Sid, &mut s) == 0 {
                return Err(io::Error::last_os_error());
            }
            let n = (0..).take_while(|&i| *s.add(i) != 0).count();
            let sid = String::from_utf16_lossy(std::slice::from_raw_parts(s, n));
            LocalFree(s.cast());
            Ok(sid)
        }
    }

    pub fn new() -> io::Result<PipeSecurity> {
        let sddl = format!("D:P(A;;GA;;;{})(A;;GA;;;SY)S:(ML;;NW;;;ME)", user_sid()?);
        let w: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
        let mut sd: *mut c_void = null_mut();
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(w.as_ptr(), SDDL_REVISION_1, &mut sd, null_mut())
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(PipeSecurity {
            sd,
            attrs: SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: sd,
                bInheritHandle: 0,
            },
        })
    }
}

#[cfg(unix)]
pub async fn listen(hub: Arc<Hub>) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let path = crate::paths::socket_path();
    if path.exists() {
        // If we can connect, an instance is already running; if not, it's a leftover file
        if tokio::net::UnixStream::connect(&path).await.is_ok() {
            anyhow::bail!("yi-agentd 已在运行");
        }
        let _ = std::fs::remove_file(&path);
    }
    let listener = tokio::net::UnixListener::bind(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    crate::info!("listening on {}", path.display());
    loop {
        let (stream, _) = listener.accept().await?;
        let hub = hub.clone();
        tokio::spawn(async move {
            let (r, w) = tokio::io::split(stream);
            crate::server::serve_conn(hub, r, w, "local").await;
        });
    }
}

#[cfg(windows)]
pub async fn listen(hub: Arc<Hub>) -> Result<()> {
    use tokio::net::windows::named_pipe::ServerOptions;
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    let name = crate::paths::pipe_name();
    if connect().await.is_ok() {
        anyhow::bail!("yi-agentd 已在运行");
    }
    let sec = pipe_sec::new()?;
    let create = |first: bool| {
        let mut attrs = sec.attrs;
        // Safety: the security descriptor attrs points to is valid while sec lives, and create only reads it
        unsafe {
            ServerOptions::new()
                .first_pipe_instance(first)
                .reject_remote_clients(true)
                .create_with_security_attributes_raw(&name, (&mut attrs as *mut SECURITY_ATTRIBUTES).cast())
        }
    };
    let mut server = create(true)?;
    crate::info!("listening on {name}");
    loop {
        server.connect().await?;
        let client = server;
        server = create(false)?;
        let hub = hub.clone();
        tokio::spawn(async move {
            let (r, w) = tokio::io::split(client);
            crate::server::serve_conn(hub, r, w, "local").await;
        });
    }
}
