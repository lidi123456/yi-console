//! Provider-neutral MCP configuration. Never rewrites a CLI's global configuration files.
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use yi_proto::{DaemonSettings, McpServerConfig, McpTransport, ToolKind};

pub const DESKTOP_SERVER: &str = "yi_computer_use";
const MASK: &str = "********";

pub fn supported(tool: ToolKind) -> bool {
    matches!(
        tool,
        ToolKind::Claude
            | ToolKind::Codex
            | ToolKind::Cursor
            | ToolKind::Kiro
            | ToolKind::Qoder
            | ToolKind::Codebuddy
    )
}

pub fn validate(settings: &DaemonSettings) -> Result<()> {
    let mut names = HashSet::new();
    for server in &settings.mcp_servers {
        if server.name.is_empty()
            || server.name.len() > 64
            || !server
                .name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            bail!("MCP 名称只能包含字母、数字、下划线和短横线，长度为 1–64");
        }
        if server.name == DESKTOP_SERVER {
            bail!("MCP 名称 yi_computer_use 为内置操作电脑保留");
        }
        if !names.insert(&server.name) {
            bail!("MCP 名称重复：{}", server.name);
        }
        if server.tools.iter().any(|&tool| !supported(tool)) {
            bail!("MCP 只能分配给 AI 工具");
        }
        match server.transport {
            McpTransport::Stdio if server.command.trim().is_empty() => {
                bail!("MCP stdio 服务缺少启动命令：{}", server.name)
            }
            McpTransport::Http
                if !(server.url.starts_with("https://") || server.url.starts_with("http://")) =>
            {
                bail!(
                    "MCP HTTP 地址必须以 http:// 或 https:// 开头：{}",
                    server.name
                )
            }
            _ => {}
        }
        if server.command.contains('\0')
            || server.args.iter().any(|v| v.contains('\0'))
            || server.url.contains('\0')
        {
            bail!("MCP 配置包含无效字符：{}", server.name);
        }
        for (key, value) in server.env.iter().chain(server.headers.iter()) {
            if key.trim().is_empty() || key.contains(['\0', '\r', '\n']) || value.contains('\0') {
                bail!("MCP 环境变量或请求头无效：{}", server.name);
            }
        }
        if server.env.keys().any(|key| key.contains('='))
            || server.headers.values().any(|v| v.contains(['\r', '\n']))
        {
            bail!("MCP 环境变量或请求头无效：{}", server.name);
        }
    }
    Ok(())
}

pub fn redact(mut settings: DaemonSettings) -> DaemonSettings {
    for server in &mut settings.mcp_servers {
        for value in server.env.values_mut().chain(server.headers.values_mut()) {
            *value = MASK.into();
        }
    }
    settings
}

pub fn keep_secrets(settings: &mut DaemonSettings, old: &DaemonSettings) -> Result<()> {
    for server in &mut settings.mcp_servers {
        let previous = old.mcp_servers.iter().find(|s| s.name == server.name);
        for (values, previous) in [
            (&mut server.env, previous.map(|s| &s.env)),
            (&mut server.headers, previous.map(|s| &s.headers)),
        ] {
            for (key, value) in values {
                if value == MASK {
                    *value = previous.and_then(|p| p.get(key)).cloned().ok_or_else(|| {
                        anyhow::anyhow!("MCP 密钥占位符没有原值，请重新填写：{}", server.name)
                    })?;
                }
            }
        }
    }
    Ok(())
}

pub fn selected(settings: &DaemonSettings, tool: ToolKind) -> Vec<McpServerConfig> {
    if !supported(tool) {
        return vec![];
    }
    settings
        .mcp_servers
        .iter()
        .filter(|server| {
            server.enabled && (server.tools.is_empty() || server.tools.contains(&tool))
        })
        .cloned()
        .collect()
}

pub fn desktop(config: Value) -> Result<McpServerConfig> {
    Ok(serde_json::from_value(
        json!({"name":DESKTOP_SERVER,"command":config["command"],"args":config["args"]}),
    )?)
}

/// Cache identity only; the digest is not used as an authorization credential.
pub fn fingerprint(servers: &[McpServerConfig]) -> String {
    format!(
        "{:x}",
        md5::compute(serde_json::to_vec(servers).unwrap_or_default())
    )
}

fn cli_config(server: &McpServerConfig, codex: bool) -> Value {
    match server.transport {
        McpTransport::Stdio => {
            json!({"command":server.command,"args":server.args,"env":server.env})
        }
        McpTransport::Http if codex => json!({"url":server.url,"http_headers":server.headers}),
        McpTransport::Http => json!({"type":"http","url":server.url,"headers":server.headers}),
    }
}

fn toml(value: &Value) -> String {
    match value {
        Value::Object(map) => format!(
            "{{ {} }}",
            map.iter()
                .map(|(key, value)| format!("{} = {}", json!(key), toml(value)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Array(items) => format!(
            "[{}]",
            items.iter().map(toml).collect::<Vec<_>>().join(", ")
        ),
        _ => value.to_string(),
    }
}

/// CLI adapters receive process arguments; all four ACP adapters receive the same session definitions.
pub fn inject(tool: ToolKind, servers: &[McpServerConfig], args: &mut Vec<String>) -> Vec<Value> {
    if servers.is_empty() {
        return vec![];
    }
    match tool {
        ToolKind::Claude => {
            let servers: BTreeMap<_, _> = servers.iter().map(|s| (&s.name, cli_config(s, false))).collect();
            let config = json!({"mcpServers":servers}).to_string();
            if let Some(index) = args.iter().rposition(|arg| arg == "--mcp-config") { args.insert(index + 1, config); }
            else { args.extend(["--mcp-config".into(), config]); }
            vec![]
        }
        ToolKind::Codex => {
            for server in servers { args.extend(["-c".into(), format!("mcp_servers.{}={}", server.name, toml(&cli_config(server, true)))]); }
            vec![]
        }
        ToolKind::Cursor | ToolKind::Kiro | ToolKind::Qoder | ToolKind::Codebuddy => servers.iter().map(|server| match server.transport {
            McpTransport::Stdio => json!({"name":server.name,"command":server.command,"args":server.args,"env":server.env.iter().map(|(name,value)|json!({"name":name,"value":value})).collect::<Vec<_>>()}),
            McpTransport::Http => json!({"name":server.name,"type":"http","url":server.url,"headers":server.headers.iter().map(|(name,value)|json!({"name":name,"value":value})).collect::<Vec<_>>()}),
        }).collect(),
        _ => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> McpServerConfig {
        serde_json::from_value(json!({"name":"probe","command":"C:\\Program Files\\probe.exe","args":["a b"],"env":{"TOKEN":"secret"}})).unwrap()
    }
    #[test]
    fn six_providers_receive_the_same_server_and_existing_args_survive() {
        for tool in [
            ToolKind::Claude,
            ToolKind::Codex,
            ToolKind::Cursor,
            ToolKind::Kiro,
            ToolKind::Qoder,
            ToolKind::Codebuddy,
        ] {
            let mut args = vec!["--existing".into()];
            let servers = inject(tool, &[fixture()], &mut args);
            assert_eq!(args[0], "--existing");
            if matches!(tool, ToolKind::Claude | ToolKind::Codex) {
                assert!(args.join(" ").contains("probe"));
            } else {
                assert_eq!(
                    servers[0]["env"][0],
                    json!({"name":"TOKEN","value":"secret"})
                );
                assert_eq!(servers[0]["args"][0], "a b");
            }
        }
    }
    #[test]
    fn reserved_names_duplicates_and_missing_secrets_are_rejected() {
        let mut settings = DaemonSettings::default();
        settings.mcp_servers = vec![fixture()];
        validate(&settings).unwrap();
        settings.mcp_servers.push(fixture());
        assert!(validate(&settings).is_err());
        settings.mcp_servers = vec![fixture()];
        settings.mcp_servers[0].name = DESKTOP_SERVER.into();
        assert!(validate(&settings).is_err());
        settings.mcp_servers[0] = fixture();
        let mut hidden = redact(settings.clone());
        assert_eq!(hidden.mcp_servers[0].env["TOKEN"], MASK);
        keep_secrets(&mut hidden, &settings).unwrap();
        assert_eq!(hidden.mcp_servers[0].env["TOKEN"], "secret");
        assert_eq!(settings.mcp_servers[0].env["TOKEN"], "secret");
    }
    #[test]
    fn disabled_and_unselected_servers_are_never_spawned() {
        let mut settings = DaemonSettings::default();
        let mut s = fixture();
        s.tools = vec![ToolKind::Kiro];
        settings.mcp_servers = vec![s];
        assert!(selected(&settings, ToolKind::Claude).is_empty());
        assert_eq!(selected(&settings, ToolKind::Kiro).len(), 1);
        settings.mcp_servers[0].enabled = false;
        assert!(selected(&settings, ToolKind::Kiro).is_empty());
        assert!(selected(&settings, ToolKind::Shell).is_empty());
    }
}
