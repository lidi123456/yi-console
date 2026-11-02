//! Parse ~/.ssh/config and import it as host config candidates (common fields only, wildcard Host entries ignored).

use serde::Serialize;

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SshConfigEntry {
    pub alias: String,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub identity_file: String,
    pub proxy_jump: String,
}

pub fn parse(text: &str) -> Vec<SshConfigEntry> {
    let mut out: Vec<SshConfigEntry> = vec![];
    let mut cur: Vec<SshConfigEntry> = vec![];
    let mut in_match = false;
    let flush = |cur: &mut Vec<SshConfigEntry>, out: &mut Vec<SshConfigEntry>| {
        out.append(cur);
    };
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (k, v) = match line.split_once(|c: char| c.is_whitespace() || c == '=') {
            Some((k, v)) => (k.trim().to_lowercase(), v.trim().trim_start_matches('=').trim()),
            None => continue,
        };
        let v = v.trim_matches('"').to_string();
        match k.as_str() {
            "host" => {
                flush(&mut cur, &mut out);
                in_match = false;
                for alias in v.split_whitespace() {
                    if alias.contains('*') || alias.contains('?') || alias.starts_with('!') {
                        continue;
                    }
                    cur.push(SshConfigEntry {
                        alias: alias.to_string(),
                        host: alias.to_string(),
                        port: 22,
                        ..Default::default()
                    });
                }
            }
            "match" => {
                flush(&mut cur, &mut out);
                in_match = true;
            }
            _ if in_match => {}
            "hostname" => cur.iter_mut().for_each(|e| e.host = v.clone()),
            "port" => cur
                .iter_mut()
                .for_each(|e| e.port = v.parse().unwrap_or(22)),
            "user" => cur.iter_mut().for_each(|e| e.user = v.clone()),
            "identityfile" => cur.iter_mut().for_each(|e| {
                if e.identity_file.is_empty() {
                    e.identity_file = expand_home(&v);
                }
            }),
            "proxyjump" => cur.iter_mut().for_each(|e| e.proxy_jump = v.clone()),
            _ => {}
        }
    }
    flush(&mut cur, &mut out);
    out
}

fn expand_home(p: &str) -> String {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(h) = dirs::home_dir() {
            return h.join(rest).display().to_string();
        }
    }
    p.to_string()
}

pub fn load_default() -> Vec<SshConfigEntry> {
    let Some(home) = dirs::home_dir() else {
        return vec![];
    };
    std::fs::read_to_string(home.join(".ssh").join("config"))
        .map(|s| parse(&s))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic() {
        let cfg = "Host *\n  ServerAliveInterval 30\n\nHost dev dev2\n  HostName 10.0.0.5\n  Port 2222\n  User alice\n  IdentityFile /k/id\n  ProxyJump bastion\n\nHost bastion\n  HostName b.example.com\n";
        let v = parse(cfg);
        assert_eq!(v.len(), 3);
        assert_eq!(v[0].alias, "dev");
        assert_eq!(v[1].alias, "dev2");
        assert_eq!(v[0].host, "10.0.0.5");
        assert_eq!(v[0].port, 2222);
        assert_eq!(v[0].proxy_jump, "bastion");
        assert_eq!(v[2].host, "b.example.com");
        assert_eq!(v[2].port, 22);
    }
}
