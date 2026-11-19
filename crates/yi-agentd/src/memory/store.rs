//! Filesystem-backed Markdown storage for host personal long-term memory (`yi_memory`).
//!
//! Layout under `<YI_CONSOLE_HOME>/memory/`:
//! - `global/<id>.md`
//! - `projects/<slug>-<hash8>/project.json`
//! - `projects/<slug>-<hash8>/<id>.md`
//! - `.history/<undo_id>.json` (pre-write/delete backups for 1-click undo)
//! - `pending.json` (AI-proposed saves/deletes awaiting confirmation when `writePolicy == "confirm"`)

use anyhow::{anyhow, bail, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use yi_proto::{
    MemoryEntry, MemoryImportCandidate, MemoryMeta, MemoryMode, MemoryProjectInfo, MemoryScope,
    MemorySettings, PendingMemory,
};

pub const MAX_CONTENT_BYTES: usize = 24 * 1024;
pub const MAX_ITEMS_PER_SCOPE: usize = 100;
pub const MAX_ALWAYS_ITEM_CHARS: usize = 350;
pub const MAX_ALWAYS_TOTAL_CHARS: usize = 1800;
pub const MAX_INDEX_GLOBAL: usize = 25;
pub const MAX_INDEX_PROJECT: usize = 35;
pub const MAX_INDEX_TOTAL_CHARS: usize = 3500;
const MAX_HISTORY_RECORDS: usize = 100;
const MAX_PENDING_RECORDS: usize = 50;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProjectFile {
    key: String,
    display_name: String,
    root_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    git_common_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    aliases: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HistoryRecord {
    undo_id: String,
    /// "create" | "overwrite" | "delete" | "move"
    kind: String,
    scope: MemoryScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    project_key: Option<String>,
    id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    previous_raw: Option<String>,
    /// For move undo: original location before move
    #[serde(default, skip_serializing_if = "Option::is_none")]
    from_scope: Option<MemoryScope>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    from_project_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    from_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    from_raw: Option<String>,
    created_at: i64,
}

#[derive(Debug, Clone)]
pub struct SaveInput {
    pub scope: MemoryScope,
    pub project_key: Option<String>,
    pub cwd: Option<PathBuf>,
    pub id: String,
    pub prev_id: Option<String>,
    pub title: Option<String>,
    pub description: Option<String>,
    pub mode: Option<MemoryMode>,
    pub tags: Option<Vec<String>>,
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct SaveOutcome {
    pub entry: MemoryEntry,
    pub undo_id: String,
    pub project_name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct DeleteOutcome {
    pub undo_id: String,
    pub deleted: MemoryMeta,
    pub project_name: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchHit {
    pub scope: MemoryScope,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_key: Option<String>,
    pub id: String,
    pub title: String,
    pub description: String,
    pub mode: MemoryMode,
    pub tags: Vec<String>,
    pub score: u32,
    pub snippet: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct L0Preview {
    pub prompt: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<MemoryProjectInfo>,
    pub always_count: usize,
    pub index_count: usize,
    pub omitted_count: usize,
    pub bytes: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListSnapshot {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_project: Option<MemoryProjectInfo>,
    pub projects: Vec<MemoryProjectInfo>,
    pub global: Vec<MemoryMeta>,
    pub project: Vec<MemoryMeta>,
    pub by_project: BTreeMap<String, Vec<MemoryMeta>>,
    pub pending: Vec<PendingMemory>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UndoOutcome {
    pub action: String,
    pub scope: MemoryScope,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_key: Option<String>,
    pub id: String,
}

pub struct MemoryStore {
    root: Option<PathBuf>,
    io_lock: Mutex<()>,
}

impl Default for MemoryStore {
    fn default() -> Self {
        Self::new(None)
    }
}

impl MemoryStore {
    /// Pass `None` to resolve `<YI_CONSOLE_HOME>/memory` dynamically (matches `paths::base_dir()` in tests).
    pub fn new(root: Option<PathBuf>) -> Self {
        Self {
            root,
            io_lock: Mutex::new(()),
        }
    }

    pub fn root_dir(&self) -> PathBuf {
        if let Some(r) = &self.root {
            return r.clone();
        }
        // Tests must never touch the user's real ~/.yi-console/memory when YI_CONSOLE_HOME is unset
        #[cfg(test)]
        if std::env::var_os("YI_CONSOLE_HOME").is_none() {
            return std::env::temp_dir()
                .join(format!("yi-agentd-test-memory-{}", std::process::id()));
        }
        crate::paths::base_dir().join("memory")
    }

    fn global_dir(&self) -> PathBuf {
        self.root_dir().join("global")
    }

    fn projects_dir(&self) -> PathBuf {
        self.root_dir().join("projects")
    }

    fn project_dir(&self, key: &str) -> PathBuf {
        self.projects_dir().join(key)
    }

    fn history_dir(&self) -> PathBuf {
        self.root_dir().join(".history")
    }

    fn pending_path(&self) -> PathBuf {
        self.root_dir().join("pending.json")
    }

    fn entry_path(&self, scope: MemoryScope, project_key: Option<&str>, id: &str) -> Result<PathBuf> {
        validate_id(id)?;
        match scope {
            MemoryScope::Global => Ok(self.global_dir().join(format!("{id}.md"))),
            MemoryScope::Project => {
                let key = project_key
                    .filter(|k| valid_project_key(k))
                    .ok_or_else(|| anyhow!("缺少有效的项目标识"))?;
                Ok(self.project_dir(key).join(format!("{id}.md")))
            }
        }
    }

    /// Resolve a working directory to a project info without creating files on disk.
    pub fn resolve_project(&self, cwd: &Path) -> Option<MemoryProjectInfo> {
        let expanded = expand_cwd(cwd);
        let (git_common, root_path) = detect_git_repo(&expanded);
        let is_git = git_common.is_some();
        if !is_git && is_generic_non_project_dir(&expanded) {
            return None;
        }
        let norm_common = git_common.as_deref().map(normalize_path_key);
        let norm_root = normalize_path_key(&root_path);

        // Match against existing project manifests first so re-bound / moved repos keep their key
        for pf in self.read_all_project_files() {
            let pf_common = pf.git_common_dir.as_deref().map(normalize_path_key);
            let pf_root = normalize_path_key(&pf.root_path);
            let alias_hit = pf.aliases.iter().any(|a| {
                let na = normalize_path_key(a);
                na == norm_root || norm_common.as_deref() == Some(na.as_str())
            });
            let common_hit = norm_common.is_some() && pf_common == norm_common;
            let root_hit = pf_root == norm_root;
            if common_hit || root_hit || alias_hit {
                let count = self.list_dir_metas(MemoryScope::Project, Some(&pf.key)).len();
                return Some(MemoryProjectInfo {
                    key: pf.key,
                    display_name: pf.display_name,
                    root_path: pf.root_path,
                    git_common_dir: pf.git_common_dir,
                    aliases: pf.aliases,
                    count,
                });
            }
        }

        let canonical_id = norm_common.as_deref().unwrap_or(&norm_root);
        let display_name = root_path
            .file_name()
            .and_then(|s| s.to_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("project")
            .to_string();
        let slug = slug_prefix(&display_name);
        let hash = format!("{:x}", md5::compute(canonical_id.as_bytes()));
        let key = format!("{slug}-{}", &hash[..8]);
        let count = self.list_dir_metas(MemoryScope::Project, Some(&key)).len();
        Some(MemoryProjectInfo {
            key,
            display_name,
            root_path: display_path(&root_path),
            git_common_dir: git_common.map(|p| display_path(&p)),
            aliases: vec![],
            count,
        })
    }

    fn ensure_project_manifest(&self, info: &MemoryProjectInfo) -> Result<()> {
        let dir = self.project_dir(&info.key);
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("project.json");
        let existing: Option<ProjectFile> = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok());
        let pf = match existing {
            Some(mut old) => {
                if old.display_name.is_empty() {
                    old.display_name = info.display_name.clone();
                }
                if old.root_path.is_empty() {
                    old.root_path = info.root_path.clone();
                }
                if old.git_common_dir.is_none() {
                    old.git_common_dir = info.git_common_dir.clone();
                }
                old
            }
            None => ProjectFile {
                key: info.key.clone(),
                display_name: info.display_name.clone(),
                root_path: info.root_path.clone(),
                git_common_dir: info.git_common_dir.clone(),
                aliases: info.aliases.clone(),
            },
        };
        atomic_write(&path, &serde_json::to_vec_pretty(&pf)?)
    }

    pub fn rebind_project(&self, project_key: &str, cwd: &Path) -> Result<MemoryProjectInfo> {
        let _g = self.io_lock.lock().unwrap();
        if !valid_project_key(project_key) {
            bail!("项目标识无效");
        }
        let dir = self.project_dir(project_key);
        let manifest = dir.join("project.json");
        if !dir.is_dir() {
            bail!("找不到对应的项目记忆目录");
        }
        let expanded = expand_cwd(cwd);
        let (git_common, root_path) = detect_git_repo(&expanded);
        let mut pf: ProjectFile = std::fs::read(&manifest)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_else(|| ProjectFile {
                key: project_key.to_string(),
                display_name: project_key.to_string(),
                root_path: String::new(),
                git_common_dir: None,
                aliases: vec![],
            });
        // Preserve previous paths as aliases so old worktrees/links still resolve
        for prev in [pf.root_path.clone(), pf.git_common_dir.clone().unwrap_or_default()] {
            if !prev.is_empty() && !pf.aliases.iter().any(|a| normalize_path_key(a) == normalize_path_key(&prev)) {
                pf.aliases.push(prev);
            }
        }
        let new_root = display_path(&root_path);
        let new_common = git_common.map(|p| display_path(&p));
        if !pf.aliases.iter().any(|a| normalize_path_key(a) == normalize_path_key(&new_root)) {
            pf.aliases.push(new_root.clone());
        }
        if let Some(c) = &new_common {
            if !pf.aliases.iter().any(|a| normalize_path_key(a) == normalize_path_key(c)) {
                pf.aliases.push(c.clone());
            }
        }
        pf.root_path = new_root;
        pf.git_common_dir = new_common;
        if let Some(name) = root_path.file_name().and_then(|s| s.to_str()).filter(|s| !s.is_empty()) {
            pf.display_name = name.to_string();
        }
        atomic_write(&manifest, &serde_json::to_vec_pretty(&pf)?)?;
        let count = self.list_dir_metas(MemoryScope::Project, Some(project_key)).len();
        Ok(MemoryProjectInfo {
            key: pf.key,
            display_name: pf.display_name,
            root_path: pf.root_path,
            git_common_dir: pf.git_common_dir,
            aliases: pf.aliases,
            count,
        })
    }

    fn read_all_project_files(&self) -> Vec<ProjectFile> {
        let dir = self.projects_dir();
        let Ok(rd) = std::fs::read_dir(&dir) else {
            return vec![];
        };
        let mut out = vec![];
        for ent in rd.flatten() {
            if !ent.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let key = ent.file_name().to_string_lossy().to_string();
            if !valid_project_key(&key) {
                continue;
            }
            let manifest = ent.path().join("project.json");
            let pf = std::fs::read(&manifest)
                .ok()
                .and_then(|b| serde_json::from_slice::<ProjectFile>(&b).ok())
                .unwrap_or_else(|| ProjectFile {
                    key: key.clone(),
                    display_name: key.clone(),
                    root_path: String::new(),
                    git_common_dir: None,
                    aliases: vec![],
                });
            out.push(pf);
        }
        out.sort_by(|a, b| a.display_name.to_lowercase().cmp(&b.display_name.to_lowercase()));
        out
    }

    pub fn project_info(&self, key: &str) -> Option<MemoryProjectInfo> {
        self.read_all_project_files()
            .into_iter()
            .find(|p| p.key == key)
            .map(|pf| {
                let count = self.list_dir_metas(MemoryScope::Project, Some(&pf.key)).len();
                MemoryProjectInfo {
                    key: pf.key,
                    display_name: pf.display_name,
                    root_path: pf.root_path,
                    git_common_dir: pf.git_common_dir,
                    aliases: pf.aliases,
                    count,
                }
            })
    }

    fn list_dir_metas(&self, scope: MemoryScope, project_key: Option<&str>) -> Vec<MemoryMeta> {
        let dir = match scope {
            MemoryScope::Global => self.global_dir(),
            MemoryScope::Project => match project_key {
                Some(k) if valid_project_key(k) => self.project_dir(k),
                _ => return vec![],
            },
        };
        let Ok(rd) = std::fs::read_dir(&dir) else {
            return vec![];
        };
        let mut items = vec![];
        for ent in rd.flatten() {
            let path = ent.path();
            if !path.is_file() || path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if validate_id(stem).is_err() {
                continue;
            }
            if let Ok(entry) = read_entry_file(&path, scope, project_key, stem) {
                items.push(entry.meta);
            }
        }
        // Always-apply items first, then newest updatedAt
        items.sort_by(|a, b| {
            let ma = matches!(a.mode, MemoryMode::Always);
            let mb = matches!(b.mode, MemoryMode::Always);
            mb.cmp(&ma)
                .then_with(|| b.updated_at.cmp(&a.updated_at))
                .then_with(|| a.id.cmp(&b.id))
        });
        items
    }

    pub fn list(&self, cwd: Option<&Path>, project_key: Option<&str>) -> ListSnapshot {
        let resolved_project = match (project_key.filter(|k| valid_project_key(k)), cwd) {
            (Some(k), _) => self.project_info(k),
            (None, Some(c)) => self.resolve_project(c),
            (None, None) => None,
        };
        let global = self.list_dir_metas(MemoryScope::Global, None);
        let mut by_project = BTreeMap::new();
        let mut projects = vec![];
        for pf in self.read_all_project_files() {
            let items = self.list_dir_metas(MemoryScope::Project, Some(&pf.key));
            let count = items.len();
            if count > 0 || resolved_project.as_ref().is_some_and(|r| r.key == pf.key) {
                projects.push(MemoryProjectInfo {
                    key: pf.key.clone(),
                    display_name: pf.display_name,
                    root_path: pf.root_path,
                    git_common_dir: pf.git_common_dir,
                    aliases: pf.aliases,
                    count,
                });
                by_project.insert(pf.key, items);
            }
        }
        let project = resolved_project
            .as_ref()
            .map(|p| {
                by_project
                    .get(&p.key)
                    .cloned()
                    .unwrap_or_else(|| self.list_dir_metas(MemoryScope::Project, Some(&p.key)))
            })
            .unwrap_or_default();
        let pending = self.list_pending();
        ListSnapshot {
            resolved_project,
            projects,
            global,
            project,
            by_project,
            pending,
        }
    }

    pub fn get(
        &self,
        scope: MemoryScope,
        project_key: Option<&str>,
        cwd: Option<&Path>,
        id: &str,
    ) -> Result<MemoryEntry> {
        let pkey = self.resolve_effective_project_key(scope, project_key, cwd)?;
        let path = self.entry_path(scope, pkey.as_deref(), id)?;
        if !path.is_file() {
            bail!("找不到记忆：{id}");
        }
        read_entry_file(&path, scope, pkey.as_deref(), id)
    }

    fn resolve_effective_project_key(
        &self,
        scope: MemoryScope,
        project_key: Option<&str>,
        cwd: Option<&Path>,
    ) -> Result<Option<String>> {
        match scope {
            MemoryScope::Global => Ok(None),
            MemoryScope::Project => {
                if let Some(k) = project_key.filter(|k| !k.trim().is_empty()) {
                    if !valid_project_key(k) {
                        bail!("项目标识无效");
                    }
                    return Ok(Some(k.to_string()));
                }
                if let Some(c) = cwd {
                    let info = self
                        .resolve_project(c)
                        .ok_or_else(|| anyhow!("当前目录不是项目目录，无法保存项目记忆"))?;
                    return Ok(Some(info.key));
                }
                bail!("项目记忆需要指定项目标识或工作目录")
            }
        }
    }

    pub fn save(&self, input: SaveInput) -> Result<SaveOutcome> {
        let _g = self.io_lock.lock().unwrap();
        self.save_locked(input)
    }

    fn save_locked(&self, input: SaveInput) -> Result<SaveOutcome> {
        validate_id(&input.id)?;
        let content = normalize_newlines(&input.content).trim_end().to_string();
        if content.trim().is_empty() {
            bail!("记忆正文不能为空");
        }
        if content.len() > MAX_CONTENT_BYTES {
            bail!("记忆正文超过上限（最多 {} KB）", MAX_CONTENT_BYTES / 1024);
        }
        let (project_key, project_name) = match input.scope {
            MemoryScope::Global => (None, None),
            MemoryScope::Project => {
                if let Some(k) = input.project_key.as_deref().filter(|s| !s.trim().is_empty()) {
                    if !valid_project_key(k) {
                        bail!("项目标识无效");
                    }
                    let info = self.project_info(k).or_else(|| {
                        input.cwd.as_deref().and_then(|c| self.resolve_project(c)).filter(|p| p.key == k)
                    });
                    if let Some(ref p) = info {
                        self.ensure_project_manifest(p)?;
                    }
                    (Some(k.to_string()), info.map(|p| p.display_name))
                } else if let Some(c) = input.cwd.as_deref() {
                    let info = self
                        .resolve_project(c)
                        .ok_or_else(|| anyhow!("当前目录不是项目目录，请改存为全局记忆"))?;
                    self.ensure_project_manifest(&info)?;
                    (Some(info.key), Some(info.display_name))
                } else {
                    bail!("项目记忆需要指定项目标识或工作目录");
                }
            }
        };

        let path = self.entry_path(input.scope, project_key.as_deref(), &input.id)?;
        let prev_id_renamed = input
            .prev_id
            .as_deref()
            .filter(|p| !p.is_empty() && *p != input.id);
        if let Some(old_id) = prev_id_renamed {
            validate_id(old_id)?;
            if path.is_file() {
                bail!("目标记忆标识已存在：{}", input.id);
            }
        }

        let existing = if path.is_file() {
            Some(read_entry_file(&path, input.scope, project_key.as_deref(), &input.id)?)
        } else if let Some(old_id) = prev_id_renamed {
            let old_path = self.entry_path(input.scope, project_key.as_deref(), old_id)?;
            if old_path.is_file() {
                Some(read_entry_file(&old_path, input.scope, project_key.as_deref(), old_id)?)
            } else {
                None
            }
        } else {
            None
        };

        if existing.is_none() {
            let current_count = self.list_dir_metas(input.scope, project_key.as_deref()).len();
            if current_count >= MAX_ITEMS_PER_SCOPE {
                bail!("当前作用域的记忆已达上限（最多 {MAX_ITEMS_PER_SCOPE} 条），请先合并或删除旧记忆");
            }
        }

        let title = input
            .title
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| existing.as_ref().map(|e| e.meta.title.clone()))
            .or_else(|| extract_heading(&content))
            .unwrap_or_else(|| input.id.clone());
        let description = input
            .description
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| existing.as_ref().map(|e| e.meta.description.clone()))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| extract_first_line(&content));
        let mode = input
            .mode
            .or_else(|| existing.as_ref().map(|e| e.meta.mode))
            .unwrap_or(MemoryMode::OnDemand);
        let tags: Vec<String> = input
            .tags
            .or_else(|| existing.as_ref().map(|e| e.meta.tags.clone()))
            .unwrap_or_default()
            .into_iter()
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .take(16)
            .collect();

        let now = crate::util::now_ms();
        let entry = MemoryEntry {
            meta: MemoryMeta {
                id: input.id.clone(),
                scope: input.scope,
                project_key: project_key.clone(),
                title,
                description,
                mode,
                tags,
                updated_at: now,
                size: content.len(),
            },
            content,
        };

        let previous_raw = if path.is_file() {
            std::fs::read_to_string(&path).ok()
        } else {
            None
        };
        let undo_id = uuid::Uuid::new_v4().simple().to_string()[..12].to_string();
        let hist = HistoryRecord {
            undo_id: undo_id.clone(),
            kind: if previous_raw.is_some() {
                "overwrite".into()
            } else {
                "create".into()
            },
            scope: input.scope,
            project_key: project_key.clone(),
            id: input.id.clone(),
            previous_raw,
            from_scope: None,
            from_project_key: None,
            from_id: None,
            from_raw: None,
            created_at: now,
        };
        self.write_history(&hist)?;

        let formatted = format_entry_markdown(&entry);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        atomic_write(&path, formatted.as_bytes())?;

        if let Some(old_id) = prev_id_renamed {
            if let Ok(old_path) = self.entry_path(input.scope, project_key.as_deref(), old_id) {
                let _ = std::fs::remove_file(old_path);
            }
        }

        Ok(SaveOutcome {
            entry,
            undo_id,
            project_name,
        })
    }

    pub fn delete(
        &self,
        scope: MemoryScope,
        project_key: Option<&str>,
        cwd: Option<&Path>,
        id: &str,
    ) -> Result<DeleteOutcome> {
        let _g = self.io_lock.lock().unwrap();
        self.delete_locked(scope, project_key, cwd, id)
    }

    fn delete_locked(
        &self,
        scope: MemoryScope,
        project_key: Option<&str>,
        cwd: Option<&Path>,
        id: &str,
    ) -> Result<DeleteOutcome> {
        let pkey = self.resolve_effective_project_key(scope, project_key, cwd)?;
        let path = self.entry_path(scope, pkey.as_deref(), id)?;
        if !path.is_file() {
            bail!("找不到记忆：{id}");
        }
        let existing = read_entry_file(&path, scope, pkey.as_deref(), id)?;
        let raw = std::fs::read_to_string(&path)?;
        let project_name = pkey
            .as_deref()
            .and_then(|k| self.project_info(k))
            .map(|p| p.display_name);
        let undo_id = uuid::Uuid::new_v4().simple().to_string()[..12].to_string();
        let hist = HistoryRecord {
            undo_id: undo_id.clone(),
            kind: "delete".into(),
            scope,
            project_key: pkey.clone(),
            id: id.to_string(),
            previous_raw: Some(raw),
            from_scope: None,
            from_project_key: None,
            from_id: None,
            from_raw: None,
            created_at: crate::util::now_ms(),
        };
        self.write_history(&hist)?;
        std::fs::remove_file(&path)?;
        Ok(DeleteOutcome {
            undo_id,
            deleted: existing.meta,
            project_name,
        })
    }

    pub fn move_entry(
        &self,
        from_scope: MemoryScope,
        from_project_key: Option<&str>,
        id: &str,
        to_scope: MemoryScope,
        to_project_key: Option<&str>,
        cwd: Option<&Path>,
        new_id: Option<&str>,
    ) -> Result<SaveOutcome> {
        let _g = self.io_lock.lock().unwrap();
        validate_id(id)?;
        let target_id = new_id.filter(|s| !s.trim().is_empty()).unwrap_or(id);
        validate_id(target_id)?;

        let src_path = self.entry_path(from_scope, from_project_key, id)?;
        if !src_path.is_file() {
            bail!("找不到要移动的记忆：{id}");
        }
        let src_raw = std::fs::read_to_string(&src_path)?;
        let mut entry = read_entry_file(&src_path, from_scope, from_project_key, id)?;

        let (dst_key, project_name) = match to_scope {
            MemoryScope::Global => (None, None),
            MemoryScope::Project => {
                if let Some(k) = to_project_key.filter(|s| !s.trim().is_empty()) {
                    if !valid_project_key(k) {
                        bail!("目标项目标识无效");
                    }
                    let info = self.project_info(k);
                    if let Some(ref p) = info {
                        self.ensure_project_manifest(p)?;
                    }
                    (Some(k.to_string()), info.map(|p| p.display_name))
                } else if let Some(c) = cwd {
                    let info = self
                        .resolve_project(c)
                        .ok_or_else(|| anyhow!("当前目录不是项目目录"))?;
                    self.ensure_project_manifest(&info)?;
                    (Some(info.key), Some(info.display_name))
                } else {
                    bail!("移动到项目记忆需要指定目标项目");
                }
            }
        };

        let dst_path = self.entry_path(to_scope, dst_key.as_deref(), target_id)?;
        if dst_path == src_path {
            return Ok(SaveOutcome {
                entry,
                undo_id: String::new(),
                project_name,
            });
        }
        if dst_path.is_file() {
            bail!("目标位置已存在同名记忆：{target_id}");
        }

        let current_count = self.list_dir_metas(to_scope, dst_key.as_deref()).len();
        if current_count >= MAX_ITEMS_PER_SCOPE {
            bail!("目标作用域的记忆已达上限（最多 {MAX_ITEMS_PER_SCOPE} 条）");
        }

        entry.meta.id = target_id.to_string();
        entry.meta.scope = to_scope;
        entry.meta.project_key = dst_key.clone();
        entry.meta.updated_at = crate::util::now_ms();

        let undo_id = uuid::Uuid::new_v4().simple().to_string()[..12].to_string();
        let hist = HistoryRecord {
            undo_id: undo_id.clone(),
            kind: "move".into(),
            scope: to_scope,
            project_key: dst_key.clone(),
            id: target_id.to_string(),
            previous_raw: None,
            from_scope: Some(from_scope),
            from_project_key: from_project_key.map(str::to_string),
            from_id: Some(id.to_string()),
            from_raw: Some(src_raw),
            created_at: entry.meta.updated_at,
        };
        self.write_history(&hist)?;

        if let Some(parent) = dst_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        atomic_write(&dst_path, format_entry_markdown(&entry).as_bytes())?;
        std::fs::remove_file(&src_path)?;

        Ok(SaveOutcome {
            entry,
            undo_id,
            project_name,
        })
    }

    pub fn undo(&self, undo_id: &str) -> Result<UndoOutcome> {
        let _g = self.io_lock.lock().unwrap();
        if !valid_simple_token(undo_id) {
            bail!("撤销标识无效");
        }
        let hpath = self.history_dir().join(format!("{undo_id}.json"));
        if !hpath.is_file() {
            bail!("没有找到对应的历史记录，可能已经撤销或过期");
        }
        let rec: HistoryRecord = serde_json::from_slice(&std::fs::read(&hpath)?)?;
        let target_path = self.entry_path(rec.scope, rec.project_key.as_deref(), &rec.id)?;
        match rec.kind.as_str() {
            "create" => {
                if target_path.is_file() {
                    let _ = std::fs::remove_file(&target_path);
                }
            }
            "overwrite" | "delete" => {
                if let Some(raw) = &rec.previous_raw {
                    if let Some(parent) = target_path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    atomic_write(&target_path, raw.as_bytes())?;
                }
            }
            "move" => {
                if target_path.is_file() {
                    let _ = std::fs::remove_file(&target_path);
                }
                if let (Some(fs), Some(fid), Some(raw)) =
                    (rec.from_scope, rec.from_id.as_deref(), rec.from_raw.as_deref())
                {
                    let orig = self.entry_path(fs, rec.from_project_key.as_deref(), fid)?;
                    if let Some(parent) = orig.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    atomic_write(&orig, raw.as_bytes())?;
                }
            }
            _ => bail!("不支持的历史记录类型"),
        }
        let _ = std::fs::remove_file(&hpath);
        Ok(UndoOutcome {
            action: rec.kind,
            scope: rec.scope,
            project_key: rec.project_key,
            id: rec.id,
        })
    }

    fn write_history(&self, rec: &HistoryRecord) -> Result<()> {
        let dir = self.history_dir();
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{}.json", rec.undo_id));
        atomic_write(&path, &serde_json::to_vec(rec)?)?;
        // Prune oldest records past MAX_HISTORY_RECORDS
        if let Ok(rd) = std::fs::read_dir(&dir) {
            let mut files: Vec<(i64, PathBuf)> = rd
                .flatten()
                .filter_map(|e| {
                    let p = e.path();
                    if p.extension().and_then(|x| x.to_str()) != Some("json") {
                        return None;
                    }
                    let mtime = e
                        .metadata()
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_millis() as i64)
                        .unwrap_or(0);
                    Some((mtime, p))
                })
                .collect();
            if files.len() > MAX_HISTORY_RECORDS {
                files.sort_by_key(|(t, _)| *t);
                let excess = files.len() - MAX_HISTORY_RECORDS;
                for (_, p) in files.into_iter().take(excess) {
                    let _ = std::fs::remove_file(p);
                }
            }
        }
        Ok(())
    }

    // ---------- Pending confirmation queue (when write_policy == "confirm") ----------

    pub fn list_pending(&self) -> Vec<PendingMemory> {
        std::fs::read(self.pending_path())
            .ok()
            .and_then(|b| serde_json::from_slice::<Vec<PendingMemory>>(&b).ok())
            .unwrap_or_default()
    }

    pub fn enqueue_pending(&self, mut item: PendingMemory) -> Result<PendingMemory> {
        let _g = self.io_lock.lock().unwrap();
        validate_id(&item.id)?;
        let mut list = self.list_pending();
        // Replace an earlier pending proposal targeting the same (scope, project_key, id)
        list.retain(|x| {
            !(x.scope == item.scope && x.project_key == item.project_key && x.id == item.id)
        });
        if item.pending_id.is_empty() {
            item.pending_id = uuid::Uuid::new_v4().simple().to_string()[..12].to_string();
        }
        if item.created_at == 0 {
            item.created_at = crate::util::now_ms();
        }
        list.insert(0, item.clone());
        list.truncate(MAX_PENDING_RECORDS);
        let p = self.pending_path();
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        atomic_write(&p, &serde_json::to_vec_pretty(&list)?)?;
        Ok(item)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn resolve_pending(
        &self,
        pending_id: &str,
        approve: bool,
        scope_override: Option<MemoryScope>,
        project_key_override: Option<String>,
        id_override: Option<String>,
        title_override: Option<String>,
        description_override: Option<String>,
        mode_override: Option<MemoryMode>,
        tags_override: Option<Vec<String>>,
        content_override: Option<String>,
    ) -> Result<Option<MemoryEntry>> {
        let _g = self.io_lock.lock().unwrap();
        let mut list = self.list_pending();
        let Some(pos) = list.iter().position(|x| x.pending_id == pending_id) else {
            bail!("这条待确认记忆已不存在或已处理");
        };
        let item = list.remove(pos);
        let result = if approve {
            let scope = scope_override.unwrap_or(item.scope);
            let pkey = project_key_override.or(item.project_key);
            let id = id_override.filter(|s| !s.trim().is_empty()).unwrap_or(item.id);
            match item.action.as_str() {
                "delete" => {
                    let _ = self.delete_locked(scope, pkey.as_deref(), None, &id)?;
                    None
                }
                _ => {
                    let saved = self.save_locked(SaveInput {
                        scope,
                        project_key: pkey,
                        cwd: None,
                        id,
                        prev_id: None,
                        title: title_override.or(Some(item.title)),
                        description: description_override.or(Some(item.description)),
                        mode: mode_override.or(Some(item.mode)),
                        tags: tags_override.or(Some(item.tags)),
                        content: content_override.unwrap_or(item.content),
                    })?;
                    Some(saved.entry)
                }
            }
        } else {
            None
        };
        let p = self.pending_path();
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        atomic_write(&p, &serde_json::to_vec_pretty(&list)?)?;
        Ok(result)
    }

    // ---------- Search (CJK bigram + ASCII words + substring bonus) ----------

    pub fn search(
        &self,
        query: &str,
        scope_filter: Option<MemoryScope>,
        project_key: Option<&str>,
        cwd: Option<&Path>,
        limit: usize,
    ) -> Vec<SearchHit> {
        let q = query.trim();
        if q.is_empty() {
            return vec![];
        }
        let pkey = match project_key {
            Some(k) if valid_project_key(k) => Some(k.to_string()),
            _ => cwd.and_then(|c| self.resolve_project(c)).map(|p| p.key),
        };
        let tokens = tokenize_query(q);
        let q_lower = q.to_lowercase();
        let mut candidates: Vec<(MemoryScope, Option<String>, MemoryMeta)> = vec![];
        if scope_filter.is_none() || scope_filter == Some(MemoryScope::Global) {
            for m in self.list_dir_metas(MemoryScope::Global, None) {
                candidates.push((MemoryScope::Global, None, m));
            }
        }
        if (scope_filter.is_none() || scope_filter == Some(MemoryScope::Project)) && pkey.is_some() {
            let pk = pkey.as_deref();
            for m in self.list_dir_metas(MemoryScope::Project, pk) {
                candidates.push((MemoryScope::Project, pkey.clone(), m));
            }
        }

        let mut hits = vec![];
        for (scope, pk, meta) in candidates {
            let Ok(path) = self.entry_path(scope, pk.as_deref(), &meta.id) else {
                continue;
            };
            let Ok(entry) = read_entry_file(&path, scope, pk.as_deref(), &meta.id) else {
                continue;
            };
            let id_l = entry.meta.id.to_lowercase();
            let title_l = entry.meta.title.to_lowercase();
            let desc_l = entry.meta.description.to_lowercase();
            let tags_l: Vec<String> = entry.meta.tags.iter().map(|t| t.to_lowercase()).collect();
            let body_l = entry.content.to_lowercase();

            let mut score = 0u32;
            if id_l.contains(&q_lower) {
                score += 40;
            }
            if title_l.contains(&q_lower) {
                score += 36;
            }
            if tags_l.iter().any(|t| t.contains(&q_lower)) {
                score += 30;
            }
            if desc_l.contains(&q_lower) {
                score += 24;
            }
            if body_l.contains(&q_lower) {
                score += 14;
            }

            let mut matched_tokens = 0u32;
            for tok in &tokens {
                let mut hit_tok = false;
                if id_l.contains(tok) {
                    score += 12;
                    hit_tok = true;
                }
                if title_l.contains(tok) {
                    score += 12;
                    hit_tok = true;
                }
                if tags_l.iter().any(|t| t.contains(tok)) {
                    score += 10;
                    hit_tok = true;
                }
                if desc_l.contains(tok) {
                    score += 7;
                    hit_tok = true;
                }
                if body_l.contains(tok) {
                    score += 3;
                    hit_tok = true;
                }
                if hit_tok {
                    matched_tokens += 1;
                }
            }
            if score == 0 {
                continue;
            }
            // Small coverage bonus when multiple query terms match
            score += matched_tokens * 2;
            // Slight tie-breaker preferring project scope on equal relevance
            if scope == MemoryScope::Project {
                score += 1;
            }
            let snippet = extract_snippet(&entry.content, &q_lower, &tokens);
            hits.push(SearchHit {
                scope,
                project_key: pk,
                id: entry.meta.id,
                title: entry.meta.title,
                description: entry.meta.description,
                mode: entry.meta.mode,
                tags: entry.meta.tags,
                score,
                snippet,
            });
        }
        hits.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.id.cmp(&b.id)));
        hits.truncate(limit.clamp(1, 50));
        hits
    }

    // ---------- L0 Prompt Snapshot Builder ----------

    pub fn build_l0_preview(
        &self,
        cwd: Option<&Path>,
        project_key: Option<&str>,
        settings: &MemorySettings,
        readonly_session: bool,
    ) -> L0Preview {
        let project = match (project_key.filter(|k| valid_project_key(k)), cwd) {
            (Some(k), _) => self.project_info(k),
            (None, Some(c)) => self.resolve_project(c),
            (None, None) => None,
        };
        if !settings.enabled {
            return L0Preview {
                prompt: String::new(),
                project,
                always_count: 0,
                index_count: 0,
                omitted_count: 0,
                bytes: 0,
            };
        }

        let global_metas = self.list_dir_metas(MemoryScope::Global, None);
        let project_metas = project
            .as_ref()
            .map(|p| self.list_dir_metas(MemoryScope::Project, Some(&p.key)))
            .unwrap_or_default();

        let can_write = !readonly_session && settings.normalized_policy() != "readonly";

        // When there are zero memories and the session cannot write, omit completely (0 token cost)
        if global_metas.is_empty() && project_metas.is_empty() && !can_write {
            return L0Preview {
                prompt: String::new(),
                project,
                always_count: 0,
                index_count: 0,
                omitted_count: 0,
                bytes: 0,
            };
        }

        let header_attr = project
            .as_ref()
            .map(|p| format!(" project=\"{}\"", escape_xml_attr(&p.display_name)))
            .unwrap_or_default();

        let mut out = format!("<yi-memory{header_attr}>\n");
        out.push_str(
            "Personal environment memories managed by Yi Console (stored outside the git repository). \
             Project memories take precedence over global memories on conflict.\n",
        );
        if !global_metas.is_empty() || !project_metas.is_empty() {
            out.push_str(
                "When a topic below is relevant—especially before running build, test, deploy, or server operations—call `memory_read` (from `yi_memory`) first.\n",
            );
        }
        if can_write {
            let write_note = if settings.normalized_policy() == "confirm" {
                "When the user explicitly asks you to remember a durable personal/environment preference or workflow rule, call `memory_save` (saved as a proposal for the user to confirm). "
            } else {
                "When the user explicitly asks you to remember a durable personal/environment preference or workflow rule, call `memory_save` (check existing topics first and update by id instead of creating duplicates; use scope=\"project\" for repo-specific paths/scripts and scope=\"global\" for host-wide environment/preferences). "
            };
            out.push_str(write_note);
            out.push_str(
                "Prefer `yi_memory` over tool-specific memory files so memories stay consistent across tools. Never store passwords, API keys, tokens, or private keys.\n",
            );
        }

        // Section 1: Always-apply short guardrails (Global first, Project second so Project wins on conflict)
        let mut always_lines = vec![];
        let mut always_chars = 0usize;
        let mut always_count = 0usize;
        let mut overflow_to_index: Vec<(MemoryScope, MemoryMeta)> = vec![];

        let mut collect_always = |scope: MemoryScope, pkey: Option<&str>, metas: &[MemoryMeta]| {
            for m in metas.iter().filter(|m| m.mode == MemoryMode::Always) {
                let Ok(path) = self.entry_path(scope, pkey, &m.id) else {
                    continue;
                };
                let Ok(entry) = read_entry_file(&path, scope, pkey, &m.id) else {
                    continue;
                };
                let body = compact_always_body(&entry.content, MAX_ALWAYS_ITEM_CHARS);
                let line = format!("- [{}:{}] {}", scope.as_str(), m.id, body);
                let chars = line.chars().count();
                if always_chars + chars + 1 <= MAX_ALWAYS_TOTAL_CHARS {
                    always_chars += chars + 1;
                    always_count += 1;
                    always_lines.push(line);
                } else {
                    overflow_to_index.push((scope, m.clone()));
                }
            }
        };
        collect_always(MemoryScope::Global, None, &global_metas);
        if let Some(p) = &project {
            collect_always(MemoryScope::Project, Some(&p.key), &project_metas);
        }

        if !always_lines.is_empty() {
            out.push_str("\n## Always-apply rules\n");
            for l in always_lines {
                out.push_str(&l);
                out.push('\n');
            }
        }

        // Section 2: On-demand progressive disclosure index
        let global_od: Vec<&MemoryMeta> = global_metas
            .iter()
            .filter(|m| m.mode == MemoryMode::OnDemand)
            .chain(
                overflow_to_index
                    .iter()
                    .filter(|(s, _)| *s == MemoryScope::Global)
                    .map(|(_, m)| m),
            )
            .collect();
        let project_od: Vec<&MemoryMeta> = project_metas
            .iter()
            .filter(|m| m.mode == MemoryMode::OnDemand)
            .chain(
                overflow_to_index
                    .iter()
                    .filter(|(s, _)| *s == MemoryScope::Project)
                    .map(|(_, m)| m),
            )
            .collect();

        let mut index_chars = 0usize;
        let mut index_count = 0usize;
        let mut omitted_count = 0usize;
        let mut global_lines = vec![];
        let mut project_lines = vec![];

        let mut push_index = |list: &[&MemoryMeta], cap: usize, target: &mut Vec<String>| {
            for (i, m) in list.iter().enumerate() {
                if i >= cap {
                    omitted_count += 1;
                    continue;
                }
                let desc = if m.description.is_empty() {
                    m.title.as_str()
                } else {
                    m.description.as_str()
                };
                let line = format!("- `{}`: {}", m.id, truncate_chars(desc, 140));
                let chars = line.chars().count();
                if index_chars + chars + 1 > MAX_INDEX_TOTAL_CHARS {
                    omitted_count += 1;
                    continue;
                }
                index_chars += chars + 1;
                index_count += 1;
                target.push(line);
            }
        };
        push_index(&global_od, MAX_INDEX_GLOBAL, &mut global_lines);
        push_index(&project_od, MAX_INDEX_PROJECT, &mut project_lines);

        if !global_lines.is_empty() || !project_lines.is_empty() {
            out.push_str("\n## On-demand memories (call `memory_read` with scope + id, or `memory_search`)\n");
            if !global_lines.is_empty() {
                out.push_str("### Global\n");
                for l in global_lines {
                    out.push_str(&l);
                    out.push('\n');
                }
            }
            if !project_lines.is_empty() {
                let pname = project
                    .as_ref()
                    .map(|p| p.display_name.as_str())
                    .unwrap_or("current");
                out.push_str(&format!("### Project ({pname})\n"));
                for l in project_lines {
                    out.push_str(&l);
                    out.push('\n');
                }
            }
            if omitted_count > 0 {
                out.push_str(&format!(
                    "...and {omitted_count} more memories; use `memory_search` or `memory_list`.\n"
                ));
            }
        }

        out.push_str("</yi-memory>");
        let bytes = out.len();
        L0Preview {
            prompt: out,
            project,
            always_count,
            index_count,
            omitted_count,
            bytes,
        }
    }

    // ---------- Read-only Import Scanner ----------

    pub fn scan_import(&self, cwd: Option<&Path>) -> (Vec<MemoryImportCandidate>, Option<MemoryProjectInfo>) {
        let project = cwd.and_then(|c| self.resolve_project(c));
        let existing_global: HashSet<String> = self
            .list_dir_metas(MemoryScope::Global, None)
            .into_iter()
            .map(|m| m.id)
            .collect();
        let existing_project: HashSet<String> = project
            .as_ref()
            .map(|p| {
                self.list_dir_metas(MemoryScope::Project, Some(&p.key))
                    .into_iter()
                    .map(|m| m.id)
                    .collect()
            })
            .unwrap_or_default();

        let mut seen_paths = HashSet::new();
        let mut out = vec![];
        let mut add_file = |source: &str, path: PathBuf, scope: MemoryScope, id_hint: Option<&str>| {
            if !path.is_file() {
                return;
            }
            let can = normalize_path_key(&std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone()));
            if !seen_paths.insert(can) {
                return;
            }
            let Ok(raw) = std::fs::read_to_string(&path) else {
                return;
            };
            if raw.trim().is_empty() || raw.len() > 256 * 1024 {
                return;
            }
            let (fm, body) = split_frontmatter(&raw);
            let body_clean = normalize_newlines(body).trim().to_string();
            if body_clean.is_empty() {
                return;
            }
            let stem = id_hint
                .map(str::to_string)
                .or_else(|| path.file_stem().and_then(|s| s.to_str()).map(str::to_string))
                .unwrap_or_else(|| "imported".into());
            let suggested_id = slugify_id(&stem);
            let title = fm
                .as_deref()
                .and_then(|f| fm_value(f, "title"))
                .or_else(|| extract_heading(&body_clean))
                .unwrap_or_else(|| stem.clone());
            let description = fm
                .as_deref()
                .and_then(|f| fm_value(f, "description"))
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| extract_first_line(&body_clean));
            let hints_always = fm.as_deref().is_some_and(|f| {
                fm_value(f, "mode").as_deref() == Some("always")
                    || fm_value(f, "alwaysApply").as_deref() == Some("true")
                    || fm_value(f, "inclusion").as_deref() == Some("always")
            });
            let suggested_mode = if hints_always && body_clean.chars().count() <= MAX_ALWAYS_ITEM_CHARS {
                MemoryMode::Always
            } else {
                MemoryMode::OnDemand
            };
            let already_exists = match scope {
                MemoryScope::Global => existing_global.contains(&suggested_id),
                MemoryScope::Project => existing_project.contains(&suggested_id),
            };
            out.push(MemoryImportCandidate {
                source: source.to_string(),
                path: display_path(&path),
                suggested_scope: scope,
                suggested_id,
                title,
                description,
                suggested_mode,
                content: body_clean,
                already_exists,
            });
        };

        let collect_dir = |source: &str, dir: PathBuf, scope: MemoryScope, out_files: &mut Vec<(String, PathBuf, MemoryScope, Option<&'static str>)>| {
            let Ok(rd) = std::fs::read_dir(&dir) else {
                return;
            };
            let mut files: Vec<PathBuf> = rd
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.is_file()
                        && matches!(
                            p.extension().and_then(|x| x.to_str()),
                            Some("md" | "mdc")
                        )
                })
                .collect();
            files.sort();
            for f in files {
                out_files.push((source.to_string(), f, scope, None));
            }
        };

        let mut candidates_to_scan: Vec<(String, PathBuf, MemoryScope, Option<&'static str>)> = vec![];
        // Project-local personal memory files (typically gitignored)
        if let Some(p) = &project {
            let root = PathBuf::from(&p.root_path);
            collect_dir("kiro-project", root.join(".kiro").join("steering"), MemoryScope::Project, &mut candidates_to_scan);
            collect_dir("antigravity-project", root.join(".agents").join("rules"), MemoryScope::Project, &mut candidates_to_scan);
            candidates_to_scan.push(("antigravity-project".into(), root.join("GEMINI.md"), MemoryScope::Project, Some("gemini-rules")));
            candidates_to_scan.push(("claude-project".into(), root.join("CLAUDE.local.md"), MemoryScope::Project, Some("claude-local")));
            collect_dir("cursor-project", root.join(".cursor").join("rules"), MemoryScope::Project, &mut candidates_to_scan);
            collect_dir("codebuddy-project", root.join(".codebuddy").join("rules"), MemoryScope::Project, &mut candidates_to_scan);
            // Claude Code auto memory: ~/.claude/projects/<encoded-root>/memory/*.md
            let encoded = encode_claude_project_dir(&root);
            collect_dir(
                "claude-auto-memory",
                crate::paths::home()
                    .join(".claude")
                    .join("projects")
                    .join(encoded)
                    .join("memory"),
                MemoryScope::Project,
                &mut candidates_to_scan,
            );
        }

        // Host global tool memory files
        let home = crate::paths::home();
        collect_dir("kiro-global", home.join(".kiro").join("steering"), MemoryScope::Global, &mut candidates_to_scan);
        candidates_to_scan.push(("claude-global".into(), home.join(".claude").join("CLAUDE.md"), MemoryScope::Global, Some("claude-global")));
        candidates_to_scan.push(("codex-global".into(), home.join(".codex").join("AGENTS.md"), MemoryScope::Global, Some("codex-global")));
        candidates_to_scan.push(("antigravity-global".into(), home.join(".gemini").join("GEMINI.md"), MemoryScope::Global, Some("gemini-global")));
        candidates_to_scan.push(("codebuddy-global".into(), home.join(".codebuddy").join("CODEBUDDY.md"), MemoryScope::Global, Some("codebuddy-global")));

        for (source, path, scope, id_hint) in candidates_to_scan {
            add_file(&source, path, scope, id_hint);
        }

        (out, project)
    }
}

// ---------- Secret detection (applied to AI MCP writes) ----------

/// Returns a short Chinese category label if the text appears to contain an actual secret / credential.
pub fn detect_secret(text: &str) -> Option<&'static str> {
    static RULES: OnceLock<Vec<(&'static str, Regex)>> = OnceLock::new();
    let rules = RULES.get_or_init(|| {
        vec![
            ("私钥内容", Regex::new(r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----").unwrap()),
            ("OpenAI / Anthropic API Key", Regex::new(r"\bsk-(?:ant|proj|live|test)-[A-Za-z0-9_-]{16,}").unwrap()),
            ("API Key", Regex::new(r"\bsk-[A-Za-z0-9]{32,}\b").unwrap()),
            ("GitHub Token", Regex::new(r"\b(?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{20,}\b").unwrap()),
            ("GitHub PAT", Regex::new(r"\bgithub_pat_[A-Za-z0-9_]{20,}\b").unwrap()),
            ("AWS Access Key", Regex::new(r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b").unwrap()),
            ("Slack Token", Regex::new(r"\bxox[baprs]-[0-9A-Za-z-]{16,}\b").unwrap()),
            ("JWT Token", Regex::new(r"\beyJ[A-Za-z0-9_-]{16,}\.eyJ[A-Za-z0-9_-]{16,}\.[A-Za-z0-9_-]{16,}\b").unwrap()),
            (
                "明文密码或密钥赋值",
                Regex::new(r#"(?i)\b(?:password|passwd|api[_-]?key|secret[_-]?key|access[_-]?token|auth[_-]?token)\s*[:=]\s*["']?([^\s"']{8,})["']?"#).unwrap(),
            ),
        ]
    });
    for (label, re) in rules {
        if let Some(caps) = re.captures(text) {
            // For the assignment rule, ignore placeholders like ********, <secret>, ${VAR}, your_key
            if let Some(val) = caps.get(1).map(|m| m.as_str()) {
                let lower = val.to_ascii_lowercase();
                if val.chars().all(|c| c == '*' || c == 'x' || c == 'X' || c == '.')
                    || lower.starts_with("your_")
                    || lower.starts_with("your-")
                    || lower.starts_with("example")
                    || lower.starts_with("changeme")
                    || lower.starts_with("placeholder")
                    || lower.starts_with('<')
                    || lower.starts_with("${")
                    || lower.starts_with('$')
                {
                    continue;
                }
            }
            return Some(label);
        }
    }
    None
}

// ---------- Helpers ----------

pub fn validate_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 64
        || id == "project"
        || id.starts_with('.')
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        bail!("记忆标识只能包含字母、数字、下划线和短横线，长度为 1–64（不能为 project）");
    }
    Ok(())
}

fn valid_project_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 96
        && !key.starts_with('.')
        && !key.contains("..")
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn valid_simple_token(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

pub fn slugify_id(raw: &str) -> String {
    let mut s = String::new();
    let mut prev_dash = false;
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            s.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if (ch == '-' || ch == '_' || ch.is_whitespace() || ch == '.') && !s.is_empty() && !prev_dash {
            s.push('-');
            prev_dash = true;
        }
        if s.len() >= 48 {
            break;
        }
    }
    let s = s.trim_matches('-').to_string();
    if s.is_empty() || s == "project" {
        let h = format!("{:x}", md5::compute(raw.as_bytes()));
        format!("note-{}", &h[..8])
    } else {
        s
    }
}

fn slug_prefix(name: &str) -> String {
    let mut s = String::new();
    let mut prev_dash = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            s.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !s.is_empty() && !prev_dash {
            s.push('-');
            prev_dash = true;
        }
        if s.len() >= 28 {
            break;
        }
    }
    let trimmed = s.trim_matches('-');
    if trimmed.is_empty() {
        "proj".into()
    } else {
        trimmed.to_string()
    }
}

fn expand_cwd(cwd: &Path) -> PathBuf {
    let s = cwd.to_string_lossy();
    let p = crate::paths::expand(&s);
    std::fs::canonicalize(&p).unwrap_or(p)
}

fn strip_verbatim(p: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let s = p.to_string_lossy();
        if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{rest}"));
        }
        if let Some(rest) = s.strip_prefix(r"\\?\") {
            return PathBuf::from(rest);
        }
    }
    p.to_path_buf()
}

fn display_path(p: &Path) -> String {
    strip_verbatim(p).to_string_lossy().to_string()
}

fn normalize_path_key(p: impl AsRef<Path>) -> String {
    let s = display_path(p.as_ref());
    let s = s.trim_end_matches(['/', '\\']).to_string();
    #[cfg(windows)]
    {
        s.replace('\\', "/").to_ascii_lowercase()
    }
    #[cfg(not(windows))]
    {
        s
    }
}

fn is_generic_non_project_dir(dir: &Path) -> bool {
    let norm = normalize_path_key(dir);
    if norm.is_empty() || norm == "/" {
        return true;
    }
    let home = normalize_path_key(crate::paths::home());
    if norm == home {
        return true;
    }
    let tmp = normalize_path_key(std::env::temp_dir());
    if norm == tmp || norm == "/tmp" {
        return true;
    }
    let bots_root = normalize_path_key(crate::paths::base_dir().join("bots"));
    if norm == bots_root || norm.starts_with(&format!("{bots_root}/")) {
        return true;
    }
    false
}

/// Resolve `(git_common_dir, repo_root_or_cwd)` using `GIT_OPTIONAL_LOCKS=0`.
fn detect_git_repo(cwd: &Path) -> (Option<PathBuf>, PathBuf) {
    if !cwd.exists() {
        return (None, strip_verbatim(cwd));
    }
    let git_out = |args: &[&str]| -> Option<String> {
        let mut cmd = Command::new("git");
        cmd.args(args)
            .current_dir(cwd)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .stdin(std::process::Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000);
        }
        let out = cmd.output().ok()?;
        if !out.status.success() {
            return None;
        }
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if s.is_empty() { None } else { Some(s) }
    };

    let common_raw = git_out(&["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .or_else(|| git_out(&["rev-parse", "--git-common-dir"]));
    let Some(common_raw) = common_raw else {
        return (None, strip_verbatim(cwd));
    };
    let common_path = {
        let p = PathBuf::from(&common_raw);
        let abs = if p.is_absolute() { p } else { cwd.join(p) };
        strip_verbatim(&std::fs::canonicalize(&abs).unwrap_or(abs))
    };
    // For a standard repo or linked worktree, `<common_dir>` is `<main-repo>/.git`, so its parent is the main repo root
    let root = if common_path.file_name().and_then(|s| s.to_str()) == Some(".git") {
        common_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| strip_verbatim(cwd))
    } else if let Some(top) = git_out(&["rev-parse", "--show-toplevel"]) {
        strip_verbatim(&std::fs::canonicalize(&top).unwrap_or_else(|_| PathBuf::from(top)))
    } else {
        strip_verbatim(cwd)
    };
    (Some(common_path), root)
}

fn encode_claude_project_dir(root: &Path) -> String {
    display_path(root)
        .chars()
        .map(|c| if c == '/' || c == '\\' || c == ':' { '-' } else { c })
        .collect()
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension(format!("tmp.{}", uuid::Uuid::new_v4().simple()));
    std::fs::write(&tmp, bytes)?;
    if let Err(e) = std::fs::rename(&tmp, path) {
        // Fallback if target is read-only / replace fails on some platforms
        if path.exists() {
            let _ = std::fs::remove_file(path);
            if std::fs::rename(&tmp, path).is_ok() {
                return Ok(());
            }
        }
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(())
}

fn normalize_newlines(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\r', "\n")
}

fn split_frontmatter(raw: &str) -> (Option<String>, &str) {
    let trimmed = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    let Some(rest) = trimmed
        .strip_prefix("---\n")
        .or_else(|| trimmed.strip_prefix("---\r\n"))
    else {
        return (None, trimmed);
    };
    let mut offset = 0usize;
    for line in rest.split_inclusive('\n') {
        let bare = line.trim_end_matches(['\r', '\n']);
        if bare == "---" {
            let fm = rest[..offset].to_string();
            let body = &rest[offset + line.len()..];
            return (Some(fm), body);
        }
        offset += line.len();
    }
    (None, trimmed)
}

fn unquote_yaml(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        return serde_json::from_str::<String>(s).unwrap_or_else(|_| s[1..s.len() - 1].to_string());
    }
    if s.len() >= 2 && s.starts_with('\'') && s.ends_with('\'') {
        return s[1..s.len() - 1].replace("''", "'");
    }
    s.to_string()
}

fn fm_value(fm: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}:");
    for line in fm.lines() {
        let l = line.trim();
        if let Some(rest) = l.strip_prefix(&prefix) {
            let val = unquote_yaml(rest);
            if !val.is_empty() {
                return Some(val);
            }
        }
    }
    None
}

fn fm_tags(fm: &str) -> Vec<String> {
    let mut tags = vec![];
    let mut in_list = false;
    for line in fm.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("tags:") {
            let rest = rest.trim();
            if rest.starts_with('[') && rest.ends_with(']') {
                for item in rest[1..rest.len() - 1].split(',') {
                    let v = unquote_yaml(item);
                    if !v.is_empty() {
                        tags.push(v);
                    }
                }
                in_list = false;
            } else if rest.is_empty() {
                in_list = true;
            } else {
                let v = unquote_yaml(rest);
                if !v.is_empty() {
                    tags.push(v);
                }
                in_list = false;
            }
        } else if in_list {
            if let Some(item) = trimmed.strip_prefix("- ") {
                let v = unquote_yaml(item);
                if !v.is_empty() {
                    tags.push(v);
                }
            } else if !trimmed.is_empty() {
                in_list = false;
            }
        }
    }
    tags
}

fn yaml_scalar(s: &str) -> String {
    let single = s.replace(['\r', '\n'], " ");
    let needs_quote = single.is_empty()
        || single.starts_with([' ', '\t', '-', '?', ':', '#', '[', ']', '{', '}', '"', '\'', '&', '*', '!', '|', '>', '%', '@', '`'])
        || single.ends_with([' ', '\t'])
        || single.contains(": ")
        || single.contains(" #")
        || matches!(
            single.to_ascii_lowercase().as_str(),
            "true" | "false" | "null" | "yes" | "no" | "~"
        );
    if needs_quote {
        serde_json::to_string(&single).unwrap_or_else(|_| format!("\"{single}\""))
    } else {
        single
    }
}

fn format_entry_markdown(entry: &MemoryEntry) -> String {
    let mut out = String::from("---\n");
    out.push_str(&format!("title: {}\n", yaml_scalar(&entry.meta.title)));
    if !entry.meta.description.is_empty() {
        out.push_str(&format!(
            "description: {}\n",
            yaml_scalar(&entry.meta.description)
        ));
    }
    out.push_str(&format!("mode: {}\n", entry.meta.mode.as_str()));
    if !entry.meta.tags.is_empty() {
        let items: Vec<String> = entry.meta.tags.iter().map(|t| yaml_scalar(t)).collect();
        out.push_str(&format!("tags: [{}]\n", items.join(", ")));
    }
    out.push_str(&format!("updatedAt: {}\n", crate::util::now_rfc3339()));
    out.push_str("---\n\n");
    out.push_str(&entry.content);
    out.push('\n');
    out
}

fn read_entry_file(
    path: &Path,
    scope: MemoryScope,
    project_key: Option<&str>,
    id: &str,
) -> Result<MemoryEntry> {
    let raw = std::fs::read_to_string(path)?;
    let (fm, body) = split_frontmatter(&raw);
    let content = normalize_newlines(body).trim().to_string();
    let title = fm
        .as_deref()
        .and_then(|f| fm_value(f, "title"))
        .or_else(|| extract_heading(&content))
        .unwrap_or_else(|| id.to_string());
    let description = fm
        .as_deref()
        .and_then(|f| fm_value(f, "description"))
        .unwrap_or_else(|| extract_first_line(&content));
    let mode = match fm.as_deref().and_then(|f| fm_value(f, "mode")).as_deref() {
        Some("always") => MemoryMode::Always,
        _ => MemoryMode::OnDemand,
    };
    let tags = fm.as_deref().map(fm_tags).unwrap_or_default();
    let file_mtime = std::fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let updated_at = fm
        .as_deref()
        .and_then(|f| fm_value(f, "updatedAt"))
        .and_then(|s| s.parse::<i64>().ok().or_else(|| crate::util::parse_iso(&s)))
        .unwrap_or(file_mtime);
    Ok(MemoryEntry {
        meta: MemoryMeta {
            id: id.to_string(),
            scope,
            project_key: project_key.map(str::to_string),
            title,
            description,
            mode,
            tags,
            updated_at,
            size: content.len(),
        },
        content,
    })
}

fn extract_heading(body: &str) -> Option<String> {
    for line in body.lines() {
        let l = line.trim();
        if let Some(h) = l.strip_prefix("# ") {
            let h = h.trim();
            if !h.is_empty() {
                return Some(truncate_chars(h, 80));
            }
        }
    }
    None
}

fn extract_first_line(body: &str) -> String {
    let mut heading_fallback = None;
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(h) = trimmed.strip_prefix("# ") {
            if heading_fallback.is_none() {
                let h = h.trim();
                if !h.is_empty() {
                    heading_fallback = Some(truncate_chars(h, 140));
                }
            }
            continue;
        }
        let l = trimmed.trim_start_matches(['#', '-', '*', '>', ' ']).trim();
        if !l.is_empty() {
            return truncate_chars(l, 140);
        }
    }
    heading_fallback.unwrap_or_default()
}

fn compact_always_body(body: &str, max_chars: usize) -> String {
    let lines: Vec<&str> = body
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let joined = lines.join(" | ");
    if joined.chars().count() <= max_chars {
        joined
    } else {
        format!("{}… (call memory_read for full details)", truncate_chars(&joined, max_chars))
    }
}

fn truncate_chars(s: &str, max: usize) -> String {
    let mut it = s.chars();
    let taken: String = it.by_ref().take(max).collect();
    if it.next().is_some() {
        format!("{taken}…")
    } else {
        taken
    }
}

fn escape_xml_attr(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn is_cjk(c: char) -> bool {
    matches!(
        c as u32,
        0x4E00..=0x9FFF | 0x3400..=0x4DBF | 0x3040..=0x30FF | 0xAC00..=0xD7AF
    )
}

fn tokenize_query(q: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut ascii_buf = String::new();
    let mut cjk_buf: Vec<char> = Vec::new();

    let flush_ascii = |buf: &mut String, out: &mut Vec<String>| {
        if !buf.is_empty() {
            let t = buf.to_ascii_lowercase();
            if t.len() >= 2 && !out.contains(&t) {
                out.push(t);
            }
            buf.clear();
        }
    };
    let flush_cjk = |buf: &mut Vec<char>, out: &mut Vec<String>| {
        if buf.len() == 1 {
            let s = buf[0].to_string();
            if !out.contains(&s) {
                out.push(s);
            }
        } else if buf.len() >= 2 {
            for w in buf.windows(2) {
                let s: String = w.iter().collect();
                if !out.contains(&s) {
                    out.push(s);
                }
            }
        }
        buf.clear();
    };

    for ch in q.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' || ch == '.' {
            flush_cjk(&mut cjk_buf, &mut tokens);
            ascii_buf.push(ch);
        } else if is_cjk(ch) {
            flush_ascii(&mut ascii_buf, &mut tokens);
            cjk_buf.push(ch);
        } else {
            flush_ascii(&mut ascii_buf, &mut tokens);
            flush_cjk(&mut cjk_buf, &mut tokens);
        }
    }
    flush_ascii(&mut ascii_buf, &mut tokens);
    flush_cjk(&mut cjk_buf, &mut tokens);

    if tokens.is_empty() {
        let t = q.trim().to_lowercase();
        if !t.is_empty() {
            tokens.push(t);
        }
    }
    tokens
}

fn extract_snippet(content: &str, q_lower: &str, tokens: &[String]) -> String {
    for line in content.lines() {
        let l = line.trim();
        if l.is_empty() {
            continue;
        }
        let ll = l.to_lowercase();
        if ll.contains(q_lower) || tokens.iter().any(|t| ll.contains(t)) {
            return truncate_chars(l, 200);
        }
    }
    extract_first_line(content)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn frontmatter_roundtrip_and_plain_md_fallback() {
        let tmp = tempdir().unwrap();
        let store = MemoryStore::new(Some(tmp.path().to_path_buf()));

        let saved = store
            .save(SaveInput {
                scope: MemoryScope::Global,
                project_key: None,
                cwd: None,
                id: "win-env".into(),
                prev_id: None,
                title: Some("Windows 构建环境: ssh win".into()),
                description: Some("在 Windows 上执行编译和测试时查阅".into()),
                mode: Some(MemoryMode::Always),
                tags: Some(vec!["windows".into(), "build".into()]),
                content: "所有编译都在 ssh win 的 D:\\agent-build 上跑。".into(),
            })
            .unwrap();
        assert_eq!(saved.entry.meta.mode, MemoryMode::Always);
        assert_eq!(saved.entry.meta.tags, vec!["windows", "build"]);

        let loaded = store.get(MemoryScope::Global, None, None, "win-env").unwrap();
        assert_eq!(loaded.meta.title, "Windows 构建环境: ssh win");
        assert_eq!(loaded.meta.description, "在 Windows 上执行编译和测试时查阅");
        assert_eq!(loaded.meta.mode, MemoryMode::Always);
        assert_eq!(loaded.content, "所有编译都在 ssh win 的 D:\\agent-build 上跑。");

        // Plain markdown file dropped manually without frontmatter
        let plain_path = tmp.path().join("global").join("manual-note.md");
        std::fs::write(&plain_path, "# 手写备忘\n\n中继日志用 journalctl 查看。\n").unwrap();
        let loaded_plain = store.get(MemoryScope::Global, None, None, "manual-note").unwrap();
        assert_eq!(loaded_plain.meta.title, "手写备忘");
        assert_eq!(loaded_plain.meta.description, "中继日志用 journalctl 查看。");
        assert_eq!(loaded_plain.meta.mode, MemoryMode::OnDemand);
    }

    #[test]
    fn undo_reverts_create_overwrite_delete_and_move() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let store = MemoryStore::new(Some(tmp.path().join("mem")));

        // 1) Create -> undo deletes it
        let c = store
            .save(SaveInput {
                scope: MemoryScope::Global,
                project_key: None,
                cwd: None,
                id: "pref".into(),
                prev_id: None,
                title: Some("Prefs".into()),
                description: None,
                mode: None,
                tags: None,
                content: "v1".into(),
            })
            .unwrap();
        store.undo(&c.undo_id).unwrap();
        assert!(store.get(MemoryScope::Global, None, None, "pref").is_err());

        // 2) Create -> Overwrite -> undo restores v1
        store
            .save(SaveInput {
                scope: MemoryScope::Global,
                project_key: None,
                cwd: None,
                id: "pref".into(),
                prev_id: None,
                title: Some("Prefs".into()),
                description: None,
                mode: None,
                tags: None,
                content: "v1".into(),
            })
            .unwrap();
        let ov = store
            .save(SaveInput {
                scope: MemoryScope::Global,
                project_key: None,
                cwd: None,
                id: "pref".into(),
                prev_id: None,
                title: Some("Prefs".into()),
                description: None,
                mode: None,
                tags: None,
                content: "v2".into(),
            })
            .unwrap();
        store.undo(&ov.undo_id).unwrap();
        assert_eq!(store.get(MemoryScope::Global, None, None, "pref").unwrap().content, "v1");

        // 3) Delete -> undo restores v1
        let del = store.delete(MemoryScope::Global, None, None, "pref").unwrap();
        assert!(store.get(MemoryScope::Global, None, None, "pref").is_err());
        store.undo(&del.undo_id).unwrap();
        assert_eq!(store.get(MemoryScope::Global, None, None, "pref").unwrap().content, "v1");

        // 4) Move global -> project -> undo restores global
        let mv = store
            .move_entry(
                MemoryScope::Global,
                None,
                "pref",
                MemoryScope::Project,
                None,
                Some(&repo),
                None,
            )
            .unwrap();
        assert!(store.get(MemoryScope::Global, None, None, "pref").is_err());
        assert_eq!(
            store
                .get(MemoryScope::Project, mv.entry.meta.project_key.as_deref(), None, "pref")
                .unwrap()
                .content,
            "v1"
        );
        store.undo(&mv.undo_id).unwrap();
        assert_eq!(store.get(MemoryScope::Global, None, None, "pref").unwrap().content, "v1");
        assert!(store
            .get(MemoryScope::Project, mv.entry.meta.project_key.as_deref(), None, "pref")
            .is_err());
    }

    #[test]
    fn search_handles_cjk_and_english_and_isolates_projects() {
        let tmp = tempdir().unwrap();
        let repo_a = tmp.path().join("repo-a");
        let repo_b = tmp.path().join("repo-b");
        std::fs::create_dir_all(&repo_a).unwrap();
        std::fs::create_dir_all(&repo_b).unwrap();
        let store = MemoryStore::new(Some(tmp.path().join("mem")));

        store
            .save(SaveInput {
                scope: MemoryScope::Global,
                project_key: None,
                cwd: None,
                id: "win-build".into(),
                prev_id: None,
                title: Some("Windows 构建机".into()),
                description: Some("ssh win 工具链".into()),
                mode: None,
                tags: Some(vec!["windows".into()]),
                content: "工具链在 D:\\agent-build".into(),
            })
            .unwrap();
        store
            .save(SaveInput {
                scope: MemoryScope::Project,
                project_key: None,
                cwd: Some(repo_a.clone()),
                id: "relay-ops".into(),
                prev_id: None,
                title: Some("中继服务器运维".into()),
                description: Some("aliyun-sz 上的 yi-relay".into()),
                mode: None,
                tags: Some(vec!["relay".into()]),
                content: "必须用 runuser -u yi-relay 运行".into(),
            })
            .unwrap();
        store
            .save(SaveInput {
                scope: MemoryScope::Project,
                project_key: None,
                cwd: Some(repo_b.clone()),
                id: "secret-b".into(),
                prev_id: None,
                title: Some("B 项目专属".into()),
                description: Some("中继测试".into()),
                mode: None,
                tags: None,
                content: "只在 repo-b 可见".into(),
            })
            .unwrap();

        let hits = store.search("中继运维", None, None, Some(&repo_a), 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "relay-ops");

        let hits_win = store.search("ssh win", None, None, Some(&repo_a), 10);
        assert_eq!(hits_win.len(), 1);
        assert_eq!(hits_win[0].id, "win-build");
    }

    #[test]
    fn secret_filter_catches_real_secrets_but_allows_normal_ops_docs() {
        assert!(detect_secret("-----BEGIN OPENSSH PRIVATE KEY-----\nabc").is_some());
        assert!(detect_secret("use sk-ant-api03-abcdef1234567890abcdef").is_some());
        assert!(detect_secret("token: ghp_1234567890abcdefghij1234567890").is_some());
        assert!(detect_secret("password=SuperSecretValue123!").is_some());

        // Safe docs mentioning placeholders or SHA-256 fingerprints must pass
        let safe_doc = "证书指纹 b702c615008bedfe9a3fd4d231347ebb07bf1712bfe4de8de362643c60481d63\n\
                        密码不写在这里：见用户的密码管理器 / GitHub Secret ANDROID_KEYSTORE_PASSWORD。\n\
                        password=********\n\
                        api_key=<your_key>";
        assert!(detect_secret(safe_doc).is_none());
    }

    #[test]
    fn l0_preview_orders_always_and_index_and_respects_readonly_empty() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("my-app");
        std::fs::create_dir_all(&repo).unwrap();
        let store = MemoryStore::new(Some(tmp.path().join("mem")));

        // Empty + readonly => empty prompt (0 token overhead)
        let ro = MemorySettings {
            enabled: true,
            write_policy: "readonly".into(),
        };
        assert!(store.build_l0_preview(Some(&repo), None, &ro, false).prompt.is_empty());

        // Empty + direct => tiny hint so AI knows memory_save exists
        let def = MemorySettings::default();
        let empty_p = store.build_l0_preview(Some(&repo), None, &def, false);
        assert!(empty_p.prompt.contains("<yi-memory"));
        assert!(empty_p.prompt.contains("memory_save"));

        // With always + on-demand across global + project
        store
            .save(SaveInput {
                scope: MemoryScope::Global,
                project_key: None,
                cwd: None,
                id: "no-linux-build".into(),
                prev_id: None,
                title: Some("No Linux Build".into()),
                description: None,
                mode: Some(MemoryMode::Always),
                tags: None,
                content: "Never compile on Linux; use ssh win.".into(),
            })
            .unwrap();
        store
            .save(SaveInput {
                scope: MemoryScope::Project,
                project_key: None,
                cwd: Some(repo.clone()),
                id: "deploy".into(),
                prev_id: None,
                title: Some("Deploy".into()),
                description: Some("How to deploy my-app".into()),
                mode: Some(MemoryMode::OnDemand),
                tags: None,
                content: "Long deploy playbook...".into(),
            })
            .unwrap();

        let p = store.build_l0_preview(Some(&repo), None, &def, false);
        assert_eq!(p.always_count, 1);
        assert_eq!(p.index_count, 1);
        assert!(p.prompt.contains("[global:no-linux-build] Never compile on Linux"));
        assert!(p.prompt.contains("`deploy`: How to deploy my-app"));
        assert!(!p.prompt.contains("Long deploy playbook"));
    }

    #[test]
    fn pending_queue_approve_and_reject() {
        let tmp = tempdir().unwrap();
        let store = MemoryStore::new(Some(tmp.path().join("mem")));
        let p = store
            .enqueue_pending(PendingMemory {
                pending_id: String::new(),
                action: "save".into(),
                scope: MemoryScope::Global,
                project_key: None,
                project_name: None,
                id: "lang".into(),
                title: "Language".into(),
                description: "Reply language".into(),
                mode: MemoryMode::Always,
                tags: vec![],
                content: "Always reply in Chinese.".into(),
                session_id: Some("s1".into()),
                created_at: 0,
            })
            .unwrap();
        assert_eq!(store.list_pending().len(), 1);
        // Not active until approved
        assert!(store.get(MemoryScope::Global, None, None, "lang").is_err());

        let saved = store
            .resolve_pending(
                &p.pending_id,
                true,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                Some("Always reply in Simplified Chinese.".into()),
            )
            .unwrap();
        assert!(saved.is_some());
        assert_eq!(store.list_pending().len(), 0);
        assert_eq!(
            store.get(MemoryScope::Global, None, None, "lang").unwrap().content,
            "Always reply in Simplified Chinese."
        );
    }

    #[test]
    fn git_worktree_shares_project_key_and_rebind_follows_moved_repo() {
        let tmp = tempdir().unwrap();
        let main_repo = tmp.path().join("demo-repo");
        let wt_repo = tmp.path().join("demo-repo-wt");
        let moved_repo = tmp.path().join("moved-repo");
        std::fs::create_dir_all(&main_repo).unwrap();
        std::fs::create_dir_all(&moved_repo).unwrap();

        let git = |dir: &Path, args: &[&str]| {
            let st = Command::new("git")
                .args(args)
                .current_dir(dir)
                .env("GIT_OPTIONAL_LOCKS", "0")
                .status()
                .unwrap();
            assert!(st.success(), "git {args:?} failed");
        };
        git(&main_repo, &["init"]);
        git(&main_repo, &["config", "user.email", "test@example.com"]);
        git(&main_repo, &["config", "user.name", "Test"]);
        std::fs::write(main_repo.join("README.md"), "hi\n").unwrap();
        git(&main_repo, &["add", "README.md"]);
        git(&main_repo, &["commit", "-m", "init"]);
        git(
            &main_repo,
            &["worktree", "add", "-b", "feat/x", &wt_repo.to_string_lossy()],
        );

        let store = MemoryStore::new(Some(tmp.path().join("mem")));
        let p_main = store.resolve_project(&main_repo).unwrap();
        let p_wt = store.resolve_project(&wt_repo).unwrap();
        assert_eq!(p_main.key, p_wt.key);

        // Save in main repo, read from worktree
        store
            .save(SaveInput {
                scope: MemoryScope::Project,
                project_key: None,
                cwd: Some(main_repo.clone()),
                id: "build".into(),
                prev_id: None,
                title: Some("Build".into()),
                description: None,
                mode: None,
                tags: None,
                content: "build on win".into(),
            })
            .unwrap();
        let from_wt = store
            .get(MemoryScope::Project, None, Some(&wt_repo), "build")
            .unwrap();
        assert_eq!(from_wt.content, "build on win");

        // Rebind to a moved directory and confirm resolution points to the same key
        let rebound = store.rebind_project(&p_main.key, &moved_repo).unwrap();
        assert_eq!(rebound.key, p_main.key);
        let resolved_moved = store.resolve_project(&moved_repo).unwrap();
        assert_eq!(resolved_moved.key, p_main.key);
    }
}
