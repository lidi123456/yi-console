//! Single-tool multi-account store, credential snapshots, env resolution, and failover selection.
//!
//! Credentials live strictly host-side under `<YI_CONSOLE_HOME>/accounts.json` (`0o600`) and
//! `<YI_CONSOLE_HOME>/accounts/<tool>/<id>/`.
//! - Isolated accounts (`Codex`, `Claude`, `Qoder`, and any tool's `ApiKey` profile) run with per-profile
//!   env vars / config dirs so multiple sessions run concurrently with different accounts.
//! - `HostActive` accounts (`Antigravity` OAuth, `Cursor`/`Kiro`/`Codebuddy` OAuth)
//!   capture and atomically swap host credential files + OS keyring snapshots with rollback.

use crate::keyring::{self, KeyringEntry};
use anyhow::{anyhow, bail, Result};
use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use yi_proto::{
    AccountAuthKind, AccountIdentity, AccountIsolation, AccountProfileView, AccountState,
    ActualAuth, ApiKeyConfig, ToolAccountsSummary, ToolKind, ToolNativeStatus,
};

const REDACTED: &str = "********";
const FILE_VERSION: u32 = 1;

/// CodeBuddy Code's login (checked on 2.167.0), relative to the home dir: `<data dir>/CodeBuddyExtension/Data/Public/auth/
/// <product.json authentication.id>.info`, with the data dir picked by OS from the home dir (not APPDATA / XDG).
/// The same file holds the CodeBuddy IDE extension's login, so switching accounts switches both.
/// `state` rows of Kiro's database that belong to the login (Identity Center start URL and region, selected profile)
const KIRO_STATE_KEYS: [&str; 3] = ["auth.idc.start-url", "auth.idc.region", "api.codewhisperer.profile"];

/// Emails `kiro-cli whoami` reported, by a hash of the token rows (Kiro's login rows don't hold the email)
static KIRO_EMAILS: Mutex<BTreeMap<String, String>> = Mutex::new(BTreeMap::new());

fn kiro_token_key(snap: &SnapshotBundle) -> Option<String> {
    let mut h = Sha256::new();
    let mut any = false;
    for r in snap.db_rows.iter().filter(|r| r.table == "auth_kv" && r.key.ends_with(":token")) {
        h.update(r.key.as_bytes());
        h.update(r.text.as_deref().unwrap_or_default().as_bytes());
        any = true;
    }
    any.then(|| h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

fn kiro_cached_email(snap: &SnapshotBundle) -> Option<String> {
    KIRO_EMAILS.lock().unwrap().get(&kiro_token_key(snap)?).cloned()
}

/// Ask `kiro-cli whoami --format json` who is signed in, with HOME and the data dirs pointed at `home`
/// (None: the host's own). Blocking, at most 15s.
fn run_kiro_whoami(home: Option<&Path>) -> Option<String> {
    let program = crate::tools::resolve(ToolKind::Kiro, &yi_proto::DaemonSettings::default())?;
    let args = ["whoami".to_string(), "--format".to_string(), "json".to_string()];
    let mut cmd = crate::adapters::std_command(&program, &args);
    if let Some(h) = home {
        cmd.env("HOME", h)
            .env("USERPROFILE", h)
            .env("APPDATA", h.join("AppData/Roaming"))
            .env("LOCALAPPDATA", h.join("AppData/Local"))
            .env("XDG_DATA_HOME", h.join(".local/share"))
            .env("XDG_CONFIG_HOME", h.join(".config"))
            .env_remove("HOMEDRIVE")
            .env_remove("HOMEPATH");
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let mut child = cmd.spawn().ok()?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < deadline => std::thread::sleep(std::time::Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let mut out = String::new();
    std::io::Read::read_to_string(&mut child.stdout.take()?, &mut out).ok()?;
    let v: serde_json::Value = serde_json::from_str(out.lines().find(|l| l.trim_start().starts_with('{'))?).ok()?;
    v["email"].as_str().filter(|s| !s.is_empty()).map(str::to_string)
}

const CODEBUDDY_AUTH_FILE: &str = if cfg!(windows) {
    "AppData/Local/CodeBuddyExtension/Data/Public/auth/Tencent-Cloud.coding-copilot.info"
} else if cfg!(target_os = "macos") {
    "Library/Application Support/CodeBuddyExtension/Data/Public/auth/Tencent-Cloud.coding-copilot.info"
} else {
    ".local/share/CodeBuddyExtension/Data/Public/auth/Tencent-Cloud.coding-copilot.info"
};

#[derive(Debug, Clone, Copy)]
pub struct ToolCapabilities {
    pub oauth_isolation: AccountIsolation,
    pub supports_oauth: bool,
    pub supports_api_key: bool,
    pub supports_device_auth: bool,
    pub supports_interactive_login: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AccountsFile {
    #[serde(default = "default_version")]
    version: u32,
    #[serde(default)]
    tools: BTreeMap<String, ToolAccountsData>,
}

fn default_version() -> u32 {
    FILE_VERSION
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ToolAccountsData {
    #[serde(default = "yes")]
    auto_failover: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    active_account_id: Option<String>,
    #[serde(default)]
    accounts: Vec<StoredAccount>,
}

fn yes() -> bool {
    true
}

impl Default for ToolAccountsData {
    fn default() -> Self {
        Self {
            auto_failover: true,
            active_account_id: None,
            accounts: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredAccount {
    pub id: String,
    pub tool: ToolKind,
    pub label: String,
    pub auth_kind: AccountAuthKind,
    #[serde(default)]
    pub allow_auto_failover: bool,
    #[serde(default)]
    pub priority: i32,
    #[serde(default)]
    pub identity: AccountIdentity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_config: Option<ApiKeyConfig>,
    #[serde(default)]
    pub updated_at: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotBundle {
    /// Paths relative to the host home directory -> base64 raw bytes.
    #[serde(default)]
    pub files: BTreeMap<String, String>,
    #[serde(default)]
    pub keyring: Vec<KeyringEntry>,
    /// Isolated home files relative to the profile's `home/` dir (used for cross-host sync).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub profile_files: BTreeMap<String, String>,
    /// Login rows of a tool that keeps it in SQLite (Kiro's `data.sqlite3`)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub db_rows: Vec<DbRow>,
}

/// One row of `table` (`key` -> `value`); TEXT values in `text`, BLOB values base64 in `blob`
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DbRow {
    pub table: String,
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob: Option<String>,
}

impl SnapshotBundle {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty() && self.keyring.is_empty() && self.profile_files.is_empty() && self.db_rows.is_empty()
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedAccount {
    pub id: String,
    pub label: String,
    pub auth_kind: AccountAuthKind,
    pub isolation: AccountIsolation,
    pub identity: AccountIdentity,
    pub env: BTreeMap<String, String>,
    pub env_unset: Vec<String>,
    pub data_dir: Option<PathBuf>,
    pub auth_method: Option<String>,
    pub fingerprint: String,
}

#[derive(Debug, Clone)]
pub struct SaveAccountRequest {
    pub tool: ToolKind,
    pub id: Option<String>,
    pub label: Option<String>,
    pub auth_kind: Option<AccountAuthKind>,
    pub allow_auto_failover: Option<bool>,
    pub priority: Option<i32>,
    pub api_config: Option<ApiKeyConfig>,
    pub verified_identity: Option<AccountIdentity>,
    pub activate: bool,
}

impl Default for SaveAccountRequest {
    fn default() -> Self {
        Self {
            tool: ToolKind::Claude,
            id: None,
            label: None,
            auth_kind: None,
            allow_auto_failover: None,
            priority: None,
            api_config: None,
            verified_identity: None,
            activate: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ActivateOutcome {
    pub account: AccountProfileView,
    pub host_switched: bool,
}

pub struct AccountManager {
    base_dir: PathBuf,
    home_dir: PathBuf,
    data: Mutex<AccountsFile>,
    runtime: Mutex<HashMap<(ToolKind, String), (AccountState, Option<String>)>>,
    host_lock: Mutex<()>,
    /// Host environment as seen by tests (None: the real one, `tools::host_env_var`)
    test_env: Mutex<Option<HashMap<String, String>>>,
}

impl AccountManager {
    pub fn new() -> Self {
        Self::with_dirs(crate::paths::base_dir(), crate::paths::home())
    }

    pub fn with_dirs(base_dir: PathBuf, home_dir: PathBuf) -> Self {
        let path = base_dir.join("accounts.json");
        let data: AccountsFile = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Self {
            base_dir,
            home_dir,
            data: Mutex::new(data),
            runtime: Mutex::new(HashMap::new()),
            host_lock: Mutex::new(()),
            test_env: Mutex::new(None),
        }
    }

    /// A host environment variable (Windows: also the registry, so values set after the daemon started count)
    fn host_env(&self, name: &str) -> Option<String> {
        if let Some(env) = self.test_env.lock().unwrap().as_ref() {
            return env.get(name).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        }
        crate::tools::host_env_var(name)
    }

    #[cfg(test)]
    fn set_test_env(&self, vars: &[(&str, &str)]) {
        *self.test_env.lock().unwrap() = Some(vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect());
    }

    /// What a Codex home's config.toml makes Codex use, when it's a custom provider. `stored_key`: the account holds the key itself
    fn codex_actual(&self, home: &Path, stored_key: Option<&str>) -> Option<ActualAuth> {
        let p = codex_provider(&std::fs::read_to_string(home.join("config.toml")).ok()?)?;
        let key_set = stored_key.is_some_and(|k| !k.is_empty()) || p.env_key.as_deref().and_then(|k| self.host_env(k)).is_some();
        Some(codex_actual_auth(p, key_set))
    }

    fn host_env_for(&self, actual: &ActualAuth) -> Option<String> {
        actual.env_key.as_deref().and_then(|k| self.host_env(k))
    }

    /// A Codex account saved as one kind while its config.toml says another. `ApiKey`: keep the custom provider and store
    /// its key (from the host variable named by `env_key`), so the account shows and fails over as what it is; the ChatGPT
    /// login is kept in `auth.json.oauth-bak`. `Oauth`: drop the provider selection so the ChatGPT login is used (restoring that backup)
    pub fn convert_codex(&self, account_id: &str, to: AccountAuthKind) -> Result<AccountProfileView> {
        let home = self.profile_home(ToolKind::Codex, account_id);
        let backup = home.join("auth.json.oauth-bak");
        let mut g = self.data.lock().unwrap();
        let td = g.tools.entry(ToolKind::Codex.as_str().to_string()).or_default();
        let labels: Vec<String> = td.accounts.iter().filter(|a| a.id != account_id).map(|a| a.label.clone()).collect();
        let acc = td.accounts.iter_mut().find(|a| a.id == account_id).ok_or_else(|| anyhow!("账号 {account_id} 不存在"))?;
        match to {
            AccountAuthKind::ApiKey => {
                if acc.auth_kind == AccountAuthKind::ApiKey {
                    bail!("这个账号已经是 API Key 账号");
                }
                let Some(actual) = self.codex_actual(&home, None) else {
                    bail!("这个账号的 config.toml 没有选择自定义 provider，它使用的就是 ChatGPT 授权");
                };
                let Some(env_key) = actual.env_key.clone() else {
                    bail!("provider {} 没有配置 env_key，无法转为 API Key 账号", actual.provider);
                };
                let Some(key) = self.host_env(&env_key) else {
                    bail!("环境变量 {env_key} 未设置，无法转为 API Key 账号");
                };
                let auth = home.join("auth.json");
                if auth.is_file() && !backup.exists() {
                    std::fs::copy(&auth, &backup)?;
                    crate::util::restrict_file(&backup);
                }
                let cfg = ApiKeyConfig {
                    api_key: key,
                    base_url: actual.base_url.clone().unwrap_or_default(),
                    provider_name: actual.provider.clone(),
                    ..Default::default()
                };
                ensure_codex_profile_files(&home, Some(&cfg))?;
                // A label that was the ChatGPT email would now name the wrong thing
                if acc.identity.email.as_deref() == Some(acc.label.as_str()) || acc.label.is_empty() {
                    let names: Vec<&str> = labels.iter().map(String::as_str).collect();
                    acc.label = unique_label(format!("codex ({})", actual.provider), &names);
                }
                acc.auth_kind = AccountAuthKind::ApiKey;
                acc.identity = AccountIdentity { plan: Some(actual.provider.clone()), workspace: actual.base_url.clone(), ..Default::default() };
                acc.api_config = Some(cfg);
            }
            AccountAuthKind::Oauth => {
                if acc.auth_kind == AccountAuthKind::ApiKey {
                    let raw = std::fs::read(&backup).map_err(|_| anyhow!("这个 API Key 账号没有保存过 ChatGPT 授权，请用「重新登录」或抓取当前登录"))?;
                    let auth = home.join("auth.json");
                    std::fs::write(&auth, &raw)?;
                    crate::util::restrict_file(&auth);
                    let _ = std::fs::remove_file(&backup);
                    acc.identity = parse_codex_auth_bytes(&raw).1;
                    acc.auth_kind = AccountAuthKind::Oauth;
                    acc.api_config = None;
                } else if self.codex_actual(&home, None).is_none() {
                    bail!("这个账号已经在使用 ChatGPT 授权");
                }
                let cfg_path = home.join("config.toml");
                let raw = std::fs::read_to_string(&cfg_path).unwrap_or_default();
                std::fs::write(&cfg_path, strip_codex_provider(&raw))?;
                crate::util::restrict_file(&cfg_path);
            }
        }
        acc.updated_at = crate::util::now_ms();
        // The quota cached for the old kind (ChatGPT plan limits) isn't this account's any more
        let _ = std::fs::remove_file(self.account_dir(ToolKind::Codex, account_id).join("quota.json"));
        let stored = acc.clone();
        let active = td.active_account_id.clone();
        self.save_file(&g)?;
        drop(g);
        self.set_runtime_state(ToolKind::Codex, account_id, AccountState::Ready, None);
        Ok(self.to_view(&stored, active.as_deref(), 0, None))
    }

    pub fn capability(tool: ToolKind) -> ToolCapabilities {
        match tool {
            ToolKind::Codex => ToolCapabilities {
                oauth_isolation: AccountIsolation::Isolated,
                supports_oauth: true,
                supports_api_key: true,
                supports_device_auth: true,
                supports_interactive_login: true,
            },
            ToolKind::Claude => ToolCapabilities {
                oauth_isolation: AccountIsolation::Isolated,
                supports_oauth: true,
                supports_api_key: true,
                supports_device_auth: false,
                supports_interactive_login: true,
            },
            ToolKind::Antigravity => ToolCapabilities {
                oauth_isolation: AccountIsolation::HostActive,
                supports_oauth: true,
                supports_api_key: true,
                supports_device_auth: false,
                supports_interactive_login: true,
            },
            ToolKind::Cursor => ToolCapabilities {
                oauth_isolation: AccountIsolation::HostActive,
                supports_oauth: true,
                supports_api_key: true,
                supports_device_auth: false,
                supports_interactive_login: true,
            },
            // Each account runs with its own `QODER_CONFIG_DIR` (login, settings, sessions; nothing shared with ~/.qoder)
            ToolKind::Qoder => ToolCapabilities {
                oauth_isolation: AccountIsolation::Isolated,
                supports_oauth: true,
                supports_api_key: true,
                supports_device_auth: false,
                supports_interactive_login: true,
            },
            // `kiro-cli login` opens an arrow-key menu (Builder ID / Google / GitHub / organization) a pipe can't drive;
            // `--license free --use-device-flow` goes straight to the Builder ID device code. Other sign-ins: capture
            ToolKind::Kiro => ToolCapabilities {
                oauth_isolation: AccountIsolation::HostActive,
                supports_oauth: true,
                supports_api_key: true,
                supports_device_auth: true,
                supports_interactive_login: false,
            },
            // CodeBuddy has no `login` subcommand (sign-in is `/login` inside its TUI, a menu a pipe can't drive):
            // accounts come from capturing the host login
            ToolKind::Codebuddy => ToolCapabilities {
                oauth_isolation: AccountIsolation::HostActive,
                supports_oauth: true,
                supports_api_key: true,
                supports_device_auth: false,
                supports_interactive_login: false,
            },
            ToolKind::Shell | ToolKind::Files => ToolCapabilities {
                oauth_isolation: AccountIsolation::HostActive,
                supports_oauth: false,
                supports_api_key: false,
                supports_device_auth: false,
                supports_interactive_login: false,
            },
        }
    }

    pub fn effective_isolation(tool: ToolKind, auth_kind: AccountAuthKind) -> AccountIsolation {
        match auth_kind {
            AccountAuthKind::ApiKey => AccountIsolation::Isolated,
            AccountAuthKind::Oauth => Self::capability(tool).oauth_isolation,
        }
    }

    pub fn new_account_id() -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let pid = std::process::id();
        static CTR: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let n = CTR.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        format!("acc_{:x}_{:x}_{}", (nanos as u64) & 0xffffffff, pid, n)
    }

    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    pub fn account_dir(&self, tool: ToolKind, account_id: &str) -> PathBuf {
        self.base_dir
            .join("accounts")
            .join(tool.as_str())
            .join(account_id)
    }

    pub fn profile_home(&self, tool: ToolKind, account_id: &str) -> PathBuf {
        self.account_dir(tool, account_id).join("home")
    }

    fn snapshot_path(&self, tool: ToolKind, account_id: &str) -> PathBuf {
        self.account_dir(tool, account_id).join("snapshot.json")
    }

    fn save_file(&self, file: &AccountsFile) -> Result<()> {
        std::fs::create_dir_all(&self.base_dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&self.base_dir, std::fs::Permissions::from_mode(0o700));
        }
        let path = self.base_dir.join("accounts.json");
        let tmp = self.base_dir.join("accounts.json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(file)?)?;
        crate::util::restrict_file(&tmp);
        std::fs::rename(&tmp, &path)?;
        crate::util::restrict_file(&path);
        Ok(())
    }

    fn save_snapshot(&self, tool: ToolKind, account_id: &str, snap: &SnapshotBundle) -> Result<()> {
        let dir = self.account_dir(tool, account_id);
        std::fs::create_dir_all(&dir)?;
        let path = self.snapshot_path(tool, account_id);
        let tmp = dir.join("snapshot.json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(snap)?)?;
        crate::util::restrict_file(&tmp);
        std::fs::rename(&tmp, &path)?;
        crate::util::restrict_file(&path);
        Ok(())
    }

    fn load_snapshot(&self, tool: ToolKind, account_id: &str) -> Option<SnapshotBundle> {
        let path = self.snapshot_path(tool, account_id);
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
    }

    /// Load Cursor's saved OAuth `auth.json` bytes if the snapshot exists and wasn't polluted with `apiKey`.
    pub fn load_cursor_oauth_auth_bytes(&self, account_id: &str) -> Option<Vec<u8>> {
        let snap = self.load_snapshot(ToolKind::Cursor, account_id)?;
        if cursor_snapshot_is_api_key(&snap) {
            return None;
        }
        let bytes = snap
            .files
            .iter()
            .find(|(k, _)| k.ends_with("auth.json"))
            .and_then(|(_, b64)| base64::engine::general_purpose::STANDARD.decode(b64).ok())?;
        if cursor_bytes_are_api_key(&bytes) {
            return None;
        }
        Some(bytes)
    }

    pub fn has_clean_cursor_oauth_snapshot(&self, account_id: &str) -> bool {
        self.load_snapshot(ToolKind::Cursor, account_id)
            .is_some_and(|s| !s.is_empty() && !cursor_snapshot_is_api_key(&s))
    }

    /// Ensure host Cursor credentials match the saved clean OAuth snapshot for `account_id`,
    /// recovering automatically if an external `CURSOR_API_KEY` process polluted `auth.json` or Keychain,
    /// and rejecting snapshots that are themselves API-key credentials.
    pub fn ensure_cursor_oauth_host_clean(&self, account_id: &str) -> Result<()> {
        let _guard = self.host_lock.lock().unwrap();
        let Some(clean) = self.load_snapshot(ToolKind::Cursor, account_id) else {
            bail!("当前 Cursor 授权账号缺少登录快照，请在账号管理中点击「重新登录」完成网页授权");
        };
        if clean.is_empty() || cursor_snapshot_is_api_key(&clean) {
            bail!("当前 Cursor 授权账号保存的凭据实际为 API Key 而非网页授权登录，请在账号管理中点击「重新登录」完成授权");
        }
        let Ok(cur) = self.take_host_snapshot_unlocked(ToolKind::Cursor) else {
            return Ok(());
        };
        if cur.is_empty() || cursor_snapshot_is_api_key(&cur) {
            self.restore_host_snapshot_unlocked(ToolKind::Cursor, &clean)?;
        }
        Ok(())
    }

    /// Restore a Cursor OAuth profile's host `auth.json` if a previous `CURSOR_API_KEY` process overwrote it.
    pub fn restore_cursor_oauth_if_api_key_polluted(&self, account_id: &str) {
        let _ = self.ensure_cursor_oauth_host_clean(account_id);
    }

    pub fn update_identity_if_missing(
        &self,
        tool: ToolKind,
        account_id: &str,
        email: Option<&str>,
        plan: Option<&str>,
    ) {
        let email = email.map(str::trim).filter(|s| !s.is_empty());
        let plan = plan.map(str::trim).filter(|s| !s.is_empty());
        if email.is_none() && plan.is_none() {
            return;
        }
        let mut g = self.data.lock().unwrap();
        let Some(td) = g.tools.get_mut(tool.as_str()) else {
            return;
        };
        let others: Vec<String> = td.accounts.iter().filter(|a| a.id != account_id).map(|a| a.label.clone()).collect();
        let Some(acc) = td.accounts.iter_mut().find(|a| a.id == account_id) else {
            return;
        };
        let mut changed = false;
        if acc.identity.email.is_none() {
            if let Some(e) = email {
                acc.identity.email = Some(e.to_string());
                if acc.identity.account_key.is_none() {
                    acc.identity.account_key = Some(e.to_string());
                }
                // Tools whose credentials carry no email (Qoder, Kiro) get a placeholder label at capture;
                // name the account after the email once a probe reports it
                if acc.auth_kind == AccountAuthKind::Oauth && is_placeholder_oauth_label(&acc.label, tool, acc.identity.workspace.as_deref()) {
                    let names: Vec<&str> = others.iter().map(String::as_str).collect();
                    acc.label = unique_label(e.to_string(), &names);
                }
                changed = true;
            }
        }
        if acc.identity.plan.is_none() {
            if let Some(p) = plan {
                acc.identity.plan = Some(p.to_string());
                changed = true;
            }
        }
        if changed {
            let _ = self.save_file(&g);
        }
    }

    pub fn summary(&self, tool: ToolKind) -> (usize, Option<String>, Option<String>) {
        if !tool.supports_accounts() {
            return (0, None, None);
        }
        let g = self.data.lock().unwrap();
        let Some(td) = g.tools.get(tool.as_str()) else {
            return (0, None, None);
        };
        let count = td.accounts.len();
        let active = td
            .active_account_id
            .as_deref()
            .and_then(|id| td.accounts.iter().find(|a| a.id == id))
            .or_else(|| td.accounts.first());
        (
            count,
            active.map(|a| a.id.clone()),
            active.map(|a| a.label.clone()),
        )
    }

    #[allow(dead_code)]
    pub fn tool_auto_failover(&self, tool: ToolKind) -> bool {
        let g = self.data.lock().unwrap();
        g.tools
            .get(tool.as_str())
            .map(|t| t.auto_failover)
            .unwrap_or(true)
    }

    pub fn active_account_id(&self, tool: ToolKind) -> Option<String> {
        let g = self.data.lock().unwrap();
        g.tools
            .get(tool.as_str())
            .and_then(|t| t.active_account_id.clone())
    }

    pub fn get_stored(&self, tool: ToolKind, account_id: &str) -> Option<StoredAccount> {
        let g = self.data.lock().unwrap();
        g.tools
            .get(tool.as_str())
            .and_then(|t| t.accounts.iter().find(|a| a.id == account_id).cloned())
    }

    pub fn reveal_api_config(&self, tool: ToolKind, account_id: &str) -> Result<ApiKeyConfig> {
        let acc = self
            .get_stored(tool, account_id)
            .ok_or_else(|| anyhow!("账号 {account_id} 不存在"))?;
        Ok(acc.api_config.unwrap_or_default())
    }

    pub fn all_accounts_for_tool(&self, tool: ToolKind) -> Vec<StoredAccount> {
        let g = self.data.lock().unwrap();
        g.tools
            .get(tool.as_str())
            .map(|t| t.accounts.clone())
            .unwrap_or_default()
    }

    pub fn set_runtime_state(
        &self,
        tool: ToolKind,
        account_id: &str,
        state: AccountState,
        detail: Option<String>,
    ) {
        self.runtime
            .lock()
            .unwrap()
            .insert((tool, account_id.to_string()), (state, detail));
    }

    pub fn clear_limited_state(&self, tool: ToolKind, account_id: &str) -> bool {
        let mut g = self.runtime.lock().unwrap();
        if matches!(g.get(&(tool, account_id.to_string())), Some((AccountState::Limited, _))) {
            g.insert((tool, account_id.to_string()), (AccountState::Ready, None));
            true
        } else {
            false
        }
    }

    fn runtime_state(&self, tool: ToolKind, account_id: &str) -> (AccountState, Option<String>) {
        self.runtime
            .lock()
            .unwrap()
            .get(&(tool, account_id.to_string()))
            .cloned()
            .unwrap_or((AccountState::Ready, None))
    }

    fn to_view(
        &self,
        acc: &StoredAccount,
        active_id: Option<&str>,
        in_use_sessions: usize,
        quota: Option<serde_json::Value>,
    ) -> AccountProfileView {
        let (mut state, mut state_detail) = self.runtime_state(acc.tool, &acc.id);
        if acc.tool == ToolKind::Cursor
            && acc.auth_kind == AccountAuthKind::Oauth
            && state == AccountState::Ready
            && !self.has_clean_cursor_oauth_snapshot(&acc.id)
        {
            state = AccountState::NeedsLogin;
            state_detail = Some(
                "已保存凭据实际为 CURSOR_API_KEY 而非网页授权登录，请点击「重新登录」完成授权".into(),
            );
        }
        AccountProfileView {
            id: acc.id.clone(),
            tool: acc.tool,
            label: acc.label.clone(),
            auth_kind: acc.auth_kind,
            isolation: Self::effective_isolation(acc.tool, acc.auth_kind),
            active: active_id == Some(acc.id.as_str()),
            in_use_sessions,
            allow_auto_failover: acc.allow_auto_failover,
            priority: acc.priority,
            identity: acc.identity.clone(),
            api_config: acc.api_config.as_ref().map(redact_api_config),
            state,
            state_detail,
            quota,
            updated_at: acc.updated_at,
            actual: (acc.tool == ToolKind::Codex)
                .then(|| {
                    let key = acc.api_config.as_ref().map(|c| c.api_key.as_str()).filter(|k| !k.is_empty());
                    self.codex_actual(&self.profile_home(ToolKind::Codex, &acc.id), key)
                })
                .flatten(),
        }
    }

    /// OAuth accounts saved without an email (e.g. Antigravity on Windows before its keyring entry was recognised):
    /// read it from the saved snapshot, which also renames a placeholder label. Tried once per account per process
    fn backfill_identity_from_snapshots(&self, tool: ToolKind) {
        static TRIED: Mutex<Option<HashSet<(ToolKind, String)>>> = Mutex::new(None);
        let ids: Vec<String> = {
            let g = self.data.lock().unwrap();
            let Some(td) = g.tools.get(tool.as_str()) else {
                return;
            };
            td.accounts
                .iter()
                .filter(|a| a.auth_kind == AccountAuthKind::Oauth && a.identity.email.is_none())
                .map(|a| a.id.clone())
                .collect()
        };
        for id in ids {
            if !TRIED.lock().unwrap().get_or_insert_with(HashSet::new).insert((tool, id.clone())) {
                continue;
            }
            let Some(snap) = self.load_snapshot(tool, &id) else {
                continue;
            };
            let ident = self.parse_host_identity(tool, &snap);
            self.update_identity_if_missing(tool, &id, ident.email.as_deref(), ident.plan.as_deref());
        }
    }

    pub fn list(
        &self,
        tool_filter: Option<ToolKind>,
        in_use: &HashMap<(ToolKind, String), usize>,
        quota_fn: &dyn Fn(ToolKind, &str) -> Option<serde_json::Value>,
        check_native: bool,
    ) -> Vec<ToolAccountsSummary> {
        let tools: Vec<ToolKind> = match tool_filter {
            Some(t) if t.supports_accounts() => vec![t],
            Some(_) => vec![],
            None => ToolKind::ALL
                .into_iter()
                .filter(|t| t.supports_accounts())
                .collect(),
        };
        for t in &tools {
            self.backfill_identity_from_snapshots(*t);
        }

        let snapshot: Vec<(ToolKind, ToolAccountsData)> = {
            let g = self.data.lock().unwrap();
            tools
                .into_iter()
                .map(|t| (t, g.tools.get(t.as_str()).cloned().unwrap_or_default()))
                .collect()
        };

        snapshot
            .into_iter()
            .map(|(tool, td)| {
                let cap = Self::capability(tool);
                let active_id = td.active_account_id.clone();
                let accounts: Vec<AccountProfileView> = td
                    .accounts
                    .iter()
                    .map(|a| {
                        let cnt = *in_use.get(&(tool, a.id.clone())).unwrap_or(&0);
                        let q = quota_fn(tool, &a.id);
                        self.to_view(a, active_id.as_deref(), cnt, q)
                    })
                    .collect();
                let native_status = if check_native {
                    Some(self.probe_native_status(tool, &td.accounts, active_id.as_deref()))
                } else {
                    None
                };
                ToolAccountsSummary {
                    tool,
                    oauth_isolation: cap.oauth_isolation,
                    supports_oauth: cap.supports_oauth,
                    supports_api_key: cap.supports_api_key,
                    supports_device_auth: cap.supports_device_auth,
                    supports_interactive_login: cap.supports_interactive_login,
                    auto_failover: td.auto_failover,
                    active_account_id: active_id,
                    native_status,
                    accounts,
                }
            })
            .collect()
    }

    /// Resolve launch env, config dir, auth method and fingerprint for `requested_account_id`
    /// (falling back to the tool's `active_account_id` when `None`).
    pub fn resolve(
        &self,
        tool: ToolKind,
        requested_account_id: Option<&str>,
    ) -> Option<ResolvedAccount> {
        if !tool.supports_accounts() {
            return None;
        }
        let acc = {
            let g = self.data.lock().unwrap();
            let td = g.tools.get(tool.as_str())?;
            if let Some(req) = requested_account_id {
                td.accounts.iter().find(|a| a.id == req)?.clone()
            } else {
                let active = td.active_account_id.as_deref()?;
                td.accounts.iter().find(|a| a.id == active)?.clone()
            }
        };
        Some(self.build_resolved(&acc))
    }

    pub fn resolve_exact(&self, tool: ToolKind, account_id: &str) -> Option<ResolvedAccount> {
        self.get_stored(tool, account_id)
            .map(|a| self.build_resolved(&a))
    }

    fn build_resolved(&self, acc: &StoredAccount) -> ResolvedAccount {
        let mut env = BTreeMap::new();
        let mut env_unset = Vec::new();
        let mut data_dir = None;
        let mut auth_method: Option<String> = None;
        let isolation = Self::effective_isolation(acc.tool, acc.auth_kind);

        match (acc.tool, acc.auth_kind) {
            (ToolKind::Codex, AccountAuthKind::Oauth) => {
                let home = self.profile_home(ToolKind::Codex, &acc.id);
                env.insert("CODEX_HOME".into(), home.display().to_string());
                env_unset.extend(["OPENAI_API_KEY", "OPENAI_BASE_URL"].into_iter().map(String::from));
                // A custom provider's key set on the host after the daemon started (Windows registry)
                if let Some(k) = self.codex_actual(&home, None).and_then(|a| a.env_key) {
                    if std::env::var_os(&k).map_or(true, |v| v.is_empty()) {
                        if let Some(v) = self.host_env(&k) {
                            env.insert(k, v);
                        }
                    }
                }
                data_dir = Some(home);
            }
            (ToolKind::Codex, AccountAuthKind::ApiKey) => {
                let home = self.profile_home(ToolKind::Codex, &acc.id);
                env.insert("CODEX_HOME".into(), home.display().to_string());
                if let Some(cfg) = &acc.api_config {
                    let _ = ensure_codex_profile_files(&home, Some(cfg));
                    // The profile's config.toml selects a custom provider: its key goes in the variable it reads,
                    // and the base URL is the provider's own (OPENAI_BASE_URL only applies to the built-in provider)
                    let provider_key = self.codex_actual(&home, None).and_then(|a| a.env_key);
                    if !cfg.api_key.is_empty() {
                        env.insert("OPENAI_API_KEY".into(), cfg.api_key.clone());
                        if let Some(k) = &provider_key {
                            env.insert(k.clone(), cfg.api_key.clone());
                        }
                    }
                    if !cfg.base_url.is_empty() && provider_key.is_none() {
                        env.insert("OPENAI_BASE_URL".into(), cfg.base_url.clone());
                    } else {
                        env_unset.push("OPENAI_BASE_URL".into());
                    }
                    for (k, v) in &cfg.extra_env {
                        if !k.is_empty() {
                            env.insert(k.clone(), v.clone());
                        }
                    }
                }
                data_dir = Some(home);
            }
            (ToolKind::Claude, AccountAuthKind::Oauth) => {
                let dir = self.profile_home(ToolKind::Claude, &acc.id);
                env.insert("CLAUDE_CONFIG_DIR".into(), dir.display().to_string());
                env_unset.extend(
                    [
                        "ANTHROPIC_API_KEY",
                        "ANTHROPIC_AUTH_TOKEN",
                        "ANTHROPIC_BASE_URL",
                        "ANTHROPIC_MODEL",
                    ]
                    .into_iter()
                    .map(String::from),
                );
                data_dir = Some(dir);
            }
            (ToolKind::Claude, AccountAuthKind::ApiKey) => {
                let dir = self.profile_home(ToolKind::Claude, &acc.id);
                let _ = std::fs::create_dir_all(&dir);
                env.insert("CLAUDE_CONFIG_DIR".into(), dir.display().to_string());
                if let Some(cfg) = &acc.api_config {
                    if !cfg.api_key.is_empty() {
                        env.insert("ANTHROPIC_API_KEY".into(), cfg.api_key.clone());
                    } else {
                        env_unset.push("ANTHROPIC_API_KEY".into());
                    }
                    if !cfg.auth_token.is_empty() {
                        env.insert("ANTHROPIC_AUTH_TOKEN".into(), cfg.auth_token.clone());
                    } else {
                        env_unset.push("ANTHROPIC_AUTH_TOKEN".into());
                    }
                    if !cfg.base_url.is_empty() {
                        env.insert("ANTHROPIC_BASE_URL".into(), cfg.base_url.clone());
                    } else {
                        env_unset.push("ANTHROPIC_BASE_URL".into());
                    }
                    if !cfg.default_model.is_empty() {
                        env.insert("ANTHROPIC_MODEL".into(), cfg.default_model.clone());
                    }
                    for (k, v) in &cfg.extra_env {
                        if !k.is_empty() {
                            env.insert(k.clone(), v.clone());
                        }
                    }
                }
                data_dir = Some(dir);
            }
            (ToolKind::Antigravity, AccountAuthKind::Oauth) => {
                env_unset.extend(["GEMINI_API_KEY", "GOOGLE_API_KEY"].into_iter().map(String::from));
            }
            (ToolKind::Antigravity, AccountAuthKind::ApiKey) => {
                let dir = self.profile_home(ToolKind::Antigravity, &acc.id);
                let _ = std::fs::create_dir_all(&dir);
                if let Some(cfg) = &acc.api_config {
                    if !cfg.api_key.is_empty() {
                        env.insert("GEMINI_API_KEY".into(), cfg.api_key.clone());
                    }
                    if !cfg.base_url.is_empty() {
                        env.insert("GOOGLE_GEMINI_BASE_URL".into(), cfg.base_url.clone());
                    }
                    if !cfg.default_model.is_empty() {
                        env.insert("GEMINI_MODEL".into(), cfg.default_model.clone());
                    }
                    for (k, v) in &cfg.extra_env {
                        if !k.is_empty() {
                            env.insert(k.clone(), v.clone());
                        }
                    }
                }
                auth_method = Some("gemini-api-key".into());
                data_dir = Some(dir);
            }
            (ToolKind::Cursor, AccountAuthKind::Oauth) => {
                env_unset.extend(["CURSOR_API_KEY", "CURSOR_AUTH_TOKEN"].into_iter().map(String::from));
                // macOS: keep the login in files (~/.cursor) rather than the Keychain, so account swaps never write the Keychain
                if cursor_file_store() {
                    env.insert("AGENT_CLI_CREDENTIAL_STORE".into(), "file".into());
                } else {
                    env_unset.push("AGENT_CLI_CREDENTIAL_STORE".into());
                }
            }
            (ToolKind::Cursor, AccountAuthKind::ApiKey) => {
                // Keep exchanged API-key tokens strictly in process memory so `cursor-agent`
                // neither reads cached host OAuth tokens nor overwrites host `auth.json` / Keychain.
                env.insert("AGENT_CLI_CREDENTIAL_STORE".into(), "memory".into());
                if let Some(cfg) = &acc.api_config {
                    if !cfg.api_key.is_empty() {
                        env.insert("CURSOR_API_KEY".into(), cfg.api_key.clone());
                    } else {
                        env_unset.push("CURSOR_API_KEY".into());
                    }
                    if !cfg.auth_token.is_empty() {
                        env.insert("CURSOR_AUTH_TOKEN".into(), cfg.auth_token.clone());
                    } else {
                        env_unset.push("CURSOR_AUTH_TOKEN".into());
                    }
                    if !cfg.base_url.is_empty() {
                        env.insert("CURSOR_API_ENDPOINT".into(), cfg.base_url.clone());
                    } else {
                        env_unset.push("CURSOR_API_ENDPOINT".into());
                    }
                    for (k, v) in &cfg.extra_env {
                        if !k.is_empty() {
                            env.insert(k.clone(), v.clone());
                        }
                    }
                }
            }
            (ToolKind::Qoder, AccountAuthKind::Oauth) => {
                let dir = self.profile_home(ToolKind::Qoder, &acc.id);
                env.insert("QODER_CONFIG_DIR".into(), dir.display().to_string());
                env_unset.push("QODER_PERSONAL_ACCESS_TOKEN".into());
                data_dir = Some(dir);
            }
            (ToolKind::Qoder, AccountAuthKind::ApiKey) => {
                // qodercli only reads a Personal Access Token from this variable; it has no base URL / auth token
                // settings. A `/login` session takes precedence over it, so the profile gets its own empty config dir.
                let dir = self.profile_home(ToolKind::Qoder, &acc.id);
                let _ = std::fs::create_dir_all(&dir);
                env.insert("QODER_CONFIG_DIR".into(), dir.display().to_string());
                data_dir = Some(dir);
                if let Some(cfg) = &acc.api_config {
                    if !cfg.api_key.is_empty() {
                        env.insert("QODER_PERSONAL_ACCESS_TOKEN".into(), cfg.api_key.clone());
                    }
                    for (k, v) in &cfg.extra_env {
                        if !k.is_empty() {
                            env.insert(k.clone(), v.clone());
                        }
                    }
                }
            }
            (ToolKind::Codebuddy, AccountAuthKind::ApiKey) => {
                if let Some(cfg) = &acc.api_config {
                    for (var, val) in [
                        ("CODEBUDDY_API_KEY", &cfg.api_key),
                        ("CODEBUDDY_AUTH_TOKEN", &cfg.auth_token),
                        ("CODEBUDDY_BASE_URL", &cfg.base_url),
                        ("CODEBUDDY_MODEL", &cfg.default_model),
                    ] {
                        if !val.is_empty() {
                            env.insert(var.into(), val.clone());
                        }
                    }
                    for (k, v) in &cfg.extra_env {
                        if !k.is_empty() {
                            env.insert(k.clone(), v.clone());
                        }
                    }
                }
            }
            (ToolKind::Kiro, AccountAuthKind::ApiKey) => {
                // kiro-cli's headless API key auth; it has no base URL / auth token / model variables
                if let Some(cfg) = &acc.api_config {
                    if !cfg.api_key.is_empty() {
                        env.insert("KIRO_API_KEY".into(), cfg.api_key.clone());
                    }
                    for (k, v) in &cfg.extra_env {
                        if !k.is_empty() {
                            env.insert(k.clone(), v.clone());
                        }
                    }
                }
            }
            (_, AccountAuthKind::ApiKey) => {}
            (_, AccountAuthKind::Oauth) => {}
        }

        env_unset.retain(|k| !env.contains_key(k));

        let mut hasher = Sha256::new();
        hasher.update(acc.id.as_bytes());
        hasher.update(b":");
        hasher.update(if acc.auth_kind == AccountAuthKind::ApiKey {
            b"api".as_slice()
        } else {
            b"oauth".as_slice()
        });
        for (k, v) in &env {
            hasher.update(k.as_bytes());
            hasher.update(b"=");
            hasher.update(v.as_bytes());
            hasher.update(b"\n");
        }
        for u in &env_unset {
            hasher.update(b"!");
            hasher.update(u.as_bytes());
        }
        if let Some(m) = &auth_method {
            hasher.update(m.as_bytes());
        }
        let fingerprint = format!("{:x}", hasher.finalize())[..16].to_string();

        ResolvedAccount {
            id: acc.id.clone(),
            label: acc.label.clone(),
            auth_kind: acc.auth_kind,
            isolation,
            identity: acc.identity.clone(),
            env,
            env_unset,
            data_dir,
            auth_method,
            fingerprint,
        }
    }

    /// Capture the host tool's current native CLI login as a new AccountProfile.
    #[cfg(test)]
    pub fn capture(&self, tool: ToolKind, label: Option<&str>) -> Result<AccountProfileView> {
        self.capture_with_source(tool, label, None)
    }

    pub fn capture_with_source(
        &self,
        tool: ToolKind,
        label: Option<&str>,
        source: Option<&str>,
    ) -> Result<AccountProfileView> {
        let cursor_env_key = if tool == ToolKind::Cursor {
            crate::tools::host_env_var("CURSOR_API_KEY")
        } else {
            None
        };
        self.capture_with_cursor_env(tool, label, source, cursor_env_key.as_deref())
    }

    fn capture_with_cursor_env(
        &self,
        tool: ToolKind,
        label: Option<&str>,
        source: Option<&str>,
        cursor_env_key: Option<&str>,
    ) -> Result<AccountProfileView> {
        if !tool.supports_accounts() {
            bail!("该工具不支持账号管理");
        }
        let _guard = self.host_lock.lock().unwrap();
        let id = Self::new_account_id();

        let (auth_kind, identity, api_config, snap_to_save, set_active) = match tool {
            ToolKind::Codex => {
                let src = self.native_codex_home();
                let cfg_raw = std::fs::read_to_string(src.join("config.toml")).ok();
                let provider = cfg_raw.as_deref().and_then(codex_provider);
                // Default to what Codex really uses: a custom provider with its own key wins over auth.json
                let want_key = match source {
                    Some("apiKey") => true,
                    Some("oauth") => false,
                    _ => provider.as_ref().is_some_and(|p| p.env_key.is_some()),
                };
                if want_key {
                    let Some((p, env_key)) = provider.and_then(|p| p.env_key.clone().map(|k| (p, k))) else {
                        bail!("~/.codex/config.toml 没有选择带 env_key 的自定义 provider，无法按 API Key 抓取");
                    };
                    let Some(key) = self.host_env(&env_key) else {
                        bail!("环境变量 {env_key} 未设置，无法抓取 provider {} 的 API Key", p.id);
                    };
                    let dst = self.profile_home(ToolKind::Codex, &id);
                    std::fs::create_dir_all(&dst)?;
                    if let Some(raw) = &cfg_raw {
                        std::fs::write(dst.join("config.toml"), raw)?;
                    }
                    let cfg = ApiKeyConfig {
                        api_key: key,
                        base_url: p.base_url.clone().unwrap_or_default(),
                        provider_name: p.id.clone(),
                        ..Default::default()
                    };
                    ensure_codex_profile_files(&dst, Some(&cfg))?;
                    let ident = AccountIdentity { plan: Some(p.id.clone()), workspace: p.base_url.clone(), ..Default::default() };
                    (AccountAuthKind::ApiKey, ident, Some(cfg), None, false)
                } else {
                    let mut auth_bytes = std::fs::read(src.join("auth.json")).ok();
                    if auth_bytes.is_none() {
                        if let Ok(kr) = keyring::snapshot_tool(ToolKind::Codex) {
                            if let Some(item) = kr.into_iter().find(|e| !e.secret.is_empty()) {
                                auth_bytes = Some(item.secret.into_bytes());
                            }
                        }
                    }
                    let Some(bytes) = auth_bytes else {
                        bail!("当前系统 Codex 未登录或未找到凭据，请先在系统登录或使用应用内登录");
                    };
                    let dst = self.profile_home(ToolKind::Codex, &id);
                    std::fs::create_dir_all(&dst)?;
                    let auth_path = dst.join("auth.json");
                    std::fs::write(&auth_path, &bytes)?;
                    crate::util::restrict_file(&auth_path);
                    if let Some(raw) = &cfg_raw {
                        // Capturing the login means using it: a custom provider in the host config would bypass it
                        std::fs::write(dst.join("config.toml"), strip_codex_provider(raw))?;
                    }
                    ensure_codex_profile_files(&dst, None)?;
                    let (ak, ident, key_opt) = parse_codex_auth_bytes(&bytes);
                    let api_cfg = key_opt.map(|k| ApiKeyConfig {
                        api_key: k,
                        ..Default::default()
                    });
                    (ak, ident, api_cfg, None, false)
                }
            }
            ToolKind::Claude => {
                let src = self.native_claude_dir();
                let mut cred_bytes = std::fs::read(src.join(".credentials.json")).ok();
                if cred_bytes.is_none() {
                    if let Ok(kr) = keyring::snapshot_tool(ToolKind::Claude) {
                        if let Some(item) = kr.into_iter().find(|e| !e.secret.is_empty()) {
                            cred_bytes = Some(item.secret.into_bytes());
                        }
                    }
                }
                let Some(bytes) = cred_bytes else {
                    bail!("当前系统 Claude 未登录或未找到凭据");
                };
                let dst = self.profile_home(ToolKind::Claude, &id);
                std::fs::create_dir_all(&dst)?;
                let cred_path = dst.join(".credentials.json");
                std::fs::write(&cred_path, &bytes)?;
                crate::util::restrict_file(&cred_path);

                let dot_claude_src = self.home_dir.join(".claude.json");
                let dot_claude_meta = extract_claude_safe_meta(&dot_claude_src);
                if let Some(meta) = &dot_claude_meta {
                    let meta_path = dst.join(".claude.json");
                    std::fs::write(&meta_path, serde_json::to_string_pretty(meta)?)?;
                    crate::util::restrict_file(&meta_path);
                }
                let ident = parse_claude_identity(&bytes, dot_claude_meta.as_ref());
                (AccountAuthKind::Oauth, ident, None, None, false)
            }
            ToolKind::Qoder => {
                let src = self.home_dir.join(".qoder").join(".auth");
                if !src.join("user").is_file() {
                    bail!("当前系统 Qoder 未登录（没有找到 ~/.qoder/.auth/user），请先运行 qodercli login 或使用应用内登录");
                }
                copy_qoder_login(&src, &self.profile_home(ToolKind::Qoder, &id).join(".auth"))?;
                (AccountAuthKind::Oauth, AccountIdentity::default(), None, None, false)
            }
            ToolKind::Cursor => {
                let snap = self.take_host_snapshot_unlocked(ToolKind::Cursor).unwrap_or_default();
                let file_api_key = cursor_snapshot_api_key(&snap);
                let is_polluted = cursor_snapshot_is_api_key(&snap);
                let env_key = cursor_env_key
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(String::from);
                match source {
                    Some("oauth") => {
                        if snap.is_empty() || is_polluted {
                            bail!("当前系统未检测到网页授权登录凭据（本地保存的是 CURSOR_API_KEY 凭据），请点击「OAuth / CLI 登录」完成授权登录");
                        }
                        let ident = self.parse_host_identity(ToolKind::Cursor, &snap);
                        (AccountAuthKind::Oauth, ident, None, Some(snap), true)
                    }
                    Some("apiKey") => {
                        let Some(key) = env_key.or_else(|| file_api_key.clone()) else {
                            bail!("当前系统未检测到 CURSOR_API_KEY 环境变量或 API Key 凭据");
                        };
                        let ident = if file_api_key.as_deref() == Some(key.as_str()) {
                            self.parse_host_identity(ToolKind::Cursor, &snap)
                        } else {
                            AccountIdentity::default()
                        };
                        let api_cfg = ApiKeyConfig {
                            api_key: key,
                            ..Default::default()
                        };
                        (AccountAuthKind::ApiKey, ident, Some(api_cfg), None, false)
                    }
                    _ => {
                        if let Some(key) = env_key.or_else(|| file_api_key.clone()) {
                            let ident = if file_api_key.as_deref() == Some(key.as_str()) {
                                self.parse_host_identity(ToolKind::Cursor, &snap)
                            } else {
                                AccountIdentity::default()
                            };
                            let api_cfg = ApiKeyConfig {
                                api_key: key,
                                ..Default::default()
                            };
                            (AccountAuthKind::ApiKey, ident, Some(api_cfg), None, false)
                        } else if snap.is_empty() {
                            bail!("当前系统 cursor 未检测到已保存的 OAuth 登录凭据");
                        } else if is_polluted {
                            bail!("当前系统未检测到网页授权登录凭据（本地保存的是 CURSOR_API_KEY 凭据），请点击「OAuth / CLI 登录」完成授权登录");
                        } else {
                            let ident = self.parse_host_identity(ToolKind::Cursor, &snap);
                            (AccountAuthKind::Oauth, ident, None, Some(snap), true)
                        }
                    }
                }
            }
            _ => {
                let snap = self.take_host_snapshot_unlocked(tool)?;
                if snap.is_empty() {
                    bail!("当前系统 {} 未检测到已保存的 OAuth 登录凭据", tool.as_str());
                }
                self.learn_kiro_email(tool, &snap, None);
                let ident = self.parse_host_identity(tool, &snap);
                (AccountAuthKind::Oauth, ident, None, Some(snap), true)
            }
        };

        if let Some(snap) = snap_to_save {
            self.save_snapshot(tool, &id, &snap)?;
        }

        let resolved_label = pick_label(label, &identity, tool, auth_kind, &self.data.lock().unwrap(), tool);
        let now = crate::util::now_ms();
        let stored = StoredAccount {
            id: id.clone(),
            tool,
            label: resolved_label,
            auth_kind,
            allow_auto_failover: auth_kind == AccountAuthKind::Oauth,
            priority: 0,
            identity,
            api_config,
            updated_at: now,
        };

        let view = {
            let mut g = self.data.lock().unwrap();
            let active_id = {
                let td = g.tools.entry(tool.as_str().to_string()).or_default();
                if set_active || td.active_account_id.is_none() {
                    td.active_account_id = Some(id.clone());
                }
                td.accounts.push(stored.clone());
                td.active_account_id.clone()
            };
            self.save_file(&g)?;
            self.to_view(&stored, active_id.as_deref(), 0, None)
        };
        Ok(view)
    }

    /// Save or update an account created by in-app OAuth / device-auth login flow.
    pub fn save_oauth_from_dir(
        &self,
        tool: ToolKind,
        target_account_id: Option<&str>,
        label: Option<&str>,
        profile_dir: Option<&Path>,
        host_snapshot: Option<SnapshotBundle>,
        activate_after: bool,
    ) -> Result<AccountProfileView> {
        let mut ident = AccountIdentity::default();
        if let Some(dir) = profile_dir {
            match tool {
                ToolKind::Codex => {
                    if let Ok(bytes) = std::fs::read(dir.join("auth.json")) {
                        ident = parse_codex_auth_bytes(&bytes).1;
                    }
                    let _ = ensure_codex_profile_files(dir, None);
                }
                ToolKind::Claude => {
                    let cred = std::fs::read(dir.join(".credentials.json")).unwrap_or_default();
                    let meta = std::fs::read_to_string(dir.join(".claude.json"))
                        .ok()
                        .and_then(|s| serde_json::from_str(&s).ok());
                    ident = parse_claude_identity(&cred, meta.as_ref());
                }
                _ => {}
            }
        }
        if let Some(snap) = &host_snapshot {
            ident = self.parse_host_identity(tool, snap);
        }

        let now = crate::util::now_ms();
        let mut g = self.data.lock().unwrap();
        let fallback_lbl = pick_label(label, &ident, tool, AccountAuthKind::Oauth, &*g, tool);

        let id = {
            let td = g.tools.entry(tool.as_str().to_string()).or_default();
            if let Some(tid) = target_account_id {
                if let Some(existing) = td.accounts.iter_mut().find(|a| a.id == tid) {
                    if let Some(l) = label.map(str::trim).filter(|s| !s.is_empty()) {
                        existing.label = l.to_string();
                    } else if existing.label.is_empty() || existing.label.starts_with("Pending ") {
                        existing.label = ident
                            .email
                            .clone()
                            .unwrap_or_else(|| format!("{} {}", tool.as_str(), tid));
                    }
                    existing.auth_kind = AccountAuthKind::Oauth;
                    existing.identity = ident.clone();
                    existing.api_config = None;
                    existing.updated_at = now;
                    tid.to_string()
                } else {
                    td.accounts.push(StoredAccount {
                        id: tid.to_string(),
                        tool,
                        label: fallback_lbl,
                        auth_kind: AccountAuthKind::Oauth,
                        allow_auto_failover: true,
                        priority: 0,
                        identity: ident.clone(),
                        api_config: None,
                        updated_at: now,
                    });
                    tid.to_string()
                }
            } else {
                let new_id = Self::new_account_id();
                td.accounts.push(StoredAccount {
                    id: new_id.clone(),
                    tool,
                    label: fallback_lbl,
                    auth_kind: AccountAuthKind::Oauth,
                    allow_auto_failover: true,
                    priority: 0,
                    identity: ident.clone(),
                    api_config: None,
                    updated_at: now,
                });
                new_id
            }
        };

        if let Some(snap) = host_snapshot {
            drop(g);
            self.save_snapshot(tool, &id, &snap)?;
            g = self.data.lock().unwrap();
        }
        let (stored, active_id) = {
            let td = g.tools.entry(tool.as_str().to_string()).or_default();
            if activate_after || td.active_account_id.is_none() {
                td.active_account_id = Some(id.clone());
            }
            (
                td.accounts.iter().find(|a| a.id == id).unwrap().clone(),
                td.active_account_id.clone(),
            )
        };
        self.save_file(&g)?;
        drop(g);
        self.set_runtime_state(tool, &id, AccountState::Ready, None);
        Ok(self.to_view(&stored, active_id.as_deref(), 0, None))
    }

    /// Create or update an API Key profile (or update metadata/failover policy on an existing account).
    pub fn save(&self, req: SaveAccountRequest) -> Result<AccountProfileView> {
        if !req.tool.supports_accounts() {
            bail!("该工具不支持账号管理");
        }
        let now = crate::util::now_ms();
        let mut g = self.data.lock().unwrap();
        let td = g.tools.entry(req.tool.as_str().to_string()).or_default();

        let id = if let Some(existing_id) = req.id.clone().filter(|s| !s.trim().is_empty()) {
            let existing = td
                .accounts
                .iter_mut()
                .find(|a| a.id == existing_id)
                .ok_or_else(|| anyhow!("账号 {existing_id} 不存在"))?;
            if let Some(lbl) = req.label.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                existing.label = lbl.to_string();
            }
            if let Some(allow) = req.allow_auto_failover {
                existing.allow_auto_failover = allow;
            }
            if let Some(prio) = req.priority {
                existing.priority = prio;
            }
            if let Some(incoming_cfg) = req.api_config {
                let merged = merge_api_config(&incoming_cfg, existing.api_config.as_ref());
                existing.api_config = Some(merged);
                if existing.auth_kind == AccountAuthKind::ApiKey {
                    existing.identity.provider_or_url_update(&existing.api_config);
                }
            }
            if let Some(vi) = &req.verified_identity {
                if vi.email.is_some() {
                    existing.identity.email = vi.email.clone();
                }
                if vi.account_key.is_some() {
                    existing.identity.account_key = vi.account_key.clone();
                }
                if vi.plan.is_some() {
                    existing.identity.plan = vi.plan.clone();
                }
            }
            existing.updated_at = now;
            if req.tool == ToolKind::Codex && existing.auth_kind == AccountAuthKind::ApiKey {
                let dir = self.base_dir.join("accounts").join(req.tool.as_str()).join(&existing_id).join("home");
                let _ = ensure_codex_profile_files(&dir, existing.api_config.as_ref());
            }
            existing_id
        } else {
            let auth_kind = req.auth_kind.unwrap_or(AccountAuthKind::ApiKey);
            if auth_kind != AccountAuthKind::ApiKey {
                bail!("新增 OAuth 账号请使用抓取当前登录或应用内登录");
            }
            let api_cfg = req
                .api_config
                .ok_or_else(|| anyhow!("缺少 API Key 配置"))?;
            let merged = merge_api_config(&api_cfg, None);
            if merged.api_key.is_empty() && merged.auth_token.is_empty() && merged.extra_env.is_empty() {
                bail!("请填写 API Key 或 Auth Token");
            }
            let new_id = Self::new_account_id();
            let mut ident = req.verified_identity.unwrap_or_default();
            if ident.plan.is_none() && !merged.provider_name.is_empty() {
                ident.plan = Some(merged.provider_name.clone());
            }
            if ident.workspace.is_none() && !merged.base_url.is_empty() {
                ident.workspace = Some(merged.base_url.clone());
            }
            let existing_labels: Vec<String> = td.accounts.iter().map(|a| a.label.clone()).collect();
            let lbl = pick_api_key_label(
                req.label.as_deref(),
                &ident,
                req.tool,
                &merged,
                &existing_labels,
            );
            if req.tool == ToolKind::Codex {
                let dir = self
                    .base_dir
                    .join("accounts")
                    .join(req.tool.as_str())
                    .join(&new_id)
                    .join("home");
                ensure_codex_profile_files(&dir, Some(&merged))?;
            }
            td.accounts.push(StoredAccount {
                id: new_id.clone(),
                tool: req.tool,
                label: lbl,
                auth_kind: AccountAuthKind::ApiKey,
                allow_auto_failover: req.allow_auto_failover.unwrap_or(false),
                priority: req.priority.unwrap_or(0),
                identity: ident,
                api_config: Some(merged),
                updated_at: now,
            });
            new_id
        };

        if req.activate || td.active_account_id.is_none() {
            td.active_account_id = Some(id.clone());
        }
        let stored = td.accounts.iter().find(|a| a.id == id).unwrap().clone();
        let active_id = td.active_account_id.clone();
        let _ = td;
        self.save_file(&g)?;
        Ok(self.to_view(&stored, active_id.as_deref(), 0, None))
    }

    pub fn delete(&self, tool: ToolKind, account_id: &str) -> Result<()> {
        {
            let mut g = self.data.lock().unwrap();
            let Some(td) = g.tools.get_mut(tool.as_str()) else {
                bail!("账号 {account_id} 不存在");
            };
            let before = td.accounts.len();
            td.accounts.retain(|a| a.id != account_id);
            if td.accounts.len() == before {
                bail!("账号 {account_id} 不存在");
            }
            if td.active_account_id.as_deref() == Some(account_id) {
                td.active_account_id = td.accounts.first().map(|a| a.id.clone());
            }
            self.save_file(&g)?;
        }
        self.runtime.lock().unwrap().remove(&(tool, account_id.to_string()));
        let dir = self.account_dir(tool, account_id);
        if dir.exists() {
            let _ = std::fs::remove_dir_all(&dir);
        }
        Ok(())
    }

    /// Put the accounts in `ids` order (unknown ids ignored, missing ones keep their place after them) and reset
    /// priorities, so automatic failover follows the list order
    pub fn reorder(&self, tool: ToolKind, ids: &[String]) -> Result<()> {
        let mut g = self.data.lock().unwrap();
        let td = g.tools.get_mut(tool.as_str()).ok_or_else(|| anyhow!("没有 {} 账号", tool.as_str()))?;
        let rank = |id: &str| ids.iter().position(|x| x == id).unwrap_or(usize::MAX);
        td.accounts.sort_by_key(|a| rank(&a.id));
        for a in td.accounts.iter_mut() {
            a.priority = 0;
        }
        self.save_file(&g)?;
        Ok(())
    }

    pub fn set_policy(&self, tool: ToolKind, auto_failover: bool) -> Result<()> {
        if !tool.supports_accounts() {
            bail!("该工具不支持账号管理");
        }
        let mut g = self.data.lock().unwrap();
        let td = g.tools.entry(tool.as_str().to_string()).or_default();
        td.auto_failover = auto_failover;
        self.save_file(&g)?;
        Ok(())
    }

    pub fn activate(&self, tool: ToolKind, account_id: &str) -> Result<ActivateOutcome> {
        let _guard = self.host_lock.lock().unwrap();
        let (target, prev_active_id) = {
            let g = self.data.lock().unwrap();
            let td = g
                .tools
                .get(tool.as_str())
                .ok_or_else(|| anyhow!("账号 {account_id} 不存在"))?;
            let acc = td
                .accounts
                .iter()
                .find(|a| a.id == account_id)
                .cloned()
                .ok_or_else(|| anyhow!("账号 {account_id} 不存在"))?;
            (acc, td.active_account_id.clone())
        };

        let isolation = Self::effective_isolation(tool, target.auth_kind);
        let mut host_switched = false;

        if isolation == AccountIsolation::HostActive {
            let target_snap = self
                .load_snapshot(tool, account_id)
                .ok_or_else(|| anyhow!("账号 {} 缺少系统凭据快照，请重新登录或重新抓取", target.label))?;
            if tool == ToolKind::Cursor && cursor_snapshot_is_api_key(&target_snap) {
                bail!("当前 Cursor 授权账号保存的凭据实际为 API Key 而非网页授权登录，请在账号管理中点击「重新登录」完成授权");
            }
            let backup = self.take_host_snapshot_unlocked(tool)?;

            // Save refreshed tokens from current host active account back into its snapshot before swapping out.
            if let Some(prev_id) = prev_active_id.as_deref() {
                if prev_id != account_id && !backup.is_empty() {
                    if let Some(prev_acc) = self.get_stored(tool, prev_id) {
                        if Self::effective_isolation(tool, prev_acc.auth_kind) == AccountIsolation::HostActive {
                            let cursor_polluted = tool == ToolKind::Cursor && cursor_snapshot_is_api_key(&backup);
                            let cur_ident = self.parse_host_identity(tool, &backup);
                            if !cursor_polluted && cur_ident.matches_or_unknown(&prev_acc.identity) {
                                let _ = self.save_snapshot(tool, prev_id, &backup);
                            }
                        }
                    }
                }
            }

            if let Err(err) = self.restore_host_snapshot_unlocked(tool, &target_snap) {
                let _ = self.restore_host_snapshot_unlocked(tool, &backup);
                return Err(anyhow!("切换系统登录凭据失败，已自动回滚: {err}"));
            }
            host_switched = true;
        }

        let view = {
            let mut g = self.data.lock().unwrap();
            let stored = {
                let td = g.tools.entry(tool.as_str().to_string()).or_default();
                td.active_account_id = Some(account_id.to_string());
                td.accounts.iter().find(|a| a.id == account_id).unwrap().clone()
            };
            self.save_file(&g)?;
            self.to_view(&stored, Some(account_id), 0, None)
        };
        Ok(ActivateOutcome {
            account: view,
            host_switched,
        })
    }

    pub fn next_failover_candidate(
        &self,
        tool: ToolKind,
        current_id: Option<&str>,
        tried_ids: &[String],
    ) -> Option<StoredAccount> {
        let g = self.data.lock().unwrap();
        let td = g.tools.get(tool.as_str())?;
        if !td.auto_failover {
            return None;
        }
        let runtime = self.runtime.lock().unwrap();
        let mut candidates: Vec<(usize, &StoredAccount)> = td
            .accounts
            .iter()
            .enumerate()
            .filter(|(_, a)| {
                if current_id == Some(a.id.as_str()) {
                    return false;
                }
                if tried_ids.iter().any(|t| t == &a.id) {
                    return false;
                }
                if !a.allow_auto_failover {
                    return false;
                }
                let st = runtime
                    .get(&(tool, a.id.clone()))
                    .map(|(s, _)| *s)
                    .unwrap_or(AccountState::Ready);
                st == AccountState::Ready
            })
            .collect();
        candidates.sort_by(|(idx_a, a), (idx_b, b)| {
            b.priority.cmp(&a.priority).then_with(|| idx_a.cmp(idx_b))
        });
        candidates.first().map(|(_, a)| (*a).clone())
    }

    /// Snapshot/copy Codex or Claude session log into `to_id`'s isolated home so native resume
    /// preserves full model context across accounts on the same session.
    pub fn copy_session_history_for_switch(
        &self,
        tool: ToolKind,
        from_id: Option<&str>,
        to_id: &str,
        external_id: &str,
    ) -> Result<bool> {
        let to_acc = self
            .get_stored(tool, to_id)
            .ok_or_else(|| anyhow!("目标账号不存在"))?;
        if Self::effective_isolation(tool, to_acc.auth_kind) != AccountIsolation::Isolated {
            return Ok(false);
        }
        match tool {
            ToolKind::Codex => {
                let src_home = match from_id {
                    Some(fid) => self.profile_home(ToolKind::Codex, fid),
                    None => self.native_codex_home(),
                };
                let dst_home = self.profile_home(ToolKind::Codex, to_id);
                let src_sessions = src_home.join("sessions");
                let Some(src_file) = find_jsonl_containing_id(&src_sessions, external_id) else {
                    return Ok(false);
                };
                let rel = src_file.strip_prefix(&src_sessions).unwrap_or(Path::new("migrated.jsonl"));
                let dst_file = dst_home.join("sessions").join(rel);
                if let Some(parent) = dst_file.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                if !dst_file.exists() {
                    if std::fs::hard_link(&src_file, &dst_file).is_err() {
                        std::fs::copy(&src_file, &dst_file)?;
                    }
                    crate::util::restrict_file(&dst_file);
                }
                Ok(true)
            }
            // Qoder's `<config dir>/projects/<dir>/<id>.jsonl` is laid out like Claude's
            ToolKind::Claude | ToolKind::Qoder => {
                let src_dir = match (from_id, tool) {
                    (Some(fid), _) => self.profile_home(tool, fid),
                    (None, ToolKind::Qoder) => self.home_dir.join(".qoder"),
                    (None, _) => self.native_claude_dir(),
                };
                let dst_dir = self.profile_home(tool, to_id);
                let src_projects = src_dir.join("projects");
                let filename = format!("{external_id}.jsonl");
                let Some(src_file) = find_exact_file_recursive(&src_projects, &filename) else {
                    return Ok(false);
                };
                let rel = src_file.strip_prefix(&src_projects).unwrap_or(Path::new(&filename));
                let dst_file = dst_dir.join("projects").join(rel);
                if let Some(parent) = dst_file.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                if !dst_file.exists() {
                    if std::fs::hard_link(&src_file, &dst_file).is_err() {
                        std::fs::copy(&src_file, &dst_file)?;
                    }
                    crate::util::restrict_file(&dst_file);
                }
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    pub fn export_bundle(&self, tool: ToolKind, account_id: &str) -> Result<serde_json::Value> {
        let acc = self
            .get_stored(tool, account_id)
            .ok_or_else(|| anyhow!("账号 {account_id} 不存在"))?;
        let mut snap = self.load_snapshot(tool, account_id).unwrap_or_default();
        if Self::effective_isolation(tool, acc.auth_kind) == AccountIsolation::Isolated
            && acc.auth_kind == AccountAuthKind::Oauth
        {
            let home = self.profile_home(tool, account_id);
            let files: &[&str] = match tool {
                ToolKind::Codex => &["auth.json", "config.toml"],
                ToolKind::Claude => &[".credentials.json", ".claude.json"],
                ToolKind::Qoder => &QODER_LOGIN_FILES,
                _ => &[],
            };
            for rel in files {
                let p = home.join(rel);
                if let Ok(bytes) = std::fs::read(&p) {
                    snap.profile_files.insert(
                        (*rel).to_string(),
                        base64::engine::general_purpose::STANDARD.encode(bytes),
                    );
                }
            }
        }
        Ok(serde_json::json!({
            "version": 1,
            "tool": acc.tool,
            "exportedAt": crate::util::now_ms(),
            "account": {
                "label": acc.label,
                "authKind": acc.auth_kind,
                "allowAutoFailover": acc.allow_auto_failover,
                "priority": acc.priority,
                "identity": acc.identity,
                "apiConfig": acc.api_config,
            },
            "snapshot": snap,
        }))
    }

    /// Import an exported credential bundle as a new account profile on this host.
    pub fn import_bundle(&self, bundle: serde_json::Value) -> Result<AccountProfileView> {
        if bundle["version"].as_u64() != Some(1) {
            bail!("不支持的凭据包版本");
        }
        let tool: ToolKind = serde_json::from_value(bundle["tool"].clone())
            .map_err(|_| anyhow!("凭据包缺少或包含无效的工具类型"))?;
        if !tool.supports_accounts() {
            bail!("该工具不支持账号管理");
        }
        let meta = &bundle["account"];
        let auth_kind: AccountAuthKind = serde_json::from_value(meta["authKind"].clone())?;
        let identity: AccountIdentity =
            serde_json::from_value(meta["identity"].clone()).unwrap_or_default();
        let api_config: Option<ApiKeyConfig> =
            serde_json::from_value(meta["apiConfig"].clone()).unwrap_or(None);
        let allow_auto_failover = meta["allowAutoFailover"]
            .as_bool()
            .unwrap_or(auth_kind == AccountAuthKind::Oauth);
        let priority = meta["priority"].as_i64().unwrap_or(0) as i32;
        let snap: SnapshotBundle =
            serde_json::from_value(bundle["snapshot"].clone()).unwrap_or_default();

        let new_id = Self::new_account_id();
        let dir = self.account_dir(tool, &new_id);
        let home = self.profile_home(tool, &new_id);
        std::fs::create_dir_all(&home)?;

        for (rel, b64) in &snap.profile_files {
            if !is_safe_rel(rel) {
                bail!("凭据包包含非法路径: {rel}");
            }
            let raw = base64::engine::general_purpose::STANDARD
                .decode(b64)
                .map_err(|e| anyhow!("凭据包解码失败: {e}"))?;
            let dst = home.join(rel);
            if let Some(p) = dst.parent() {
                std::fs::create_dir_all(p)?;
            }
            std::fs::write(&dst, raw)?;
            crate::util::restrict_file(&dst);
        }
        if tool == ToolKind::Codex {
            ensure_codex_profile_files(&home, api_config.as_ref())?;
        }
        if !snap.files.is_empty() || !snap.keyring.is_empty() || !snap.db_rows.is_empty() {
            let mut host_only = snap;
            host_only.profile_files.clear();
            self.save_snapshot(tool, &new_id, &host_only)?;
        }

        let mut g = self.data.lock().unwrap();
        let raw_label = meta["label"].as_str();
        let label = pick_label(raw_label, &identity, tool, auth_kind, &*g, tool);
        let stored = StoredAccount {
            id: new_id.clone(),
            tool,
            label,
            auth_kind,
            allow_auto_failover,
            priority,
            identity,
            api_config,
            updated_at: crate::util::now_ms(),
        };
        let active_id = {
            let td = g.tools.entry(tool.as_str().to_string()).or_default();
            td.accounts.push(stored.clone());
            if td.active_account_id.is_none()
                && Self::effective_isolation(tool, auth_kind) == AccountIsolation::Isolated
            {
                td.active_account_id = Some(new_id.clone());
            }
            td.active_account_id.clone()
        };
        if let Err(e) = self.save_file(&g) {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(e);
        }
        Ok(self.to_view(&stored, active_id.as_deref(), 0, None))
    }

    /// The login a sign-in left in `home`, a staging home it ran in (HOME / USERPROFILE / APPDATA... pointed there), with
    /// the same relative paths as the host's. Keyring entries are global: read live only for Antigravity, whose CLI can't
    /// be told to use files (the caller puts the host's entries back right after)
    pub fn take_snapshot_in_home(&self, tool: ToolKind, home: &Path) -> Result<SnapshotBundle> {
        let tmp = AccountManager::with_dirs(self.base_dir.clone(), home.to_path_buf());
        let mut paths = tmp.host_auth_paths(tool);
        if tool == ToolKind::Cursor {
            for rel in ["AppData/Roaming/Cursor/auth.json", ".cursor/auth.json", ".config/cursor/auth.json"] {
                if !paths.iter().any(|(r, _)| r == rel) {
                    paths.push((rel.to_string(), home.join(rel)));
                }
            }
        }
        let mut files = BTreeMap::new();
        for (rel, abs) in paths {
            if !abs.starts_with(home) || !abs.is_file() {
                continue;
            }
            if let Ok(bytes) = std::fs::read(&abs) {
                files.insert(rel, base64::engine::general_purpose::STANDARD.encode(bytes));
            }
        }
        let keyring = if tool == ToolKind::Antigravity && keyring_managed(tool) {
            keyring::snapshot_tool(tool).unwrap_or_default()
        } else {
            vec![]
        };
        let db_rows = tmp.take_db_rows(tool)?;
        Ok(SnapshotBundle { files, keyring, profile_files: BTreeMap::new(), db_rows })
    }

    #[allow(dead_code)]
    pub fn take_host_snapshot(&self, tool: ToolKind) -> Result<SnapshotBundle> {
        let _guard = self.host_lock.lock().unwrap();
        self.take_host_snapshot_unlocked(tool)
    }

    pub fn restore_host_snapshot(&self, tool: ToolKind, snap: &SnapshotBundle) -> Result<()> {
        let _guard = self.host_lock.lock().unwrap();
        self.restore_host_snapshot_unlocked(tool, snap)
    }

    #[allow(dead_code)]
    pub fn clear_host_live(&self, tool: ToolKind) -> Result<()> {
        let _guard = self.host_lock.lock().unwrap();
        let empty = SnapshotBundle::default();
        self.restore_host_snapshot_unlocked(tool, &empty)
    }

    fn take_host_snapshot_unlocked(&self, tool: ToolKind) -> Result<SnapshotBundle> {
        let mut files = BTreeMap::new();
        for (rel, abs) in self.host_auth_paths(tool) {
            if abs.is_file() {
                if let Ok(bytes) = std::fs::read(&abs) {
                    files.insert(
                        rel,
                        base64::engine::general_purpose::STANDARD.encode(bytes),
                    );
                }
            }
        }
        let keyring = if keyring_managed(tool) { keyring::snapshot_tool(tool).unwrap_or_default() } else { vec![] };
        let db_rows = self.take_db_rows(tool)?;
        Ok(SnapshotBundle {
            files,
            keyring,
            profile_files: BTreeMap::new(),
            db_rows,
        })
    }

    /// Kiro's SQLite database: `<data dir>/kiro-cli/data.sqlite3` (`dirs::data_local_dir`, so XDG_DATA_HOME on Linux)
    fn kiro_db_path(&self) -> PathBuf {
        if cfg!(windows) {
            return self.home_dir.join("AppData/Local/kiro-cli/data.sqlite3");
        }
        if cfg!(target_os = "macos") {
            return self.home_dir.join("Library/Application Support/kiro-cli/data.sqlite3");
        }
        // Only the real home follows XDG_DATA_HOME (tests point home_dir elsewhere)
        let xdg = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).filter(|p| p.is_absolute());
        match xdg {
            Some(x) if self.home_dir == crate::paths::home() => x.join("kiro-cli/data.sqlite3"),
            _ => self.home_dir.join(".local/share/kiro-cli/data.sqlite3"),
        }
    }

    /// Login rows of tools that keep them in SQLite. Kiro (checked on kiro-cli 2.35 / 2.37): tokens and the OIDC client
    /// registration are `auth_kv` rows `kirocli:*` (`kirocli:odic:token`, `kirocli:social:token`, `kirocli:external-idp:token`,
    /// `kirocli:odic:device-registration`, ...), and the Identity Center start URL / region / chosen profile are `state` rows.
    /// Nothing is captured without a token row, so leftover state alone doesn't count as a login.
    fn take_db_rows(&self, tool: ToolKind) -> Result<Vec<DbRow>> {
        if tool != ToolKind::Kiro {
            return Ok(vec![]);
        }
        let path = self.kiro_db_path();
        if !path.is_file() {
            return Ok(vec![]);
        }
        let conn = rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| anyhow!("打开 Kiro 数据库失败: {e}"))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let read = |table: &str, filter: &str, params: &[&str]| -> Result<Vec<DbRow>> {
            let sql = format!("SELECT key, value FROM {table} WHERE {filter}");
            let mut stmt = match conn.prepare(&sql) {
                Ok(s) => s,
                // An older or not yet migrated database without the table
                Err(_) => return Ok(vec![]),
            };
            let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |r| {
                let key: String = r.get(0)?;
                let (text, blob) = match r.get_ref(1)? {
                    rusqlite::types::ValueRef::Text(t) => (Some(String::from_utf8_lossy(t).into_owned()), None),
                    rusqlite::types::ValueRef::Blob(b) => (None, Some(base64::engine::general_purpose::STANDARD.encode(b))),
                    rusqlite::types::ValueRef::Integer(i) => (Some(i.to_string()), None),
                    rusqlite::types::ValueRef::Real(f) => (Some(f.to_string()), None),
                    rusqlite::types::ValueRef::Null => (None, None),
                };
                Ok(DbRow { table: table.to_string(), key, text, blob })
            })?;
            Ok(rows.collect::<std::result::Result<_, _>>()?)
        };
        let auth = read("auth_kv", "key LIKE 'kirocli:%'", &[])?;
        if !auth.iter().any(|r| r.key.ends_with(":token")) {
            return Ok(vec![]);
        }
        let mut out = auth;
        out.extend(read("state", "key IN (?1, ?2, ?3)", &KIRO_STATE_KEYS)?);
        Ok(out)
    }

    /// Replace the tool's login rows with `rows` in one transaction (empty `rows`: sign out)
    fn restore_db_rows(&self, tool: ToolKind, rows: &[DbRow]) -> Result<()> {
        if tool != ToolKind::Kiro {
            return Ok(());
        }
        let path = self.kiro_db_path();
        if !path.is_file() {
            if rows.is_empty() {
                return Ok(());
            }
            bail!("没有找到 Kiro 数据库 {}，请先在这台机器上运行一次 kiro-cli", path.display());
        }
        let mut conn = rusqlite::Connection::open(&path).map_err(|e| anyhow!("打开 Kiro 数据库失败: {e}"))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM auth_kv WHERE key LIKE 'kirocli:%'", [])?;
        tx.execute("DELETE FROM state WHERE key IN (?1, ?2, ?3)", KIRO_STATE_KEYS)?;
        for r in rows {
            let table = match r.table.as_str() {
                "auth_kv" if r.key.starts_with("kirocli:") => "auth_kv",
                "state" if KIRO_STATE_KEYS.contains(&r.key.as_str()) => "state",
                _ => bail!("非法的 Kiro 凭据记录: {}.{}", r.table, r.key),
            };
            let sql = format!("INSERT OR REPLACE INTO {table} (key, value) VALUES (?1, ?2)");
            match (&r.text, &r.blob) {
                (_, Some(b64)) => {
                    let bytes = base64::engine::general_purpose::STANDARD.decode(b64).map_err(|e| anyhow!("快照解码失败: {e}"))?;
                    tx.execute(&sql, rusqlite::params![r.key, bytes])?;
                }
                (Some(t), None) => {
                    tx.execute(&sql, rusqlite::params![r.key, t])?;
                }
                (None, None) => {
                    tx.execute(&sql, rusqlite::params![r.key, rusqlite::types::Null])?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    fn restore_host_snapshot_unlocked(&self, tool: ToolKind, snap: &SnapshotBundle) -> Result<()> {
        // The database first: it's the step that can fail (locked), before any file is touched
        self.restore_db_rows(tool, &snap.db_rows)?;
        for (rel, abs) in self.host_auth_paths(tool) {
            if !snap.files.contains_key(&rel) && abs.exists() {
                let _ = std::fs::remove_file(&abs);
            }
        }
        for (rel, b64) in &snap.files {
            if !is_safe_rel(rel) {
                bail!("非法凭据文件路径: {rel}");
            }
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(b64)
                .map_err(|e| anyhow!("快照解码失败: {e}"))?;
            let abs = self.resolve_rel_path(rel);
            if let Some(parent) = abs.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&abs, bytes)?;
            crate::util::restrict_file(&abs);
        }
        if keyring_managed(tool) {
            keyring::restore_tool(tool, &snap.keyring)?;
        }
        if tool == ToolKind::Cursor {
            self.sanitize_cursor_cli_config(snap);
        }
        Ok(())
    }

    fn cursor_cli_config_paths(&self) -> Vec<PathBuf> {
        let mut out = vec![
            self.home_dir.join(".cursor/cli-config.json"),
            self.home_dir.join(".config/cursor/cli-config.json"),
        ];
        if let Ok(custom) = std::env::var("CURSOR_CONFIG_DIR") {
            if !custom.trim().is_empty() {
                out.insert(0, PathBuf::from(custom.trim()).join("cli-config.json"));
            }
        }
        out
    }

    /// Clear stale `authInfo` (such as `activeTeamId`) and `privacyCache` from Cursor's `cli-config.json`
    /// when switching to an empty or different Cursor OAuth account, preserving user editor/model settings.
    fn sanitize_cursor_cli_config(&self, snap: &SnapshotBundle) {
        let target_sub = cursor_snapshot_sub(snap);
        for cfg_path in self.cursor_cli_config_paths() {
            let Ok(raw) = std::fs::read_to_string(&cfg_path) else {
                continue;
            };
            let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&raw) else {
                continue;
            };
            let Some(obj) = v.as_object_mut() else {
                continue;
            };
            let cur_sub = obj
                .get("authInfo")
                .and_then(|a| a.get("authId"))
                .and_then(|s| s.as_str());
            let should_clear = snap.is_empty()
                || match (cur_sub, target_sub.as_deref()) {
                    (Some(c), Some(t)) => c != t,
                    (Some(_), None) => true,
                    _ => false,
                };
            if should_clear {
                let mut modified = false;
                if obj.remove("authInfo").is_some() {
                    modified = true;
                }
                if obj.remove("privacyCache").is_some() {
                    modified = true;
                }
                if modified {
                    if let Ok(updated) = serde_json::to_string_pretty(&v) {
                        let _ = std::fs::write(&cfg_path, updated);
                    }
                }
            }
        }
    }

    fn native_codex_home(&self) -> PathBuf {
        self.home_dir.join(".codex")
    }

    fn native_claude_dir(&self) -> PathBuf {
        self.home_dir.join(".claude")
    }

    fn resolve_rel_path(&self, rel: &str) -> PathBuf {
        self.home_dir.join(rel)
    }

    fn host_auth_paths(&self, tool: ToolKind) -> Vec<(String, PathBuf)> {
        let mut res = Vec::new();
        let add = |res: &mut Vec<(String, PathBuf)>, rel: &str| {
            res.push((rel.to_string(), self.home_dir.join(rel)));
        };
        match tool {
            ToolKind::Antigravity => {
                for rel in [
                    ".gemini/antigravity-cli/antigravity-oauth-token",
                    ".gemini/antigravity-cli/oauth_creds.json",
                    ".gemini/antigravity-cli/google_accounts.json",
                    ".gemini/oauth_creds.json",
                    ".gemini/google_accounts.json",
                ] {
                    add(&mut res, rel);
                }
            }
            ToolKind::Cursor => {
                for rel in [".cursor/auth.json", ".config/cursor/auth.json"] {
                    add(&mut res, rel);
                }
                if let Some(p) = native_cursor_extra_auth(&self.home_dir) {
                    if let Ok(rel) = p.strip_prefix(&self.home_dir) {
                        let rel_s = rel.to_string_lossy().replace('\\', "/");
                        if !res.iter().any(|(r, _)| r == &rel_s) {
                            res.push((rel_s, p));
                        }
                    }
                }
            }
            ToolKind::Kiro => {
                // The login lives in SQLite (`take_db_rows`), not in files
            }
            ToolKind::Qoder => {
                // Only for the native status: accounts are isolated (`QODER_LOGIN_FILES` in each config dir)
                for rel in [".qoder/.auth/user", ".qoder/.auth/id"] {
                    add(&mut res, rel);
                }
            }
            ToolKind::Codebuddy => {
                add(&mut res, CODEBUDDY_AUTH_FILE);
            }
            ToolKind::Claude => {
                add(&mut res, ".claude/.credentials.json");
            }
            ToolKind::Codex => {
                add(&mut res, ".codex/auth.json");
            }
            ToolKind::Shell | ToolKind::Files => {}
        }
        res
    }

    /// Kiro: remember who `snap` is signed in as (`kiro-cli whoami`), unless already known. `home` is where that
    /// login lives (None: the host). The real home only: tests point `home_dir` elsewhere and must not run kiro-cli
    pub fn learn_kiro_email(&self, tool: ToolKind, snap: &SnapshotBundle, home: Option<&Path>) {
        if tool != ToolKind::Kiro || (home.is_none() && self.home_dir != crate::paths::home()) {
            return;
        }
        let Some(key) = kiro_token_key(snap) else {
            return;
        };
        if KIRO_EMAILS.lock().unwrap().contains_key(&key) {
            return;
        }
        if let Some(email) = run_kiro_whoami(home) {
            KIRO_EMAILS.lock().unwrap().insert(key, email);
        }
    }

    fn parse_host_identity(&self, tool: ToolKind, snap: &SnapshotBundle) -> AccountIdentity {
        let decode_file = |rel_suffix: &str| -> Option<Vec<u8>> {
            snap.files
                .iter()
                .find(|(k, _)| k.ends_with(rel_suffix))
                .and_then(|(_, b64)| base64::engine::general_purpose::STANDARD.decode(b64).ok())
        };
        match tool {
            ToolKind::Antigravity => {
                if let Some(bytes) = decode_file("antigravity-oauth-token") {
                    if let Some(id) = parse_antigravity_token_bytes(&bytes) {
                        return id;
                    }
                }
                for e in &snap.keyring {
                    // Windows keeps it as service `gemini:antigravity`, account `antigravity`
                    if e.service.contains("antigravity-oauth-token")
                        || e.account.contains("antigravity-oauth-token")
                        || e.service.contains("jetski-standalone-oauth-token")
                        || e.account.contains("jetski-standalone-oauth-token")
                        || e.service == "gemini:antigravity"
                    {
                        if let Some(id) = parse_antigravity_token_bytes(&e.secret_bytes()) {
                            return id;
                        }
                    }
                }
                if let Some(bytes) = decode_file("google_accounts.json") {
                    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                        if let Some(email) = v["active"].as_str().filter(|s| !s.is_empty()) {
                            return AccountIdentity {
                                email: Some(email.to_string()),
                                account_key: Some(email.to_string()),
                                ..Default::default()
                            };
                        }
                    }
                }
                AccountIdentity::default()
            }
            ToolKind::Cursor => {
                let mut email: Option<String> = None;
                let mut sub: Option<String> = None;
                if let Some(bytes) = decode_file("auth.json") {
                    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                        email = v["email"]
                            .as_str()
                            .or_else(|| v["user"]["email"].as_str())
                            .filter(|s| !s.is_empty())
                            .map(String::from);
                        if let Some(claims) = v["accessToken"].as_str().and_then(decode_jwt_claims) {
                            if email.is_none() {
                                email = claims["email"].as_str().filter(|s| !s.is_empty()).map(String::from);
                            }
                            sub = claims["sub"].as_str().filter(|s| !s.is_empty()).map(String::from);
                        }
                    }
                }
                if sub.is_none() {
                    for e in &snap.keyring {
                        if e.service.contains("cursor-access-token") {
                            if let Some(claims) = decode_jwt_claims(e.secret.trim()) {
                                if email.is_none() {
                                    email = claims["email"].as_str().filter(|s| !s.is_empty()).map(String::from);
                                }
                                sub = claims["sub"].as_str().filter(|s| !s.is_empty()).map(String::from);
                                break;
                            }
                        }
                    }
                }
                let mut workspace: Option<String> = None;
                for cfg_path in self.cursor_cli_config_paths() {
                    let Ok(raw) = std::fs::read_to_string(&cfg_path) else {
                        continue;
                    };
                    let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
                        continue;
                    };
                    let ai = &v["authInfo"];
                    if !ai.is_object() {
                        continue;
                    }
                    let cfg_sub = ai["authId"].as_str().filter(|s| !s.is_empty());
                    if let (Some(s), Some(cs)) = (sub.as_deref(), cfg_sub) {
                        if s != cs {
                            continue;
                        }
                    }
                    if email.is_none() {
                        email = ai["email"].as_str().filter(|s| !s.is_empty()).map(String::from);
                    }
                    if workspace.is_none() {
                        workspace = ai["teamName"].as_str().filter(|s| !s.is_empty()).map(String::from);
                    }
                    if sub.is_none() {
                        sub = cfg_sub.map(String::from);
                    }
                    break;
                }
                AccountIdentity {
                    account_key: sub.or_else(|| email.clone()),
                    email,
                    workspace,
                    ..Default::default()
                }
            }
            ToolKind::Codex => {
                if let Some(bytes) = decode_file("auth.json") {
                    return parse_codex_auth_bytes(&bytes).1;
                }
                AccountIdentity::default()
            }
            ToolKind::Claude => {
                if let Some(bytes) = decode_file(".credentials.json") {
                    return parse_claude_identity(&bytes, None);
                }
                AccountIdentity::default()
            }
            ToolKind::Kiro => {
                // The token rows carry no email: `learn_kiro_email` asks `kiro-cli whoami` once per login and caches it.
                // The workspace says how it signed in
                let json = |key: &str| {
                    snap.db_rows
                        .iter()
                        .find(|r| r.table == "auth_kv" && r.key == key)
                        .and_then(|r| r.text.as_deref())
                        .and_then(|t| serde_json::from_str::<serde_json::Value>(t).ok())
                };
                let workspace = if let Some(t) = json("kirocli:odic:token") {
                    match t["start_url"].as_str().filter(|s| !s.is_empty()) {
                        Some(u) if !u.contains("view.awsapps.com/start") => Some(u.to_string()),
                        _ => Some("Builder ID".to_string()),
                    }
                } else if json("kirocli:social:token").is_some() {
                    Some("Google / GitHub".to_string())
                } else if json("kirocli:external-idp:token").is_some() {
                    Some("External IdP".to_string())
                } else {
                    None
                };
                let email = kiro_cached_email(snap);
                AccountIdentity { account_key: email.clone(), email, workspace, plan: None }
            }
            ToolKind::Codebuddy => {
                // {"account": {"uid", "nickname", ...}, "auth": {"accessToken", "domain", ...}, "accounts": [...]};
                // there is no email, the nickname stands in for it (as in the quota probe)
                let Some(v) = decode_file(".info").and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok()) else {
                    return AccountIdentity::default();
                };
                let s = |x: &serde_json::Value| x.as_str().filter(|s| !s.is_empty()).map(String::from);
                let a = &v["account"];
                AccountIdentity {
                    email: s(&a["nickname"]),
                    account_key: s(&a["uid"]).or_else(|| s(&a["uniqueUserId"])),
                    workspace: s(&v["auth"]["domain"]),
                    plan: None,
                }
            }
            _ => AccountIdentity::default(),
        }
    }

    /// Inspect the host's native CLI login status & conflicting environment variables.
    fn probe_native_status(
        &self,
        tool: ToolKind,
        accounts: &[StoredAccount],
        active_id: Option<&str>,
    ) -> ToolNativeStatus {
        let cursor_env_key = if tool == ToolKind::Cursor {
            crate::tools::host_env_var("CURSOR_API_KEY")
        } else {
            None
        };
        self.probe_native_status_with_cursor_env(tool, accounts, active_id, cursor_env_key.as_deref())
    }

    fn probe_native_status_with_cursor_env(
        &self,
        tool: ToolKind,
        accounts: &[StoredAccount],
        active_id: Option<&str>,
        cursor_env_key: Option<&str>,
    ) -> ToolNativeStatus {
        let env_vars: &[&str] = match tool {
            ToolKind::Codex => &["OPENAI_API_KEY", "OPENAI_BASE_URL", "CODEX_HOME"],
            ToolKind::Claude => &[
                "ANTHROPIC_API_KEY",
                "ANTHROPIC_AUTH_TOKEN",
                "ANTHROPIC_BASE_URL",
                "CLAUDE_CONFIG_DIR",
            ],
            ToolKind::Antigravity => &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
            ToolKind::Cursor => &["CURSOR_API_KEY", "CURSOR_AUTH_TOKEN"],
            _ => &[],
        };
        let mut present_envs: Vec<&str> = env_vars
            .iter()
            .copied()
            .filter(|k| crate::tools::host_env_var(k).is_some())
            .collect();
        if tool == ToolKind::Cursor
            && cursor_env_key.is_some_and(|s| !s.trim().is_empty())
            && !present_envs.contains(&"CURSOR_API_KEY")
        {
            present_envs.insert(0, "CURSOR_API_KEY");
        }
        if tool == ToolKind::Cursor && cursor_env_key.is_none() {
            // In unit tests with explicit `None`, don't leak ambient process CURSOR_API_KEY.
            present_envs.retain(|k| *k != "CURSOR_API_KEY");
        }
        let env_override = (!present_envs.is_empty()).then(|| present_envs.join(", "));

        match tool {
            ToolKind::Codex => {
                let auth_path = self.native_codex_home().join("auth.json");
                let bytes = std::fs::read(&auth_path).ok().or_else(|| {
                    keyring::snapshot_tool(ToolKind::Codex)
                        .ok()
                        .and_then(|k| k.into_iter().find(|e| !e.secret.is_empty()).map(|e| e.secret.into_bytes()))
                });
                // config.toml decides what Codex really uses: a custom model_provider takes over from auth.json
                if let Some(actual) = self.codex_actual(&self.native_codex_home(), None) {
                    let has_oauth = bytes.as_deref().is_some_and(|b| parse_codex_auth_bytes(b).0 == AccountAuthKind::Oauth);
                    let logged_in = actual.key_set || (actual.method == "providerOpenaiAuth" && bytes.is_some()) || actual.method == "providerNoAuth";
                    let matched = find_codex_provider_account(self, accounts, &actual, self.host_env_for(&actual).as_deref());
                    return ToolNativeStatus {
                        logged_in,
                        auth_method: Some(match actual.method.as_str() {
                            "providerApiKey" => "provider_api_key",
                            "providerOpenaiAuth" => "provider_openai_auth",
                            _ => "provider_no_auth",
                        }.into()),
                        email: None,
                        plan: None,
                        env_override: None,
                        matched_account_id: matched,
                        env_api_key_matched_id: None,
                        has_local_oauth: has_oauth,
                        detail: None,
                        actual: Some(actual),
                    };
                }
                if let Some(bytes) = bytes {
                    let (ak, ident, key_opt) = parse_codex_auth_bytes(&bytes);
                    let matched = find_matching_account(accounts, &ident, ak);
                    ToolNativeStatus {
                        logged_in: true,
                        actual: None,
                        auth_method: Some(match ak {
                            AccountAuthKind::Oauth => "chatgpt".into(),
                            AccountAuthKind::ApiKey => "api_key".into(),
                        }),
                        email: ident.email,
                        plan: ident.plan,
                        env_override,
                        matched_account_id: matched,
                        env_api_key_matched_id: None,
                        has_local_oauth: ak == AccountAuthKind::Oauth,
                        detail: key_opt.map(|_| "使用 OPENAI_API_KEY 登录".to_string()),
                    }
                } else {
                    let has_env_key = std::env::var_os("OPENAI_API_KEY").is_some_and(|v| !v.is_empty());
                    ToolNativeStatus {
                        logged_in: has_env_key,
                        actual: None,
                        auth_method: has_env_key.then(|| "env_api_key".into()),
                        email: None,
                        plan: None,
                        env_override,
                        matched_account_id: None,
                        env_api_key_matched_id: None,
                        has_local_oauth: false,
                        detail: if has_env_key {
                            Some("使用宿主环境变量 OPENAI_API_KEY".into())
                        } else {
                            Some("未检测到系统 ~/.codex 登录凭据".into())
                        },
                    }
                }
            }
            ToolKind::Claude => {
                let dir = self.native_claude_dir();
                let bytes = std::fs::read(dir.join(".credentials.json")).ok().or_else(|| {
                    keyring::snapshot_tool(ToolKind::Claude)
                        .ok()
                        .and_then(|k| k.into_iter().find(|e| !e.secret.is_empty()).map(|e| e.secret.into_bytes()))
                });
                let dot = extract_claude_safe_meta(&self.home_dir.join(".claude.json"));
                if let Some(bytes) = bytes {
                    let ident = parse_claude_identity(&bytes, dot.as_ref());
                    let matched = find_matching_account(accounts, &ident, AccountAuthKind::Oauth);
                    ToolNativeStatus {
                        logged_in: true,
                        actual: None,
                        auth_method: Some("oauth".into()),
                        email: ident.email,
                        plan: ident.plan,
                        env_override,
                        matched_account_id: matched,
                        env_api_key_matched_id: None,
                        has_local_oauth: true,
                        detail: None,
                    }
                } else {
                    let has_env = std::env::var_os("ANTHROPIC_API_KEY").is_some_and(|v| !v.is_empty())
                        || std::env::var_os("ANTHROPIC_AUTH_TOKEN").is_some_and(|v| !v.is_empty());
                    ToolNativeStatus {
                        logged_in: has_env,
                        actual: None,
                        auth_method: has_env.then(|| "env_api_key".into()),
                        email: None,
                        plan: None,
                        env_override,
                        matched_account_id: None,
                        env_api_key_matched_id: None,
                        has_local_oauth: false,
                        detail: if has_env {
                            Some("使用宿主环境变量 ANTHROPIC_API_KEY / AUTH_TOKEN".into())
                        } else {
                            Some("未检测到系统 ~/.claude 登录凭据".into())
                        },
                    }
                }
            }
            ToolKind::Cursor => {
                let snap = self.take_host_snapshot_unlocked(ToolKind::Cursor).unwrap_or_default();
                let file_api_key = cursor_snapshot_api_key(&snap);
                let is_api_key_snap = cursor_snapshot_is_api_key(&snap);
                let has_local_oauth = !snap.is_empty() && !is_api_key_snap;
                let env_key = cursor_env_key.map(str::trim).filter(|s| !s.is_empty());
                if let Some(key) = env_key {
                    let matched = find_matching_api_key(accounts, key);
                    let matched_acc = matched.as_deref().and_then(|id| accounts.iter().find(|a| a.id == id));
                    let file_ident = if file_api_key.as_deref() == Some(key) {
                        self.parse_host_identity(ToolKind::Cursor, &snap)
                    } else {
                        AccountIdentity::default()
                    };
                    let email = matched_acc.and_then(|a| a.identity.email.clone()).or(file_ident.email);
                    let plan = matched_acc.and_then(|a| a.identity.plan.clone()).or(file_ident.plan);
                    let detail = if has_local_oauth {
                        Some("宿主环境变量 CURSOR_API_KEY 优先级高于本地 OAuth 登录；切换到授权登录账号时会自动屏蔽该变量".into())
                    } else {
                        Some("使用宿主环境变量 CURSOR_API_KEY；切换到授权登录账号时会自动屏蔽该变量".into())
                    };
                    ToolNativeStatus {
                        logged_in: true,
                        actual: None,
                        auth_method: Some("env_api_key".into()),
                        email,
                        plan,
                        env_override,
                        matched_account_id: matched.clone(),
                        env_api_key_matched_id: matched,
                        has_local_oauth,
                        detail,
                    }
                } else if is_api_key_snap {
                    let ident = self.parse_host_identity(ToolKind::Cursor, &snap);
                    let matched = file_api_key
                        .as_deref()
                        .and_then(|key| find_matching_api_key(accounts, key))
                        .or_else(|| find_matching_account(accounts, &ident, AccountAuthKind::ApiKey));
                    ToolNativeStatus {
                        logged_in: true,
                        actual: None,
                        auth_method: Some("api_key".into()),
                        email: ident.email,
                        plan: ident.plan,
                        env_override,
                        matched_account_id: matched,
                        env_api_key_matched_id: None,
                        has_local_oauth: false,
                        detail: Some("auth.json 中保存的是 CURSOR_API_KEY 凭据".into()),
                    }
                } else if !snap.is_empty() {
                    let ident = self.parse_host_identity(ToolKind::Cursor, &snap);
                    let matched = find_matching_account(accounts, &ident, AccountAuthKind::Oauth).or_else(|| {
                        active_id
                            .and_then(|id| accounts.iter().find(|a| a.id == id))
                            .filter(|a| a.auth_kind == AccountAuthKind::Oauth)
                            .map(|a| a.id.clone())
                    });
                    ToolNativeStatus {
                        logged_in: true,
                        actual: None,
                        auth_method: Some("oauth".into()),
                        email: ident.email,
                        plan: ident.plan,
                        env_override,
                        matched_account_id: matched,
                        env_api_key_matched_id: None,
                        has_local_oauth: true,
                        detail: None,
                    }
                } else {
                    ToolNativeStatus {
                        logged_in: false,
                        actual: None,
                        auth_method: None,
                        email: None,
                        plan: None,
                        env_override,
                        matched_account_id: None,
                        env_api_key_matched_id: None,
                        has_local_oauth: false,
                        detail: Some("未检测到系统 Cursor 登录凭据或 CURSOR_API_KEY".into()),
                    }
                }
            }
            _ => {
                let snap = self.take_host_snapshot_unlocked(tool).unwrap_or_default();
                let logged_in = !snap.is_empty();
                if logged_in {
                    self.learn_kiro_email(tool, &snap, None);
                }
                let ident = if logged_in {
                    self.parse_host_identity(tool, &snap)
                } else {
                    AccountIdentity::default()
                };
                // Only a single-slot (HostActive) tool's active account is what the host is logged in as
                let host_active = Self::capability(tool).oauth_isolation == AccountIsolation::HostActive;
                let matched = if logged_in {
                    find_matching_account(accounts, &ident, AccountAuthKind::Oauth).or_else(|| {
                        active_id
                            .filter(|_| host_active)
                            .and_then(|id| accounts.iter().find(|a| a.id == id))
                            .filter(|a| a.auth_kind == AccountAuthKind::Oauth)
                            .map(|a| a.id.clone())
                    })
                } else {
                    None
                };
                let detail = if tool == ToolKind::Antigravity && env_override.is_some() {
                    Some("宿主环境变量可能覆盖 OAuth，已在托管账号中自动隔离".into())
                } else if !logged_in {
                    Some("未检测到系统登录凭据".into())
                } else {
                    None
                };
                ToolNativeStatus {
                    logged_in,
                    actual: None,
                    auth_method: logged_in.then(|| "oauth".into()),
                    email: ident.email,
                    plan: ident.plan,
                    env_override,
                    matched_account_id: matched,
                    env_api_key_matched_id: None,
                    has_local_oauth: logged_in,
                    detail,
                }
            }
        }
    }
}

trait IdentityExt {
    fn matches_or_unknown(&self, other: &AccountIdentity) -> bool;
    fn provider_or_url_update(&mut self, cfg: &Option<ApiKeyConfig>);
}

impl IdentityExt for AccountIdentity {
    fn matches_or_unknown(&self, other: &AccountIdentity) -> bool {
        if let (Some(a), Some(b)) = (&self.account_key, &other.account_key) {
            return a == b;
        }
        if let (Some(a), Some(b)) = (&self.email, &other.email) {
            return a.eq_ignore_ascii_case(b);
        }
        true
    }

    fn provider_or_url_update(&mut self, cfg: &Option<ApiKeyConfig>) {
        if let Some(c) = cfg {
            self.plan = (!c.provider_name.is_empty()).then(|| c.provider_name.clone());
            self.workspace = (!c.base_url.is_empty()).then(|| c.base_url.clone());
        }
    }
}

pub fn cursor_auth_api_key(bytes: &[u8]) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    v["apiKey"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

fn cursor_jwt_is_api_key(jwt: &str) -> bool {
    let Some(claims) = decode_jwt_claims(jwt) else {
        return false;
    };
    claims["type"].as_str() == Some("api_key_token") || !claims["apiKeyId"].is_null()
}

pub fn cursor_bytes_are_api_key(bytes: &[u8]) -> bool {
    if cursor_auth_api_key(bytes).is_some() {
        return true;
    }
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return false;
    };
    v["accessToken"].as_str().is_some_and(cursor_jwt_is_api_key)
        || v["refreshToken"].as_str().is_some_and(cursor_jwt_is_api_key)
}

fn cursor_snapshot_api_key(snap: &SnapshotBundle) -> Option<String> {
    if let Some(k) = snap
        .files
        .iter()
        .find(|(k, _)| k.ends_with("auth.json"))
        .and_then(|(_, b64)| base64::engine::general_purpose::STANDARD.decode(b64).ok())
        .and_then(|bytes| cursor_auth_api_key(&bytes))
    {
        return Some(k);
    }
    snap.keyring
        .iter()
        .find(|e| e.service.eq_ignore_ascii_case("cursor-api-key"))
        .and_then(|e| String::from_utf8(e.secret_bytes()).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub fn cursor_snapshot_is_api_key(snap: &SnapshotBundle) -> bool {
    if cursor_snapshot_api_key(snap).is_some() {
        return true;
    }
    for (k, b64) in &snap.files {
        if k.ends_with("auth.json") {
            if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64) {
                if cursor_bytes_are_api_key(&bytes) {
                    return true;
                }
            }
        }
    }
    snap.keyring.iter().any(|e| {
        (e.service.contains("cursor-access-token") || e.service.contains("cursor-refresh-token"))
            && cursor_jwt_is_api_key(e.secret.trim())
    })
}

fn cursor_snapshot_sub(snap: &SnapshotBundle) -> Option<String> {
    if let Some(sub) = snap
        .files
        .iter()
        .find(|(k, _)| k.ends_with("auth.json"))
        .and_then(|(_, b64)| base64::engine::general_purpose::STANDARD.decode(b64).ok())
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|v| {
            v["accessToken"]
                .as_str()
                .and_then(decode_jwt_claims)
                .and_then(|c| c["sub"].as_str().filter(|s| !s.is_empty()).map(String::from))
        })
    {
        return Some(sub);
    }
    snap.keyring
        .iter()
        .find(|e| e.service.contains("cursor-access-token"))
        .and_then(|e| decode_jwt_claims(e.secret.trim()))
        .and_then(|c| c["sub"].as_str().filter(|s| !s.is_empty()).map(String::from))
}

fn find_matching_api_key(accounts: &[StoredAccount], api_key: &str) -> Option<String> {
    accounts
        .iter()
        .find(|a| {
            a.auth_kind == AccountAuthKind::ApiKey
                && a.api_config.as_ref().is_some_and(|c| c.api_key == api_key)
        })
        .map(|a| a.id.clone())
}

fn find_matching_account(
    accounts: &[StoredAccount],
    ident: &AccountIdentity,
    auth_kind: AccountAuthKind,
) -> Option<String> {
    accounts
        .iter()
        .find(|a| {
            a.auth_kind == auth_kind
                && ((ident.account_key.is_some() && a.identity.account_key == ident.account_key)
                    || (ident.email.is_some()
                        && a.identity
                            .email
                            .as_deref()
                            .zip(ident.email.as_deref())
                            .is_some_and(|(x, y)| x.eq_ignore_ascii_case(y))))
        })
        .map(|a| a.id.clone())
}

/// What qodercli's login is made of, under `<config dir>/.auth/`. `user` is encrypted with a key tied to `machine_id`
/// (checked on 1.1.80: a copy without `machine_id` is not logged in), so all three move together
const QODER_LOGIN_FILES: [&str; 3] = [".auth/user", ".auth/id", ".auth/machine_id"];

fn copy_qoder_login(src_auth: &Path, dst_auth: &Path) -> Result<()> {
    std::fs::create_dir_all(dst_auth)?;
    for name in ["user", "id", "machine_id"] {
        let from = src_auth.join(name);
        if from.is_file() {
            let to = dst_auth.join(name);
            std::fs::copy(&from, &to)?;
            crate::util::restrict_file(&to);
        }
    }
    Ok(())
}

/// `pick_label`'s fallback for an OAuth account without an email: the workspace or "<tool> OAuth", maybe with " #n"
fn is_placeholder_oauth_label(label: &str, tool: ToolKind, workspace: Option<&str>) -> bool {
    let generic = format!("{} OAuth", tool.as_str());
    let is = |base: &str| {
        label == base || label.strip_prefix(&format!("{base} #")).is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    };
    is(&generic) || workspace.is_some_and(is)
}

fn unique_label(base: String, existing: &[&str]) -> String {
    if !existing.contains(&base.as_str()) {
        return base;
    }
    for i in 2..=999 {
        let cand = format!("{base} #{i}");
        if !existing.contains(&cand.as_str()) {
            return cand;
        }
    }
    format!("{base} ({})", crate::util::now_ms())
}

fn pick_api_key_label(
    requested: Option<&str>,
    ident: &AccountIdentity,
    tool: ToolKind,
    cfg: &ApiKeyConfig,
    existing_owned: &[String],
) -> String {
    if let Some(s) = requested.map(str::trim).filter(|s| !s.is_empty()) {
        return s.to_string();
    }
    let existing: Vec<&str> = existing_owned.iter().map(String::as_str).collect();
    let base = if let Some(email) = &ident.email {
        format!("{email} (API Key)")
    } else if !cfg.provider_name.is_empty() {
        format!("{} ({})", tool.as_str(), cfg.provider_name)
    } else if tool == ToolKind::Cursor {
        "Cursor API Key".to_string()
    } else {
        format!("{} API Key", tool.as_str())
    };
    unique_label(base, &existing)
}

fn pick_label(
    requested: Option<&str>,
    ident: &AccountIdentity,
    tool: ToolKind,
    auth_kind: AccountAuthKind,
    file: &AccountsFile,
    target_tool: ToolKind,
) -> String {
    if let Some(s) = requested.map(str::trim).filter(|s| !s.is_empty()) {
        return s.to_string();
    }
    let existing: Vec<&str> = file
        .tools
        .get(target_tool.as_str())
        .map(|t| t.accounts.iter().map(|a| a.label.as_str()).collect())
        .unwrap_or_default();
    let base = match (auth_kind, ident.email.as_deref()) {
        (AccountAuthKind::ApiKey, Some(email)) => format!("{email} (API Key)"),
        (_, Some(email)) => email.to_string(),
        // Codex custom provider: named after the provider
        (AccountAuthKind::ApiKey, None) if tool == ToolKind::Codex && ident.plan.is_some() => {
            format!("{} ({})", tool.as_str(), ident.plan.as_deref().unwrap_or_default())
        }
        _ => ident.workspace.clone().unwrap_or_else(|| match auth_kind {
            AccountAuthKind::Oauth => format!("{} OAuth", tool.as_str()),
            AccountAuthKind::ApiKey => {
                if tool == ToolKind::Cursor {
                    "Cursor API Key".to_string()
                } else {
                    format!("{} API Key", tool.as_str())
                }
            }
        }),
    };
    unique_label(base, &existing)
}

fn is_sensitive_env_key(k: &str) -> bool {
    let u = k.to_ascii_uppercase();
    u.contains("KEY") || u.contains("TOKEN") || u.contains("SECRET") || u.contains("PASS") || u.contains("AUTH")
}

fn redact_api_config(cfg: &ApiKeyConfig) -> ApiKeyConfig {
    ApiKeyConfig {
        api_key: if cfg.api_key.is_empty() { String::new() } else { REDACTED.into() },
        auth_token: if cfg.auth_token.is_empty() { String::new() } else { REDACTED.into() },
        base_url: cfg.base_url.clone(),
        default_model: cfg.default_model.clone(),
        provider_name: cfg.provider_name.clone(),
        extra_env: cfg
            .extra_env
            .iter()
            .map(|(k, v)| {
                if !v.is_empty() && is_sensitive_env_key(k) {
                    (k.clone(), REDACTED.into())
                } else {
                    (k.clone(), v.clone())
                }
            })
            .collect(),
    }
}

fn merge_api_config(incoming: &ApiKeyConfig, existing: Option<&ApiKeyConfig>) -> ApiKeyConfig {
    let keep = |val: &str, old: Option<&str>| -> String {
        if val == REDACTED {
            old.unwrap_or("").to_string()
        } else {
            val.trim().to_string()
        }
    };
    let mut extra_env = BTreeMap::new();
    for (k, v) in &incoming.extra_env {
        let k = k.trim().to_string();
        if k.is_empty() {
            continue;
        }
        let val = if v == REDACTED {
            existing.and_then(|e| e.extra_env.get(&k)).cloned().unwrap_or_default()
        } else {
            v.clone()
        };
        extra_env.insert(k, val);
    }
    ApiKeyConfig {
        api_key: keep(&incoming.api_key, existing.map(|e| e.api_key.as_str())),
        auth_token: keep(&incoming.auth_token, existing.map(|e| e.auth_token.as_str())),
        base_url: incoming.base_url.trim().to_string(),
        default_model: incoming.default_model.trim().to_string(),
        provider_name: incoming.provider_name.trim().to_string(),
        extra_env,
    }
}

fn is_safe_rel(rel: &str) -> bool {
    let p = Path::new(rel);
    !p.is_absolute()
        && p.components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
}

fn find_jsonl_containing_id(dir: &Path, id: &str) -> Option<PathBuf> {
    let rd = std::fs::read_dir(dir).ok()?;
    for entry in rd.flatten() {
        let p = entry.path();
        if p.is_dir() {
            if let Some(found) = find_jsonl_containing_id(&p, id) {
                return Some(found);
            }
        } else if p.extension().and_then(|e| e.to_str()) == Some("jsonl")
            && p.file_stem()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(id))
        {
            return Some(p);
        }
    }
    None
}

fn find_exact_file_recursive(dir: &Path, target_name: &str) -> Option<PathBuf> {
    let rd = std::fs::read_dir(dir).ok()?;
    for entry in rd.flatten() {
        let p = entry.path();
        if p.is_dir() {
            if let Some(found) = find_exact_file_recursive(&p, target_name) {
                return Some(found);
            }
        } else if p.file_name().and_then(|n| n.to_str()) == Some(target_name) {
            return Some(p);
        }
    }
    None
}

fn decode_jwt_claims(jwt: &str) -> Option<serde_json::Value> {
    let seg = jwt.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(seg)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(seg))
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub fn parse_codex_auth_bytes(bytes: &[u8]) -> (AccountAuthKind, AccountIdentity, Option<String>) {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return (AccountAuthKind::Oauth, AccountIdentity::default(), None);
    };
    if let Some(api_key) = v["OPENAI_API_KEY"].as_str().filter(|s| !s.is_empty()) {
        if v["tokens"].is_null() {
            return (
                AccountAuthKind::ApiKey,
                AccountIdentity::default(),
                Some(api_key.to_string()),
            );
        }
    }
    let mut ident = AccountIdentity::default();
    ident.account_key = v["tokens"]["account_id"].as_str().map(String::from);
    if let Some(id_tok) = v["tokens"]["id_token"].as_str() {
        if let Some(claims) = decode_jwt_claims(id_tok) {
            ident.email = claims["email"].as_str().map(String::from);
            let auth = &claims["https://api.openai.com/auth"];
            ident.plan = auth["chatgpt_plan_type"].as_str().map(String::from);
            if ident.account_key.is_none() {
                ident.account_key = auth["chatgpt_account_id"].as_str().map(String::from);
            }
        }
    }
    (AccountAuthKind::Oauth, ident, None)
}

pub fn parse_claude_identity(cred_bytes: &[u8], dot_claude: Option<&serde_json::Value>) -> AccountIdentity {
    let mut ident = AccountIdentity::default();
    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(cred_bytes) {
        ident.plan = v["claudeAiOauth"]["subscriptionType"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(String::from);
    }
    if let Some(dot) = dot_claude {
        let oa = &dot["oauthAccount"];
        ident.email = oa["emailAddress"].as_str().filter(|s| !s.is_empty()).map(String::from);
        ident.workspace = oa["organizationName"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(String::from);
        ident.account_key = oa["accountUuid"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(String::from);
    }
    ident
}

fn parse_antigravity_token_bytes(bytes: &[u8]) -> Option<AccountIdentity> {
    let v: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let email = v["id_token_email"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(String::from)
        .or_else(|| {
            v["token"]["id_token"]
                .as_str()
                .or_else(|| v["id_token"].as_str())
                .and_then(decode_jwt_claims)
                .and_then(|c| c["email"].as_str().filter(|s| !s.is_empty()).map(String::from))
        })?;
    let auth_method = v["auth_method"].as_str().filter(|s| !s.is_empty()).map(String::from);
    Some(AccountIdentity {
        account_key: Some(email.clone()),
        email: Some(email),
        plan: auth_method,
        workspace: None,
    })
}

fn extract_claude_safe_meta(dot_claude_path: &Path) -> Option<serde_json::Value> {
    let raw = std::fs::read_to_string(dot_claude_path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let mut out = serde_json::Map::new();
    for k in ["oauthAccount", "hasCompletedOnboarding", "userID"] {
        if let Some(val) = v.get(k) {
            out.insert(k.to_string(), val.clone());
        }
    }
    Some(serde_json::Value::Object(out))
}

/// macOS Cursor processes we start keep their login in files (`AGENT_CLI_CREDENTIAL_STORE=file`), never the Keychain
pub fn cursor_file_store() -> bool {
    cfg!(target_os = "macos")
}

/// Whether account swaps read and write the OS keyring for `tool` (not Cursor on macOS, see `cursor_file_store`)
pub fn keyring_managed(tool: ToolKind) -> bool {
    !(tool == ToolKind::Cursor && cursor_file_store())
}

fn native_cursor_extra_auth(home: &Path) -> Option<PathBuf> {
    if cfg!(windows) {
        std::env::var("APPDATA")
            .ok()
            .map(|a| PathBuf::from(a).join("Cursor").join("auth.json"))
    } else if !cfg!(target_os = "macos") {
        std::env::var("XDG_CONFIG_HOME")
            .ok()
            .map(|x| PathBuf::from(x).join("cursor").join("auth.json"))
            .or_else(|| Some(home.join(".config").join("cursor").join("auth.json")))
    } else {
        None
    }
}

/// The provider a Codex config.toml selects (root `model_provider`, or the active `profile`'s), unless it's the built-in OpenAI one
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CodexProvider {
    pub id: String,
    pub base_url: Option<String>,
    pub env_key: Option<String>,
    pub requires_openai_auth: bool,
    pub bearer_token: bool,
}

/// `key = value` lines of a TOML file with the table each is in. Enough for Codex's provider settings
/// (plain tables, string / bool values); arrays and inline tables come back as raw text
fn toml_entries(cfg: &str) -> Vec<(Vec<String>, String, String, usize)> {
    let mut table: Vec<String> = vec![];
    let mut out = vec![];
    for (i, line) in cfg.lines().enumerate() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        if t.starts_with("[[") {
            table = vec!["[[array]]".into()];
            continue;
        }
        if let Some(inner) = t.strip_prefix('[') {
            let inner = inner.split(']').next().unwrap_or("");
            table = toml_key_path(inner);
            continue;
        }
        let Some((k, v)) = t.split_once('=') else { continue };
        let mut path = toml_key_path(k);
        let Some(key) = path.pop() else { continue };
        let mut full = table.clone();
        full.extend(path);
        out.push((full, key, toml_value(v.trim()), i));
    }
    out
}

/// `model_providers."my api"` -> ["model_providers", "my api"]
fn toml_key_path(s: &str) -> Vec<String> {
    let mut parts = vec![];
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for c in s.trim().chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => cur.push(c),
            (None, '"' | '\'') => quote = Some(c),
            (None, '.') => parts.push(std::mem::take(&mut cur).trim().to_string()),
            (None, c) => cur.push(c),
        }
    }
    parts.push(cur.trim().to_string());
    parts.retain(|p| !p.is_empty());
    parts
}

fn toml_value(v: &str) -> String {
    if let Some(rest) = v.strip_prefix('"') {
        let mut out = String::new();
        let mut esc = false;
        for c in rest.chars() {
            match (esc, c) {
                (true, 'n') => { out.push('\n'); esc = false; }
                (true, 't') => { out.push('\t'); esc = false; }
                (true, c) => { out.push(c); esc = false; }
                (false, '\\') => esc = true,
                (false, '"') => break,
                (false, c) => out.push(c),
            }
        }
        return out;
    }
    if let Some(rest) = v.strip_prefix('\'') {
        return rest.split('\'').next().unwrap_or("").to_string();
    }
    v.split('#').next().unwrap_or("").trim().to_string()
}

pub fn codex_provider(cfg: &str) -> Option<CodexProvider> {
    let entries = toml_entries(cfg);
    let get = |table: &[&str], key: &str| {
        entries.iter().find(|(t, k, _, _)| k == key && t.iter().map(String::as_str).eq(table.iter().copied())).map(|e| e.2.clone())
    };
    let profile_provider = get(&[], "profile").and_then(|p| get(&["profiles", p.as_str()], "model_provider"));
    let id = profile_provider.or_else(|| get(&[], "model_provider")).filter(|id| !id.is_empty() && id != "openai")?;
    let sec = ["model_providers", id.as_str()];
    let nonempty = |v: Option<String>| v.filter(|s| !s.trim().is_empty());
    Some(CodexProvider {
        base_url: nonempty(get(&sec, "base_url")),
        env_key: nonempty(get(&sec, "env_key")),
        requires_openai_auth: get(&sec, "requires_openai_auth").as_deref() == Some("true"),
        bearer_token: nonempty(get(&sec, "experimental_bearer_token")).is_some(),
        id,
    })
}

fn codex_actual_auth(p: CodexProvider, key_set: bool) -> ActualAuth {
    let method = if p.env_key.is_some() || p.bearer_token {
        "providerApiKey"
    } else if p.requires_openai_auth {
        "providerOpenaiAuth"
    } else {
        "providerNoAuth"
    };
    ActualAuth {
        method: method.into(),
        key_set: key_set || p.bearer_token,
        provider: p.id,
        base_url: p.base_url,
        env_key: p.env_key,
    }
}

/// config.toml without the provider selection, so Codex uses its own login (root `model_provider`, and `profile` when that one picks the provider)
fn strip_codex_provider(cfg: &str) -> String {
    let entries = toml_entries(cfg);
    let profile = entries.iter().find(|(t, k, _, _)| t.is_empty() && k == "profile").map(|e| e.2.clone());
    let profile_picks = profile.as_deref().is_some_and(|p| {
        entries.iter().any(|(t, k, _, _)| k == "model_provider" && t.len() == 2 && t[0] == "profiles" && t[1] == p)
    });
    let drop: HashSet<usize> = entries
        .iter()
        .filter(|(t, k, _, _)| t.is_empty() && (k == "model_provider" || (k == "profile" && profile_picks)))
        .map(|e| e.3)
        .collect();
    let mut s: String = cfg.lines().enumerate().filter(|(i, _)| !drop.contains(i)).map(|(_, l)| format!("{l}\n")).collect();
    if !cfg.ends_with('\n') && s.ends_with('\n') {
        s.pop();
    }
    s
}

/// The custom provider's `base_url` in config.toml set to `url`
fn set_codex_provider_base_url(cfg: &str, provider: &str, url: &str) -> String {
    let entries = toml_entries(cfg);
    let is_sec = |t: &Vec<String>| t.len() == 2 && t[0] == "model_providers" && t[1] == provider;
    let line = format!("base_url = {url:?}");
    let mut lines: Vec<String> = cfg.lines().map(String::from).collect();
    if let Some(e) = entries.iter().find(|(t, k, _, _)| is_sec(t) && k == "base_url") {
        if e.2 == url {
            return cfg.to_string();
        }
        lines[e.3] = line;
    } else if let Some(e) = entries.iter().find(|(t, _, _, _)| is_sec(t)) {
        lines.insert(e.3, line);
    } else {
        return cfg.to_string();
    }
    let mut s = lines.join("\n");
    if cfg.ends_with('\n') {
        s.push('\n');
    }
    s
}

/// The saved account Codex is really using on the host: an API Key account holding the provider's key,
/// else one whose profile selects the same provider
fn find_codex_provider_account(mgr: &AccountManager, accounts: &[StoredAccount], actual: &ActualAuth, host_key: Option<&str>) -> Option<String> {
    let by_key = host_key.filter(|k| !k.is_empty()).and_then(|k| {
        accounts.iter().find(|a| a.auth_kind == AccountAuthKind::ApiKey && a.api_config.as_ref().is_some_and(|c| c.api_key == k))
    });
    by_key
        .or_else(|| {
            let mut same = accounts.iter().filter(|a| {
                mgr.codex_actual(&mgr.profile_home(ToolKind::Codex, &a.id), None)
                    .is_some_and(|x| x.provider == actual.provider && x.base_url == actual.base_url)
            });
            let first = same.next()?;
            // Several accounts on one provider: only an API Key account can be told apart (by its key)
            same.next().is_none().then_some(first)
        })
        .map(|a| a.id.clone())
}

/// Ensure `<codex_home>/config.toml` pins `cli_auth_credentials_store = "file"`, and for API-key
/// profiles writes `auth.json` + optional `model` without mutating user-wide `~/.codex`.
pub fn ensure_codex_profile_files(codex_home: &Path, api_cfg: Option<&ApiKeyConfig>) -> Result<()> {
    std::fs::create_dir_all(codex_home)?;
    let cfg_path = codex_home.join("config.toml");
    let existing = std::fs::read_to_string(&cfg_path).unwrap_or_default();
    let mut updated = upsert_codex_config_toml(&existing, api_cfg.map(|c| c.default_model.as_str()).filter(|s| !s.is_empty()));
    // An account on a custom provider edits that provider's URL (OPENAI_BASE_URL doesn't reach it)
    if let Some(cfg) = api_cfg.filter(|c| !c.base_url.is_empty() && !c.provider_name.is_empty()) {
        if codex_provider(&updated).is_some_and(|p| p.id == cfg.provider_name) {
            updated = set_codex_provider_base_url(&updated, &cfg.provider_name, &cfg.base_url);
        }
    }
    if updated != existing {
        std::fs::write(&cfg_path, updated)?;
        crate::util::restrict_file(&cfg_path);
    }
    if let Some(cfg) = api_cfg {
        if !cfg.api_key.is_empty() {
            let auth_path = codex_home.join("auth.json");
            let body = serde_json::json!({ "OPENAI_API_KEY": cfg.api_key });
            std::fs::write(&auth_path, serde_json::to_string_pretty(&body)?)?;
            crate::util::restrict_file(&auth_path);
        }
    }
    Ok(())
}

fn upsert_codex_config_toml(existing: &str, default_model: Option<&str>) -> String {
    let mut lines: Vec<String> = existing.lines().map(String::from).collect();
    // Root-level keys must appear before the first `[table]` header.
    let first_section = lines
        .iter()
        .position(|l| l.trim_start().starts_with('['))
        .unwrap_or(lines.len());

    let set_root_key = |lines: &mut Vec<String>, first_section: &mut usize, key: &str, val_toml: &str| {
        if let Some(idx) = lines[..*first_section]
            .iter()
            .position(|l| l.trim_start().starts_with(&format!("{key} ")) || l.trim_start().starts_with(&format!("{key}=")))
        {
            lines[idx] = format!("{key} = {val_toml}");
        } else {
            lines.insert(*first_section, format!("{key} = {val_toml}"));
            *first_section += 1;
        }
    };

    let mut sec = first_section;
    set_root_key(&mut lines, &mut sec, "cli_auth_credentials_store", "\"file\"");
    if let Some(model) = default_model {
        set_root_key(&mut lines, &mut sec, "model", &format!("{model:?}"));
    }
    let mut s = lines.join("\n");
    if !s.ends_with('\n') {
        s.push('\n');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dirs() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("yi");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        (tmp, base, home)
    }

    #[test]
    fn api_key_save_redacts_secrets_and_defaults_no_failover() {
        let (_t, base, home) = temp_dirs();
        let mgr = AccountManager::with_dirs(base, home);
        let view = mgr
            .save(SaveAccountRequest {
                tool: ToolKind::Claude,
                label: Some("OpenRouter Proxy".into()),
                auth_kind: Some(AccountAuthKind::ApiKey),
                api_config: Some(ApiKeyConfig {
                    auth_token: "sk-or-secret-123".into(),
                    base_url: "https://openrouter.ai/api".into(),
                    default_model: "claude-sonnet".into(),
                    ..Default::default()
                }),
                ..Default::default()
            })
            .unwrap();

        assert_eq!(view.auth_kind, AccountAuthKind::ApiKey);
        assert_eq!(view.isolation, AccountIsolation::Isolated);
        assert!(!view.allow_auto_failover);
        let cfg = view.api_config.unwrap();
        assert_eq!(cfg.auth_token, REDACTED);
        assert_eq!(cfg.base_url, "https://openrouter.ai/api");

        // Updating label with redacted token preserves original secret in resolved env.
        let updated = mgr
            .save(SaveAccountRequest {
                tool: ToolKind::Claude,
                id: Some(view.id.clone()),
                label: Some("OpenRouter Primary".into()),
                allow_auto_failover: Some(true),
                api_config: Some(cfg),
                ..Default::default()
            })
            .unwrap();
        assert!(updated.allow_auto_failover);
        let resolved = mgr.resolve(ToolKind::Claude, Some(&view.id)).unwrap();
        assert_eq!(resolved.env.get("ANTHROPIC_AUTH_TOKEN").unwrap(), "sk-or-secret-123");
        assert!(resolved.env_unset.contains(&"ANTHROPIC_API_KEY".to_string()));

        let revealed = mgr.reveal_api_config(ToolKind::Claude, &view.id).unwrap();
        assert_eq!(revealed.auth_token, "sk-or-secret-123");
    }

    #[test]
    fn antigravity_api_key_resolves_isolated_with_gemini_method() {
        let (_t, base, home) = temp_dirs();
        let mgr = AccountManager::with_dirs(base, home);
        let view = mgr
            .save(SaveAccountRequest {
                tool: ToolKind::Antigravity,
                label: Some("AIza Key".into()),
                auth_kind: Some(AccountAuthKind::ApiKey),
                api_config: Some(ApiKeyConfig {
                    api_key: "AIza-test-key".into(),
                    ..Default::default()
                }),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(view.isolation, AccountIsolation::Isolated);
        let res = mgr.resolve(ToolKind::Antigravity, Some(&view.id)).unwrap();
        assert_eq!(res.auth_method.as_deref(), Some("gemini-api-key"));
        assert_eq!(res.env.get("GEMINI_API_KEY").unwrap(), "AIza-test-key");
    }

    #[test]
    fn capture_codex_and_copy_session_history_for_switch() {
        let (_t, base, home) = temp_dirs();
        let native_codex = home.join(".codex");
        std::fs::create_dir_all(native_codex.join("sessions/2026/11/15")).unwrap();

        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(r#"{"email":"a@example.com","https://api.openai.com/auth":{"chatgpt_plan_type":"plus","chatgpt_account_id":"acct_1"}}"#);
        let jwt = format!("eyJhbGciOiJub25lIn0.{payload}.sig");
        std::fs::write(
            native_codex.join("auth.json"),
            serde_json::json!({ "tokens": { "id_token": jwt, "account_id": "acct_1" } }).to_string(),
        )
        .unwrap();

        let mgr = AccountManager::with_dirs(base, home);
        let a1 = mgr.capture(ToolKind::Codex, None).unwrap();
        assert_eq!(a1.label, "a@example.com");
        assert_eq!(a1.identity.plan.as_deref(), Some("plus"));
        assert!(a1.allow_auto_failover);

        // Capture a second time creates a distinct account (per user requirement: keep as new account).
        let a2 = mgr.capture(ToolKind::Codex, None).unwrap();
        assert_ne!(a1.id, a2.id);
        assert_eq!(a2.label, "a@example.com #2");

        // Failover prefers a2 over a1 when a1 is current.
        let cand = mgr.next_failover_candidate(ToolKind::Codex, Some(&a1.id), &[]).unwrap();
        assert_eq!(cand.id, a2.id);

        // Copy session rollout from a1 to a2.
        let rollout = mgr.profile_home(ToolKind::Codex, &a1.id).join("sessions/2026/11/15/rollout-tid-1111.jsonl");
        std::fs::create_dir_all(rollout.parent().unwrap()).unwrap();
        std::fs::write(&rollout, "{\"type\":\"session_meta\"}\n").unwrap();
        assert!(mgr.copy_session_history_for_switch(ToolKind::Codex, Some(&a1.id), &a2.id, "tid-1111").unwrap());
        assert!(mgr
            .profile_home(ToolKind::Codex, &a2.id)
            .join("sessions/2026/11/15/rollout-tid-1111.jsonl")
            .exists());
    }

    #[test]
    fn host_active_activate_swaps_and_saves_refreshed_snapshot() {
        let (_t, base, home) = temp_dirs();
        let agy_dir = home.join(".gemini/antigravity-cli");
        std::fs::create_dir_all(&agy_dir).unwrap();
        std::fs::write(
            agy_dir.join("antigravity-oauth-token"),
            r#"{"token":"tok_1","id_token_email":"u1@gmail.com","auth_method":"oauth-personal"}"#,
        )
        .unwrap();

        let mgr = AccountManager::with_dirs(base.clone(), home.clone());
        let u1 = mgr.capture(ToolKind::Antigravity, None).unwrap();
        assert_eq!(u1.label, "u1@gmail.com");

        // Simulate login of u2 on host, then capture u2.
        std::fs::write(
            agy_dir.join("antigravity-oauth-token"),
            r#"{"token":"tok_2","id_token_email":"u2@gmail.com","auth_method":"oauth-personal"}"#,
        )
        .unwrap();
        let u2 = mgr.capture(ToolKind::Antigravity, None).unwrap();
        assert_eq!(mgr.active_account_id(ToolKind::Antigravity).as_deref(), Some(u2.id.as_str()));

        // Switch back to u1.
        let out = mgr.activate(ToolKind::Antigravity, &u1.id).unwrap();
        assert!(out.host_switched);
        let live = std::fs::read_to_string(agy_dir.join("antigravity-oauth-token")).unwrap();
        assert!(live.contains("u1@gmail.com"));

        // Export & import u2 as a new account.
        let bundle = mgr.export_bundle(ToolKind::Antigravity, &u2.id).unwrap();
        let u2_copy = mgr.import_bundle(bundle).unwrap();
        assert_ne!(u2_copy.id, u2.id);
    }

    #[test]
    fn qoder_accounts_are_isolated_config_dirs() {
        let (_t, base, home) = temp_dirs();
        let auth = home.join(".qoder/.auth");
        std::fs::create_dir_all(&auth).unwrap();
        std::fs::write(auth.join("user"), "enc-user-1").unwrap();
        std::fs::write(auth.join("id"), "id-1").unwrap();
        std::fs::write(auth.join("machine_id"), "machine").unwrap();
        std::fs::write(home.join(".qoder/settings.json"), "{}").unwrap();

        let mgr = AccountManager::with_dirs(base, home.clone());
        let u1 = mgr.capture(ToolKind::Qoder, None).unwrap();
        assert_eq!((u1.label.as_str(), u1.isolation), ("qoder OAuth", AccountIsolation::Isolated));
        let dir1 = mgr.profile_home(ToolKind::Qoder, &u1.id);
        assert_eq!(std::fs::read_to_string(dir1.join(".auth/user")).unwrap(), "enc-user-1");
        assert_eq!(std::fs::read_to_string(dir1.join(".auth/machine_id")).unwrap(), "machine");
        assert!(!dir1.join("settings.json").exists(), "only the login is copied");
        let res = mgr.resolve(ToolKind::Qoder, Some(&u1.id)).unwrap();
        assert_eq!(res.env.get("QODER_CONFIG_DIR").map(PathBuf::from), Some(dir1.clone()));
        assert!(res.env_unset.contains(&"QODER_PERSONAL_ACCESS_TOKEN".to_string()));

        // A second host login is a second account; activating one leaves ~/.qoder alone
        std::fs::write(auth.join("user"), "enc-user-2").unwrap();
        let u2 = mgr.capture(ToolKind::Qoder, None).unwrap();
        assert_eq!(u2.label, "qoder OAuth #2");
        assert!(!mgr.activate(ToolKind::Qoder, &u1.id).unwrap().host_switched);
        assert_eq!(std::fs::read_to_string(auth.join("user")).unwrap(), "enc-user-2");
        let st = mgr.probe_native_status(ToolKind::Qoder, &mgr.all_accounts_for_tool(ToolKind::Qoder), Some(&u1.id));
        assert!(st.logged_in && st.matched_account_id.is_none(), "the active account isn't the host login");

        // Continuing a session on another account carries its log over
        let log = dir1.join("projects/d-git-x/sess-1.jsonl");
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        std::fs::write(&log, "{}\n").unwrap();
        assert!(mgr.copy_session_history_for_switch(ToolKind::Qoder, Some(&u1.id), &u2.id, "sess-1").unwrap());
        assert!(mgr.profile_home(ToolKind::Qoder, &u2.id).join("projects/d-git-x/sess-1.jsonl").is_file());

        // Export / import carries the login files
        let copy = mgr.import_bundle(mgr.export_bundle(ToolKind::Qoder, &u1.id).unwrap()).unwrap();
        assert_eq!(std::fs::read_to_string(mgr.profile_home(ToolKind::Qoder, &copy.id).join(".auth/user")).unwrap(), "enc-user-1");

        // The quota probe reports the email: the placeholder label becomes the email, a user-chosen one stays
        mgr.update_identity_if_missing(ToolKind::Qoder, &u1.id, Some("q1@example.com"), None);
        assert_eq!(mgr.get_stored(ToolKind::Qoder, &u1.id).unwrap().label, "q1@example.com");
        mgr.save(SaveAccountRequest { tool: ToolKind::Qoder, id: Some(u2.id.clone()), label: Some("work".into()), ..Default::default() }).unwrap();
        mgr.update_identity_if_missing(ToolKind::Qoder, &u2.id, Some("q2@example.com"), None);
        let s2 = mgr.get_stored(ToolKind::Qoder, &u2.id).unwrap();
        assert_eq!((s2.label.as_str(), s2.identity.email.as_deref()), ("work", Some("q2@example.com")));

        std::fs::remove_file(auth.join("user")).unwrap();
        assert!(mgr.capture(ToolKind::Qoder, None).is_err());
    }

    #[test]
    fn qoder_api_key_uses_personal_access_token() {
        let (_t, base, home) = temp_dirs();
        let mgr = AccountManager::with_dirs(base, home);
        let v = mgr
            .save(SaveAccountRequest {
                tool: ToolKind::Qoder,
                auth_kind: Some(AccountAuthKind::ApiKey),
                api_config: Some(ApiKeyConfig { api_key: "pat-123".into(), base_url: "https://ignored".into(), ..Default::default() }),
                ..Default::default()
            })
            .unwrap();
        let res = mgr.resolve(ToolKind::Qoder, Some(&v.id)).unwrap();
        assert_eq!(res.env.get("QODER_PERSONAL_ACCESS_TOKEN").map(String::as_str), Some("pat-123"));
        // Its own empty config dir, so a host /login can't take precedence over the token
        assert_eq!(res.env.get("QODER_CONFIG_DIR").map(PathBuf::from), Some(mgr.profile_home(ToolKind::Qoder, &v.id)));
        assert!(!res.env.contains_key("QODER_API_KEY") && !res.env.contains_key("OPENAI_BASE_URL"));
    }

    #[test]
    fn kiro_identity_uses_the_whoami_email_of_that_login() {
        let (_t, base, home) = temp_dirs();
        let mgr = AccountManager::with_dirs(base, home);
        let row = |t: &str| DbRow { table: "auth_kv".into(), key: "kirocli:odic:token".into(), text: Some(t.into()), blob: None };
        let snap_a = SnapshotBundle { db_rows: vec![row("{\"access_token\":\"a\"}")], ..Default::default() };
        let snap_b = SnapshotBundle { db_rows: vec![row("{\"access_token\":\"b\"}")], ..Default::default() };
        KIRO_EMAILS.lock().unwrap().insert(kiro_token_key(&snap_a).unwrap(), "a@acme.com".into());
        let id = mgr.parse_host_identity(ToolKind::Kiro, &snap_a);
        assert_eq!((id.email.as_deref(), id.account_key.as_deref()), (Some("a@acme.com"), Some("a@acme.com")));
        assert!(mgr.parse_host_identity(ToolKind::Kiro, &snap_b).email.is_none(), "another login isn't that email");
        // Not the real home: no kiro-cli run, nothing learned
        mgr.learn_kiro_email(ToolKind::Kiro, &snap_b, None);
        assert!(kiro_cached_email(&snap_b).is_none());
    }

    fn kiro_db(mgr: &AccountManager) -> rusqlite::Connection {
        let path = mgr.kiro_db_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let c = rusqlite::Connection::open(&path).unwrap();
        c.execute_batch(
            "CREATE TABLE IF NOT EXISTS auth_kv (key TEXT PRIMARY KEY, value TEXT);
             CREATE TABLE IF NOT EXISTS state (key TEXT PRIMARY KEY, value BLOB);",
        )
        .unwrap();
        c
    }

    fn kiro_login(c: &rusqlite::Connection, token_key: &str, token: &str, start_url: Option<&str>) {
        c.execute("DELETE FROM auth_kv WHERE key LIKE 'kirocli:%'", []).unwrap();
        c.execute("DELETE FROM state WHERE key LIKE 'auth.idc.%'", []).unwrap();
        c.execute("INSERT INTO auth_kv VALUES (?1, ?2)", rusqlite::params![token_key, token]).unwrap();
        c.execute("INSERT INTO auth_kv VALUES ('kirocli:odic:device-registration', '{\"client_id\":\"c\"}')", []).unwrap();
        if let Some(u) = start_url {
            c.execute("INSERT INTO state VALUES ('auth.idc.start-url', ?1)", rusqlite::params![u.as_bytes()]).unwrap();
        }
    }

    fn kiro_value(c: &rusqlite::Connection, table: &str, key: &str) -> Option<String> {
        c.query_row(&format!("SELECT CAST(value AS TEXT) FROM {table} WHERE key = ?1"), [key], |r| r.get(0)).ok()
    }

    #[test]
    fn kiro_swaps_login_rows_in_sqlite() {
        let (_t, base, home) = temp_dirs();
        let mgr = AccountManager::with_dirs(base, home);
        let c = kiro_db(&mgr);
        c.execute("INSERT INTO state VALUES ('telemetryClientId', 'keep-me')", []).unwrap();
        // No token row: not logged in, even with leftover state
        c.execute("INSERT INTO state VALUES ('auth.idc.region', 'us-east-1')", []).unwrap();
        assert!(mgr.capture(ToolKind::Kiro, None).is_err());

        let org = "https://acme.awsapps.com/start";
        kiro_login(&c, "kirocli:odic:token", &format!("{{\"access_token\":\"t-org\",\"start_url\":\"{org}\"}}"), Some(org));
        let a = mgr.capture(ToolKind::Kiro, None).unwrap();
        assert_eq!((a.label.as_str(), a.identity.workspace.as_deref()), (org, Some(org)));

        kiro_login(&c, "kirocli:social:token", "{\"access_token\":\"t-social\"}", None);
        let b = mgr.capture(ToolKind::Kiro, None).unwrap();
        assert_eq!(b.identity.workspace.as_deref(), Some("Google / GitHub"));

        assert!(mgr.activate(ToolKind::Kiro, &a.id).unwrap().host_switched);
        assert!(kiro_value(&c, "auth_kv", "kirocli:odic:token").unwrap().contains("t-org"));
        assert_eq!(kiro_value(&c, "auth_kv", "kirocli:social:token"), None);
        assert_eq!(kiro_value(&c, "state", "auth.idc.start-url").as_deref(), Some(org));
        assert!(mgr.activate(ToolKind::Kiro, &b.id).unwrap().host_switched);
        assert!(kiro_value(&c, "auth_kv", "kirocli:social:token").unwrap().contains("t-social"));
        assert_eq!(kiro_value(&c, "auth_kv", "kirocli:odic:token"), None);
        assert_eq!(kiro_value(&c, "state", "auth.idc.start-url"), None);
        // Signing out before an in-app login only drops the login rows
        mgr.clear_host_live(ToolKind::Kiro).unwrap();
        assert_eq!(kiro_value(&c, "auth_kv", "kirocli:odic:device-registration"), None);
        assert_eq!(kiro_value(&c, "state", "telemetryClientId").as_deref(), Some("keep-me"));

        // The probe's email replaces the workspace label
        mgr.update_identity_if_missing(ToolKind::Kiro, &a.id, Some("dev@acme.com"), None);
        assert_eq!(mgr.get_stored(ToolKind::Kiro, &a.id).unwrap().label, "dev@acme.com");
    }

    #[test]
    fn codebuddy_oauth_swaps_extension_auth_file() {
        let (_t, base, home) = temp_dirs();
        let file = home.join(CODEBUDDY_AUTH_FILE);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let login = |uid: &str, nick: &str| {
            serde_json::json!({"account": {"uid": uid, "nickname": nick}, "auth": {"accessToken": format!("tok-{uid}"), "domain": "www.codebuddy.cn"}, "accounts": []}).to_string()
        };
        std::fs::write(&file, login("u1", "alice")).unwrap();
        let mgr = AccountManager::with_dirs(base, home);
        let a = mgr.capture(ToolKind::Codebuddy, None).unwrap();
        assert_eq!((a.label.as_str(), a.identity.account_key.as_deref(), a.identity.workspace.as_deref()), ("alice", Some("u1"), Some("www.codebuddy.cn")));

        std::fs::write(&file, login("u2", "bob")).unwrap();
        let b = mgr.capture(ToolKind::Codebuddy, None).unwrap();
        let st = mgr.probe_native_status(ToolKind::Codebuddy, &mgr.all_accounts_for_tool(ToolKind::Codebuddy), Some(&b.id));
        assert_eq!((st.logged_in, st.matched_account_id.as_deref()), (true, Some(b.id.as_str())));

        assert!(mgr.activate(ToolKind::Codebuddy, &a.id).unwrap().host_switched);
        assert!(std::fs::read_to_string(&file).unwrap().contains("tok-u1"));
        // b's snapshot kept what was live when it was swapped out
        assert!(mgr.activate(ToolKind::Codebuddy, &b.id).unwrap().host_switched);
        assert!(std::fs::read_to_string(&file).unwrap().contains("tok-u2"));
        assert!(!AccountManager::capability(ToolKind::Codebuddy).supports_interactive_login);
    }

    #[test]
    fn codebuddy_api_key_uses_its_own_variables() {
        let (_t, base, home) = temp_dirs();
        let mgr = AccountManager::with_dirs(base, home);
        let v = mgr
            .save(SaveAccountRequest {
                tool: ToolKind::Codebuddy,
                auth_kind: Some(AccountAuthKind::ApiKey),
                api_config: Some(ApiKeyConfig { api_key: "ck".into(), base_url: "https://cb.example".into(), default_model: "glm-4.7".into(), ..Default::default() }),
                ..Default::default()
            })
            .unwrap();
        let env = mgr.resolve(ToolKind::Codebuddy, Some(&v.id)).unwrap().env;
        assert_eq!(env.get("CODEBUDDY_API_KEY").map(String::as_str), Some("ck"));
        assert_eq!(env.get("CODEBUDDY_BASE_URL").map(String::as_str), Some("https://cb.example"));
        assert_eq!(env.get("CODEBUDDY_MODEL").map(String::as_str), Some("glm-4.7"));
        assert!(!env.contains_key("OPENAI_BASE_URL") && !env.contains_key("AUTH_TOKEN"));
    }

    #[test]
    fn codex_config_toml_upsert_places_root_keys_before_sections() {
        let src = "model_reasoning_effort = \"high\"\n\n[projects.\"/tmp\"]\ntrust_level = \"trusted\"\n";
        let out = upsert_codex_config_toml(src, Some("gpt-5.4"));
        let store_pos = out.find("cli_auth_credentials_store = \"file\"").unwrap();
        let sec_pos = out.find("[projects.").unwrap();
        assert!(store_pos < sec_pos);
        assert!(out.contains("model = \"gpt-5.4\""));
    }

    #[test]
    fn cursor_env_api_key_takes_precedence_over_oauth_and_protects_snapshot() {
        let (_t, base, home) = temp_dirs();
        let cursor_dir = home.join(".config/cursor");
        let dot_cursor = home.join(".cursor");
        std::fs::create_dir_all(&cursor_dir).unwrap();
        std::fs::create_dir_all(&dot_cursor).unwrap();

        let oauth_payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(r#"{"sub":"auth0|user_oauth_1","type":"session"}"#);
        let oauth_jwt = format!("eyJhbGciOiJub25lIn0.{oauth_payload}.sig");
        let oauth_json = serde_json::json!({
            "accessToken": oauth_jwt,
            "refreshToken": oauth_jwt,
        })
        .to_string();
        std::fs::write(cursor_dir.join("auth.json"), &oauth_json).unwrap();
        std::fs::write(
            dot_cursor.join("cli-config.json"),
            r#"{"authInfo":{"email":"oauth@example.com","authId":"auth0|user_oauth_1","teamName":"Acme","activeTeamId":42},"editor":{"vimMode":true}}"#,
        )
        .unwrap();

        let mgr = AccountManager::with_dirs(base, home);
        let oauth_acc = mgr
            .capture_with_cursor_env(ToolKind::Cursor, None, None, None)
            .unwrap();
        assert_eq!(oauth_acc.auth_kind, AccountAuthKind::Oauth);
        assert_eq!(oauth_acc.label, "oauth@example.com");
        assert_eq!(oauth_acc.identity.account_key.as_deref(), Some("auth0|user_oauth_1"));
        assert_eq!(oauth_acc.identity.workspace.as_deref(), Some("Acme"));

        // When CURSOR_API_KEY is set, native status prioritizes env_api_key over OAuth while reporting has_local_oauth.
        let stored_before = mgr.all_accounts_for_tool(ToolKind::Cursor);
        let st_unmatched = mgr.probe_native_status_with_cursor_env(
            ToolKind::Cursor,
            &stored_before,
            Some(&oauth_acc.id),
            Some("key_env_123"),
        );
        assert!(st_unmatched.logged_in);
        assert_eq!(st_unmatched.auth_method.as_deref(), Some("env_api_key"));
        assert_eq!(st_unmatched.matched_account_id, None);
        assert_eq!(st_unmatched.env_api_key_matched_id, None);
        assert!(st_unmatched.has_local_oauth);
        assert_eq!(st_unmatched.env_override.as_deref(), Some("CURSOR_API_KEY"));

        // Capture with source="oauth" still captures local OAuth even when CURSOR_API_KEY is set!
        let oauth_acc_2 = mgr
            .capture_with_cursor_env(ToolKind::Cursor, None, Some("oauth"), Some("key_env_123"))
            .unwrap();
        assert_eq!(oauth_acc_2.auth_kind, AccountAuthKind::Oauth);
        assert_eq!(oauth_acc_2.label, "oauth@example.com #2");

        // Capture while CURSOR_API_KEY is set creates an isolated ApiKey profile with memory credential store.
        let key_acc = mgr
            .save(SaveAccountRequest {
                tool: ToolKind::Cursor,
                auth_kind: Some(AccountAuthKind::ApiKey),
                api_config: Some(ApiKeyConfig {
                    api_key: "key_env_123".into(),
                    base_url: "https://api2.cursor.example".into(),
                    ..Default::default()
                }),
                verified_identity: Some(AccountIdentity {
                    email: Some("oauth@example.com".into()),
                    plan: Some("Team".into()),
                    account_key: Some("oauth@example.com".into()),
                    workspace: None,
                }),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(key_acc.auth_kind, AccountAuthKind::ApiKey);
        assert_eq!(key_acc.isolation, AccountIsolation::Isolated);
        assert_eq!(key_acc.label, "oauth@example.com (API Key)");
        let resolved_key = mgr.resolve(ToolKind::Cursor, Some(&key_acc.id)).unwrap();
        assert_eq!(resolved_key.env.get("CURSOR_API_KEY").unwrap(), "key_env_123");
        assert_eq!(
            resolved_key.env.get("AGENT_CLI_CREDENTIAL_STORE").unwrap(),
            "memory"
        );
        assert_eq!(
            resolved_key.env.get("CURSOR_API_ENDPOINT").unwrap(),
            "https://api2.cursor.example"
        );
        let resolved_oauth = mgr.resolve(ToolKind::Cursor, Some(&oauth_acc.id)).unwrap();
        assert!(resolved_oauth.env_unset.contains(&"CURSOR_API_KEY".to_string()));
        assert!(resolved_oauth.env_unset.contains(&"CURSOR_AUTH_TOKEN".to_string()));
        assert!(resolved_oauth
            .env_unset
            .contains(&"AGENT_CLI_CREDENTIAL_STORE".to_string()));

        // Native status now matches the ApiKey profile, not the OAuth profile.
        let stored_after = mgr.all_accounts_for_tool(ToolKind::Cursor);
        let st_matched = mgr.probe_native_status_with_cursor_env(
            ToolKind::Cursor,
            &stored_after,
            Some(&oauth_acc.id),
            Some("key_env_123"),
        );
        assert_eq!(st_matched.matched_account_id.as_deref(), Some(key_acc.id.as_str()));
        assert_eq!(
            st_matched.env_api_key_matched_id.as_deref(),
            Some(key_acc.id.as_str())
        );

        // If an external CURSOR_API_KEY run pollutes auth.json (including JWT type=api_key_token), ensure_cursor_oauth_host_clean recovers OAuth.
        let key_payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(r#"{"sub":"auth0|other_user","type":"api_key_token","apiKeyId":"999"}"#);
        let key_jwt = format!("eyJhbGciOiJub25lIn0.{key_payload}.sig");
        std::fs::write(
            cursor_dir.join("auth.json"),
            serde_json::json!({ "accessToken": key_jwt, "refreshToken": key_jwt }).to_string(),
        )
        .unwrap();
        assert!(mgr
            .capture_with_cursor_env(ToolKind::Cursor, None, Some("oauth"), None)
            .is_err());
        mgr.ensure_cursor_oauth_host_clean(&oauth_acc.id).unwrap();
        let restored = std::fs::read_to_string(cursor_dir.join("auth.json")).unwrap();
        assert!(restored.contains(&oauth_jwt));
        assert!(!restored.contains(&key_jwt));
    }

    #[test]
    fn antigravity_parse_host_identity_handles_gemini_scoped_keyring() {
        let (_t, base, home) = temp_dirs();
        let mgr = AccountManager::with_dirs(base, home);
        let payload = serde_json::json!({
            "id_token_email": "alice@example.com",
            "auth_method": "consumer"
        })
        .to_string();

        // macOS / Linux layout: service="gemini", account="<appDataDir>/antigravity-oauth-token"
        let unix_snap = SnapshotBundle {
            keyring: vec![KeyringEntry::from_bytes(
                "gemini".into(),
                "/home/alice/.gemini/antigravity-cli/antigravity-oauth-token".into(),
                None,
                payload.as_bytes(),
            )],
            ..Default::default()
        };
        let unix_id = mgr.parse_host_identity(ToolKind::Antigravity, &unix_snap);
        assert_eq!(unix_id.email.as_deref(), Some("alice@example.com"));
        assert_eq!(unix_id.plan.as_deref(), Some("consumer"));

        // Windows go-keyring layout: TargetName="gemini:<path>\\antigravity-oauth-token"
        let win_snap = SnapshotBundle {
            keyring: vec![KeyringEntry::from_bytes(
                r"gemini:C:\Users\alice\.gemini\antigravity-cli\antigravity-oauth-token".into(),
                r"C:\Users\alice\.gemini\antigravity-cli\antigravity-oauth-token".into(),
                None,
                payload.as_bytes(),
            )],
            ..Default::default()
        };
        let win_id = mgr.parse_host_identity(ToolKind::Antigravity, &win_snap);
        assert_eq!(win_id.email.as_deref(), Some("alice@example.com"));

        // Current agy on Windows: TargetName="gemini:antigravity", UserName="antigravity"
        let win2_snap = SnapshotBundle {
            keyring: vec![KeyringEntry::from_bytes("gemini:antigravity".into(), "antigravity".into(), None, payload.as_bytes())],
            ..Default::default()
        };
        assert_eq!(mgr.parse_host_identity(ToolKind::Antigravity, &win2_snap).email.as_deref(), Some("alice@example.com"));
    }

    const GATEWAY_CFG: &str = "model = \"gpt-6.1-sol\"\nmodel_provider = \"myapi\"\n[projects.'d:\\git\\x']\ntrust_level = \"trusted\"\n\n[model_providers.myapi]\nname = \"myapi\"\nbase_url = \"https://gw.example/api/v1\"\nenv_key = \"API_GATEWAY_KEY\" # the gateway's key\nwire_api = \"responses\"\n";

    fn write_codex_chatgpt_login(dir: &Path) {
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(r#"{"email":"a@example.com","https://api.openai.com/auth":{"chatgpt_plan_type":"plus","chatgpt_account_id":"acct_1"}}"#);
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("auth.json"),
            serde_json::json!({ "auth_mode": "chatgpt", "OPENAI_API_KEY": null, "tokens": { "id_token": format!("eyJhbGciOiJub25lIn0.{payload}.sig"), "account_id": "acct_1" } }).to_string(),
        )
        .unwrap();
    }

    #[test]
    fn codex_provider_is_read_from_config_toml() {
        let p = codex_provider(GATEWAY_CFG).unwrap();
        assert_eq!(p, CodexProvider { id: "myapi".into(), base_url: Some("https://gw.example/api/v1".into()), env_key: Some("API_GATEWAY_KEY".into()), requires_openai_auth: false, bearer_token: false });
        assert!(codex_provider("model = \"x\"\n").is_none());
        assert!(codex_provider("model_provider = \"openai\"\n").is_none());
        // The active profile's provider wins; quoted table names
        let prof = "profile = \"work\"\nmodel_provider = \"openai\"\n[profiles.work]\nmodel_provider = 'az ure'\n[model_providers.\"az ure\"]\nrequires_openai_auth = true\n";
        let p = codex_provider(prof).unwrap();
        assert_eq!((p.id.as_str(), p.requires_openai_auth, p.env_key), ("az ure", true, None));
        // Keys inside other tables don't count as the root selection
        assert!(codex_provider("[profiles.x]\nmodel_provider = \"myapi\"\n").is_none());
        let stripped = strip_codex_provider(GATEWAY_CFG);
        assert!(codex_provider(&stripped).is_none() && stripped.contains("[model_providers.myapi]") && stripped.contains("model = \"gpt-6.1-sol\""));
        assert!(codex_provider(&strip_codex_provider(prof)).is_none());
        let moved = set_codex_provider_base_url(GATEWAY_CFG, "myapi", "https://gw2.example/v1");
        assert_eq!(codex_provider(&moved).unwrap().base_url.as_deref(), Some("https://gw2.example/v1"));
    }

    #[test]
    fn codex_native_status_shows_the_provider_in_use() {
        let (_t, base, home) = temp_dirs();
        let native = home.join(".codex");
        write_codex_chatgpt_login(&native);
        std::fs::write(native.join("config.toml"), GATEWAY_CFG).unwrap();
        let mgr = AccountManager::with_dirs(base, home);
        mgr.set_test_env(&[("API_GATEWAY_KEY", "gw-secret")]);
        let st = mgr.probe_native_status(ToolKind::Codex, &[], None);
        assert!(st.logged_in);
        assert_eq!(st.auth_method.as_deref(), Some("provider_api_key"));
        assert_eq!(st.email, None, "the ChatGPT login isn't what Codex uses");
        assert!(st.has_local_oauth);
        let a = st.actual.unwrap();
        assert_eq!((a.method.as_str(), a.provider.as_str(), a.env_key.as_deref(), a.key_set), ("providerApiKey", "myapi", Some("API_GATEWAY_KEY"), true));
        mgr.set_test_env(&[]);
        let st = mgr.probe_native_status(ToolKind::Codex, &[], None);
        assert!(!st.logged_in && !st.actual.unwrap().key_set);
        // Without a provider: the ChatGPT login as before
        std::fs::write(native_codex_cfg(&mgr), "model = \"x\"\n").unwrap();
        let st = mgr.probe_native_status(ToolKind::Codex, &[], None);
        assert_eq!((st.auth_method.as_deref(), st.email.as_deref(), st.actual), (Some("chatgpt"), Some("a@example.com"), None));
    }

    fn native_codex_cfg(mgr: &AccountManager) -> PathBuf {
        mgr.native_codex_home().join("config.toml")
    }

    #[test]
    fn codex_capture_follows_the_provider_and_can_take_the_login_instead() {
        let (_t, base, home) = temp_dirs();
        let native = home.join(".codex");
        write_codex_chatgpt_login(&native);
        std::fs::write(native.join("config.toml"), GATEWAY_CFG).unwrap();
        let mgr = AccountManager::with_dirs(base, home);
        mgr.set_test_env(&[("API_GATEWAY_KEY", "gw-secret")]);
        let key = mgr.capture(ToolKind::Codex, None).unwrap();
        assert_eq!(key.auth_kind, AccountAuthKind::ApiKey);
        assert_eq!(key.label, "codex (myapi)");
        assert_eq!(key.actual.as_ref().unwrap().provider, "myapi");
        let res = mgr.resolve(ToolKind::Codex, Some(&key.id)).unwrap();
        assert_eq!(res.env.get("API_GATEWAY_KEY").map(String::as_str), Some("gw-secret"));
        assert!(!res.env.contains_key("OPENAI_BASE_URL") && res.env_unset.contains(&"OPENAI_BASE_URL".to_string()));
        let stored: Vec<StoredAccount> = mgr.data.lock().unwrap().tools["codex"].accounts.clone();
        let st = mgr.probe_native_status(ToolKind::Codex, &stored, None);
        assert_eq!(st.matched_account_id.as_deref(), Some(key.id.as_str()));

        let login = mgr.capture_with_source(ToolKind::Codex, None, Some("oauth")).unwrap();
        assert_eq!((login.auth_kind, login.identity.email.as_deref(), login.actual.clone()), (AccountAuthKind::Oauth, Some("a@example.com"), None));
        let cfg = std::fs::read_to_string(mgr.profile_home(ToolKind::Codex, &login.id).join("config.toml")).unwrap();
        assert!(codex_provider(&cfg).is_none() && cfg.contains("gpt-6.1-sol"));
        mgr.set_test_env(&[]);
        assert!(mgr.capture_with_source(ToolKind::Codex, None, Some("apiKey")).unwrap_err().to_string().contains("API_GATEWAY_KEY"));
    }

    #[test]
    fn codex_oauth_account_on_a_provider_converts_both_ways() {
        let (_t, base, home) = temp_dirs();
        let native = home.join(".codex");
        write_codex_chatgpt_login(&native);
        std::fs::write(native.join("config.toml"), "model = \"x\"\n").unwrap();
        let mgr = AccountManager::with_dirs(base, home);
        mgr.set_test_env(&[("API_GATEWAY_KEY", "gw-secret")]);
        // Captured before: an OAuth account whose copied config.toml routes to the gateway
        let acc = mgr.capture(ToolKind::Codex, None).unwrap();
        let profile = mgr.profile_home(ToolKind::Codex, &acc.id);
        std::fs::write(profile.join("config.toml"), GATEWAY_CFG).unwrap();
        let view = mgr.to_view(&mgr.get_stored(ToolKind::Codex, &acc.id).unwrap(), None, 0, None);
        assert_eq!((view.auth_kind, view.actual.as_ref().unwrap().method.as_str()), (AccountAuthKind::Oauth, "providerApiKey"));
        let res = mgr.resolve(ToolKind::Codex, Some(&acc.id)).unwrap();
        assert!(res.env.contains_key("API_GATEWAY_KEY") || std::env::var_os("API_GATEWAY_KEY").is_some());

        let key = mgr.convert_codex(&acc.id, AccountAuthKind::ApiKey).unwrap();
        assert_eq!((key.auth_kind, key.label.as_str(), key.identity.email.clone()), (AccountAuthKind::ApiKey, "codex (myapi)", None));
        assert_eq!(key.api_config.as_ref().unwrap().base_url, "https://gw.example/api/v1");
        assert!(profile.join("auth.json.oauth-bak").is_file());
        assert_eq!(mgr.resolve(ToolKind::Codex, Some(&acc.id)).unwrap().env.get("API_GATEWAY_KEY").map(String::as_str), Some("gw-secret"));
        assert!(mgr.convert_codex(&acc.id, AccountAuthKind::ApiKey).is_err());

        let back = mgr.convert_codex(&acc.id, AccountAuthKind::Oauth).unwrap();
        assert_eq!((back.auth_kind, back.identity.email.as_deref(), back.actual), (AccountAuthKind::Oauth, Some("a@example.com"), None));
        let auth = std::fs::read_to_string(profile.join("auth.json")).unwrap();
        assert!(auth.contains("acct_1") && !profile.join("auth.json.oauth-bak").exists());
        assert!(mgr.convert_codex(&acc.id, AccountAuthKind::Oauth).is_err());
    }
}
