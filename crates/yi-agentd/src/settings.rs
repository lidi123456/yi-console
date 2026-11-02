use yi_proto::DaemonSettings;

pub fn load() -> DaemonSettings {
    std::fs::read_to_string(crate::paths::settings_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save(s: &DaemonSettings) -> anyhow::Result<()> {
    crate::paths::ensure_base()?;
    let path = crate::paths::settings_path();
    std::fs::write(&path, serde_json::to_string_pretty(s)?)?;
    crate::util::restrict_file(&path);
    Ok(())
}
