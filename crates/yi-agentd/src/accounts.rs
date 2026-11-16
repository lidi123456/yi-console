//! Single-tool multi-account store, credential snapshots, env resolution, and failover selection.
//!
//! Credentials live strictly host-side under `<YI_CONSOLE_HOME>/accounts.json` (`0o600`) and
//! `<YI_CONSOLE_HOME>/accounts/<tool>/<id>/`.
//! - Isolated accounts (`Codex`, `Claude`, and any tool's `ApiKey` profile) run with per-profile
//!   env vars / config dirs so multiple sessions run concurrently with different accounts.
//! - `HostActive` accounts (`Antigravity` OAuth, `Cursor`/`Kiro`/`Qoder`/`Codebuddy` OAuth)
//!   capture and atomically swap host credential files + OS keyring snapshots with rollback.

use crate::keyring::{self, KeyringEntry};
use anyhow::{anyhow, bail, Result};
use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use yi_proto::{
    AccountAuthKind, AccountIdentity, AccountIsolation, AccountProfileView, AccountState,
    ApiKeyConfig, ToolAccountsSummary, ToolKind, ToolNativeStatus,
};

const REDACTED: &str = "********";
const FILE_VERSION: u32 = 1;

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
}

impl SnapshotBundle {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty() && self.keyring.is_empty() && self.profile_files.is_empty()
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
        }
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
            ToolKind::Cursor | ToolKind::Kiro | ToolKind::Qoder | ToolKind::Codebuddy => {
                ToolCapabilities {
                    oauth_isolation: AccountIsolation::HostActive,
                    supports_oauth: true,
                    supports_api_key: true,
                    supports_device_auth: false,
                    supports_interactive_login: true,
                }
            }
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
        let bytes = snap
            .files
            .iter()
            .find(|(k, _)| k.ends_with("auth.json"))
            .and_then(|(_, b64)| base64::engine::general_purpose::STANDARD.decode(b64).ok())?;
        if cursor_auth_api_key(&bytes).is_some() {
            return None;
        }
        Some(bytes)
    }

    /// Restore a Cursor OAuth profile's host `auth.json` if a previous `CURSOR_API_KEY` process overwrote it.
    pub fn restore_cursor_oauth_if_api_key_polluted(&self, account_id: &str) {
        let _guard = self.host_lock.lock().unwrap();
        let Ok(cur) = self.take_host_snapshot_unlocked(ToolKind::Cursor) else {
            return;
        };
        if cursor_snapshot_api_key(&cur).is_none() {
            return;
        }
        let Some(clean) = self.load_snapshot(ToolKind::Cursor, account_id) else {
            return;
        };
        if cursor_snapshot_api_key(&clean).is_some() {
            return;
        }
        let _ = self.restore_host_snapshot_unlocked(ToolKind::Cursor, &clean);
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
        let (state, state_detail) = self.runtime_state(acc.tool, &acc.id);
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
                data_dir = Some(home);
            }
            (ToolKind::Codex, AccountAuthKind::ApiKey) => {
                let home = self.profile_home(ToolKind::Codex, &acc.id);
                env.insert("CODEX_HOME".into(), home.display().to_string());
                if let Some(cfg) = &acc.api_config {
                    let _ = ensure_codex_profile_files(&home, Some(cfg));
                    if !cfg.api_key.is_empty() {
                        env.insert("OPENAI_API_KEY".into(), cfg.api_key.clone());
                    }
                    if !cfg.base_url.is_empty() {
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
                env_unset.push("CURSOR_API_KEY".into());
                env_unset.push("CURSOR_AUTH_TOKEN".into());
            }
            (ToolKind::Cursor, AccountAuthKind::ApiKey) => {
                if let Some(cfg) = &acc.api_config {
                    if !cfg.api_key.is_empty() {
                        env.insert("CURSOR_API_KEY".into(), cfg.api_key.clone());
                    }
                    if !cfg.base_url.is_empty() {
                        env.insert("OPENAI_BASE_URL".into(), cfg.base_url.clone());
                    }
                    for (k, v) in &cfg.extra_env {
                        if !k.is_empty() {
                            env.insert(k.clone(), v.clone());
                        }
                    }
                }
            }
            (t, AccountAuthKind::ApiKey) => {
                let key_var = match t {
                    ToolKind::Kiro => "KIRO_API_KEY",
                    ToolKind::Qoder => "QODER_API_KEY",
                    ToolKind::Codebuddy => "CODEBUDDY_API_KEY",
                    _ => "API_KEY",
                };
                if let Some(cfg) = &acc.api_config {
                    if !cfg.api_key.is_empty() {
                        env.insert(key_var.into(), cfg.api_key.clone());
                    }
                    if !cfg.auth_token.is_empty() {
                        env.insert("AUTH_TOKEN".into(), cfg.auth_token.clone());
                    }
                    if !cfg.base_url.is_empty() {
                        env.insert("OPENAI_BASE_URL".into(), cfg.base_url.clone());
                    }
                    for (k, v) in &cfg.extra_env {
                        if !k.is_empty() {
                            env.insert(k.clone(), v.clone());
                        }
                    }
                }
            }
            (_, AccountAuthKind::Oauth) => {}
        }

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
    pub fn capture(&self, tool: ToolKind, label: Option<&str>) -> Result<AccountProfileView> {
        let cursor_env_key = if tool == ToolKind::Cursor {
            std::env::var("CURSOR_API_KEY")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        } else {
            None
        };
        self.capture_with_cursor_env(tool, label, cursor_env_key.as_deref())
    }

    fn capture_with_cursor_env(
        &self,
        tool: ToolKind,
        label: Option<&str>,
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
                if let Ok(cfg_raw) = std::fs::read_to_string(src.join("config.toml")) {
                    std::fs::write(dst.join("config.toml"), cfg_raw)?;
                }
                ensure_codex_profile_files(&dst, None)?;
                let (ak, ident, key_opt) = parse_codex_auth_bytes(&bytes);
                let api_cfg = key_opt.map(|k| ApiKeyConfig {
                    api_key: k,
                    ..Default::default()
                });
                (ak, ident, api_cfg, None, false)
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
            ToolKind::Cursor => {
                let snap = self.take_host_snapshot_unlocked(ToolKind::Cursor).unwrap_or_default();
                let file_api_key = cursor_snapshot_api_key(&snap);
                let env_key = cursor_env_key
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(String::from);
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
                } else {
                    if snap.is_empty() {
                        bail!("当前系统 cursor 未检测到已保存的 OAuth 登录凭据");
                    }
                    let ident = self.parse_host_identity(ToolKind::Cursor, &snap);
                    (AccountAuthKind::Oauth, ident, None, Some(snap), true)
                }
            }
            _ => {
                let snap = self.take_host_snapshot_unlocked(tool)?;
                if snap.is_empty() {
                    bail!("当前系统 {} 未检测到已保存的 OAuth 登录凭据", tool.as_str());
                }
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
            let mut ident = AccountIdentity::default();
            if !merged.provider_name.is_empty() {
                ident.plan = Some(merged.provider_name.clone());
            } else if !merged.base_url.is_empty() {
                ident.workspace = Some(merged.base_url.clone());
            }
            let lbl = req
                .label
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .unwrap_or_else(|| {
                    if !merged.provider_name.is_empty() {
                        format!("{} ({})", req.tool.as_str(), merged.provider_name)
                    } else {
                        format!("{} API Key", req.tool.as_str())
                    }
                });
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
            let backup = self.take_host_snapshot_unlocked(tool)?;

            // Save refreshed tokens from current host active account back into its snapshot before swapping out.
            if let Some(prev_id) = prev_active_id.as_deref() {
                if prev_id != account_id && !backup.is_empty() {
                    if let Some(prev_acc) = self.get_stored(tool, prev_id) {
                        if Self::effective_isolation(tool, prev_acc.auth_kind) == AccountIsolation::HostActive {
                            let cursor_polluted = tool == ToolKind::Cursor && cursor_snapshot_api_key(&backup).is_some();
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
            ToolKind::Claude => {
                let src_dir = match from_id {
                    Some(fid) => self.profile_home(ToolKind::Claude, fid),
                    None => self.native_claude_dir(),
                };
                let dst_dir = self.profile_home(ToolKind::Claude, to_id);
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
        if !snap.files.is_empty() || !snap.keyring.is_empty() {
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

    pub fn take_host_snapshot(&self, tool: ToolKind) -> Result<SnapshotBundle> {
        let _guard = self.host_lock.lock().unwrap();
        self.take_host_snapshot_unlocked(tool)
    }

    pub fn restore_host_snapshot(&self, tool: ToolKind, snap: &SnapshotBundle) -> Result<()> {
        let _guard = self.host_lock.lock().unwrap();
        self.restore_host_snapshot_unlocked(tool, snap)
    }

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
        let keyring = keyring::snapshot_tool(tool).unwrap_or_default();
        Ok(SnapshotBundle {
            files,
            keyring,
            profile_files: BTreeMap::new(),
        })
    }

    fn restore_host_snapshot_unlocked(&self, tool: ToolKind, snap: &SnapshotBundle) -> Result<()> {
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
        keyring::restore_tool(tool, &snap.keyring)?;
        Ok(())
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
                for rel in [".kiro/auth.json", ".kiro/credentials.json"] {
                    add(&mut res, rel);
                }
            }
            ToolKind::Qoder => {
                for rel in [".qoder/auth.json", ".qoder/.credentials.json", ".qoder/credentials.json"] {
                    add(&mut res, rel);
                }
            }
            ToolKind::Codebuddy => {
                for rel in [".codebuddy/auth.json", ".codebuddy/.credentials.json", ".codebuddy/credentials.json"] {
                    add(&mut res, rel);
                }
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
                    if e.service.contains("antigravity-oauth-token") {
                        if let Some(id) = parse_antigravity_token_bytes(e.secret.as_bytes()) {
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
                if let Some(bytes) = decode_file("auth.json") {
                    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                        let email = v["email"]
                            .as_str()
                            .or_else(|| v["user"]["email"].as_str())
                            .map(String::from)
                            .or_else(|| {
                                v["accessToken"]
                                    .as_str()
                                    .and_then(decode_jwt_claims)
                                    .and_then(|c| c["email"].as_str().map(String::from))
                            });
                        return AccountIdentity {
                            account_key: email.clone(),
                            email,
                            ..Default::default()
                        };
                    }
                }
                AccountIdentity::default()
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
            std::env::var("CURSOR_API_KEY")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
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
            ToolKind::Cursor => &["CURSOR_API_KEY"],
            _ => &[],
        };
        let mut present_envs: Vec<&str> = env_vars
            .iter()
            .copied()
            .filter(|k| std::env::var_os(k).is_some_and(|v| !v.is_empty()))
            .collect();
        if tool == ToolKind::Cursor
            && cursor_env_key.is_some_and(|s| !s.trim().is_empty())
            && !present_envs.contains(&"CURSOR_API_KEY")
        {
            present_envs.push("CURSOR_API_KEY");
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
                if let Some(bytes) = bytes {
                    let (ak, ident, key_opt) = parse_codex_auth_bytes(&bytes);
                    let matched = find_matching_account(accounts, &ident, ak);
                    ToolNativeStatus {
                        logged_in: true,
                        auth_method: Some(match ak {
                            AccountAuthKind::Oauth => "chatgpt".into(),
                            AccountAuthKind::ApiKey => "api_key".into(),
                        }),
                        email: ident.email,
                        plan: ident.plan,
                        env_override,
                        matched_account_id: matched,
                        detail: key_opt.map(|_| "使用 OPENAI_API_KEY 登录".to_string()),
                    }
                } else {
                    let has_env_key = std::env::var_os("OPENAI_API_KEY").is_some_and(|v| !v.is_empty());
                    ToolNativeStatus {
                        logged_in: has_env_key,
                        auth_method: has_env_key.then(|| "env_api_key".into()),
                        email: None,
                        plan: None,
                        env_override,
                        matched_account_id: None,
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
                        auth_method: Some("oauth".into()),
                        email: ident.email,
                        plan: ident.plan,
                        env_override,
                        matched_account_id: matched,
                        detail: None,
                    }
                } else {
                    let has_env = std::env::var_os("ANTHROPIC_API_KEY").is_some_and(|v| !v.is_empty())
                        || std::env::var_os("ANTHROPIC_AUTH_TOKEN").is_some_and(|v| !v.is_empty());
                    ToolNativeStatus {
                        logged_in: has_env,
                        auth_method: has_env.then(|| "env_api_key".into()),
                        email: None,
                        plan: None,
                        env_override,
                        matched_account_id: None,
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
                    let detail = if !snap.is_empty() && file_api_key.is_none() {
                        Some("宿主环境变量 CURSOR_API_KEY 优先级高于本地 OAuth 登录；托管 OAuth 账号会自动隔离该变量".into())
                    } else {
                        Some("使用宿主环境变量 CURSOR_API_KEY".into())
                    };
                    ToolNativeStatus {
                        logged_in: true,
                        auth_method: Some("env_api_key".into()),
                        email,
                        plan,
                        env_override,
                        matched_account_id: matched,
                        detail,
                    }
                } else if let Some(key) = file_api_key {
                    let ident = self.parse_host_identity(ToolKind::Cursor, &snap);
                    let matched = find_matching_api_key(accounts, &key)
                        .or_else(|| find_matching_account(accounts, &ident, AccountAuthKind::ApiKey));
                    ToolNativeStatus {
                        logged_in: true,
                        auth_method: Some("api_key".into()),
                        email: ident.email,
                        plan: ident.plan,
                        env_override,
                        matched_account_id: matched,
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
                        auth_method: Some("oauth".into()),
                        email: ident.email,
                        plan: ident.plan,
                        env_override,
                        matched_account_id: matched,
                        detail: None,
                    }
                } else {
                    ToolNativeStatus {
                        logged_in: false,
                        auth_method: None,
                        email: None,
                        plan: None,
                        env_override,
                        matched_account_id: None,
                        detail: Some("未检测到系统 Cursor 登录凭据或 CURSOR_API_KEY".into()),
                    }
                }
            }
            _ => {
                let snap = self.take_host_snapshot_unlocked(tool).unwrap_or_default();
                let logged_in = !snap.is_empty();
                let ident = if logged_in {
                    self.parse_host_identity(tool, &snap)
                } else {
                    AccountIdentity::default()
                };
                let matched = if logged_in {
                    find_matching_account(accounts, &ident, AccountAuthKind::Oauth).or_else(|| {
                        active_id
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
                    auth_method: logged_in.then(|| "oauth".into()),
                    email: ident.email,
                    plan: ident.plan,
                    env_override,
                    matched_account_id: matched,
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

fn cursor_auth_api_key(bytes: &[u8]) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    v["apiKey"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

fn cursor_snapshot_api_key(snap: &SnapshotBundle) -> Option<String> {
    snap.files
        .iter()
        .find(|(k, _)| k.ends_with("auth.json"))
        .and_then(|(_, b64)| base64::engine::general_purpose::STANDARD.decode(b64).ok())
        .and_then(|bytes| cursor_auth_api_key(&bytes))
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
    let base = ident
        .email
        .clone()
        .or_else(|| ident.workspace.clone())
        .unwrap_or_else(|| match auth_kind {
            AccountAuthKind::Oauth => format!("{} OAuth", tool.as_str()),
            AccountAuthKind::ApiKey => format!("{} API Key", tool.as_str()),
        });
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
    let email = v["id_token_email"].as_str().filter(|s| !s.is_empty())?.to_string();
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

/// Ensure `<codex_home>/config.toml` pins `cli_auth_credentials_store = "file"`, and for API-key
/// profiles writes `auth.json` + optional `model` without mutating user-wide `~/.codex`.
pub fn ensure_codex_profile_files(codex_home: &Path, api_cfg: Option<&ApiKeyConfig>) -> Result<()> {
    std::fs::create_dir_all(codex_home)?;
    let cfg_path = codex_home.join("config.toml");
    let existing = std::fs::read_to_string(&cfg_path).unwrap_or_default();
    let updated = upsert_codex_config_toml(&existing, api_cfg.map(|c| c.default_model.as_str()).filter(|s| !s.is_empty()));
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
        std::fs::create_dir_all(&cursor_dir).unwrap();
        let oauth_json = r#"{"accessToken":"oauth_tok","refreshToken":"oauth_ref","email":"oauth@example.com"}"#;
        std::fs::write(cursor_dir.join("auth.json"), oauth_json).unwrap();

        let mgr = AccountManager::with_dirs(base, home);
        let oauth_acc = mgr.capture_with_cursor_env(ToolKind::Cursor, None, None).unwrap();
        assert_eq!(oauth_acc.auth_kind, AccountAuthKind::Oauth);
        assert_eq!(oauth_acc.label, "oauth@example.com");

        // When CURSOR_API_KEY is set, native status prioritizes env_api_key over OAuth.
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
        assert_eq!(st_unmatched.env_override.as_deref(), Some("CURSOR_API_KEY"));

        // Capture while CURSOR_API_KEY is set creates an isolated ApiKey profile.
        let key_acc = mgr
            .capture_with_cursor_env(ToolKind::Cursor, Some("My Cursor Key"), Some("key_env_123"))
            .unwrap();
        assert_eq!(key_acc.auth_kind, AccountAuthKind::ApiKey);
        assert_eq!(key_acc.isolation, AccountIsolation::Isolated);
        let resolved_key = mgr.resolve(ToolKind::Cursor, Some(&key_acc.id)).unwrap();
        assert_eq!(resolved_key.env.get("CURSOR_API_KEY").unwrap(), "key_env_123");
        let resolved_oauth = mgr.resolve(ToolKind::Cursor, Some(&oauth_acc.id)).unwrap();
        assert!(resolved_oauth.env_unset.contains(&"CURSOR_API_KEY".to_string()));

        // Native status now matches the ApiKey profile, not the OAuth profile.
        let stored_after = mgr.all_accounts_for_tool(ToolKind::Cursor);
        let st_matched = mgr.probe_native_status_with_cursor_env(
            ToolKind::Cursor,
            &stored_after,
            Some(&oauth_acc.id),
            Some("key_env_123"),
        );
        assert_eq!(st_matched.matched_account_id.as_deref(), Some(key_acc.id.as_str()));

        // If an external CURSOR_API_KEY run pollutes auth.json on disk, restore_cursor_oauth_if_api_key_polluted recovers OAuth.
        std::fs::write(
            cursor_dir.join("auth.json"),
            r#"{"accessToken":"key_tok","refreshToken":"key_ref","apiKey":"key_env_123"}"#,
        )
        .unwrap();
        mgr.restore_cursor_oauth_if_api_key_polluted(&oauth_acc.id);
        let restored = std::fs::read_to_string(cursor_dir.join("auth.json")).unwrap();
        assert!(restored.contains("oauth_tok"));
        assert!(!restored.contains("key_env_123"));
    }
}
