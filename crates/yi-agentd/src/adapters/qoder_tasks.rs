use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use yi_proto::PlanEntry;

/// Resolve the directory used by the child, including an isolated account's environment override.
pub(crate) fn config_dir(cwd: &Path, env: &BTreeMap<String, String>, env_unset: &[String]) -> PathBuf {
    const KEY: &str = "QODER_CONFIG_DIR";
    let dir = env.get(KEY).map(PathBuf::from).or_else(|| {
        if env_unset.iter().any(|key| key == KEY) {
            return None;
        }
        std::env::var_os(KEY).map(PathBuf::from)
    });
    dir.filter(|p| !p.as_os_str().is_empty())
        .map(|p| cwd.join(p))
        .unwrap_or_else(|| crate::paths::home().join(".qoder"))
}

pub(crate) fn task_dir(config_dir: &Path, session_id: &str) -> Option<PathBuf> {
    uuid::Uuid::parse_str(session_id).ok()?;
    Some(config_dir.join("tasks").join(session_id))
}

#[derive(serde::Deserialize)]
struct Task {
    subject: String,
    status: String,
}

/// Read Qoder's per-session task files in numeric order. Retry on a partial read rather than reporting a deletion.
pub(crate) fn read(dir: &Path) -> Option<Vec<PlanEntry>> {
    let mut entries = BTreeMap::new();
    for file in fs::read_dir(dir).ok()? {
        let file = file.ok()?;
        let name = file.file_name();
        let Some(id) = name.to_str().and_then(|s| s.strip_suffix(".json")).and_then(|s| s.parse::<u64>().ok()) else {
            continue;
        };
        if !file.file_type().ok()?.is_file() {
            continue;
        }
        let task: Task = serde_json::from_slice(&fs::read(file.path()).ok()?).ok()?;
        entries.insert(id, PlanEntry { content: format!("#{id} {}", task.subject), status: task.status });
    }
    Some(entries.into_values().collect())
}
