//! The machines in hosts.json and small bits of config around them.
use crate::util::{self, Res};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default)]
pub struct Node {
    pub alias: String,
    pub label: String,
    pub hostname: String,
    pub address: String,
    pub user: String,
    pub os: String,
    pub mac: Option<String>,
    /// Path of archfleet on that machine (a `.py` path means the old Python helper).
    pub helper: Option<String>,
    pub clipboard_client: Option<String>,
    pub screentime_command: Option<String>,
    pub lilin: bool,
    pub vpn: Map<String, Value>,
}

impl Node {
    pub fn linux(&self) -> bool {
        self.os == "linux"
    }
    pub fn is_local(&self) -> bool {
        self.hostname.eq_ignore_ascii_case(util::local_hostname())
    }
    pub fn vpn_str(&self, key: &str) -> Option<&str> {
        self.vpn.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
    }
    pub fn clipboard_client(&self) -> String {
        self.clipboard_client.clone().unwrap_or_else(|| format!("C:/Users/{}/AppData/Local/archfleet/clipboard-client.ps1", self.user))
    }
}

pub fn config_path() -> PathBuf {
    match std::env::var_os("ARCHFLEET_HOSTS").filter(|v| !v.is_empty()) {
        Some(path) => PathBuf::from(path),
        None => util::home().join(".config").join("archfleet").join("hosts.json"),
    }
}

fn text(obj: &Map<String, Value>, key: &str) -> Option<String> {
    obj.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string)
}

pub fn parse_nodes(json: &str) -> Res<Vec<Node>> {
    let list: Vec<Value> = serde_json::from_str(json).map_err(|e| format!("hosts.json: {e}"))?;
    let mut nodes = Vec::new();
    for item in list {
        let Some(obj) = item.as_object() else { return Err("hosts.json: every entry must be an object".into()) };
        let alias = text(obj, "alias").ok_or("hosts.json: an entry has no \"alias\"")?;
        let hostname = text(obj, "hostname").unwrap_or_else(|| alias.clone());
        nodes.push(Node {
            label: text(obj, "label").unwrap_or_else(|| hostname.clone()),
            hostname,
            address: text(obj, "address").unwrap_or_default(),
            user: text(obj, "user").unwrap_or_default(),
            os: text(obj, "os").unwrap_or_else(|| "linux".into()),
            mac: text(obj, "mac"),
            helper: text(obj, "helper"),
            clipboard_client: text(obj, "clipboard_client"),
            screentime_command: text(obj, "screentime_command"),
            lilin: obj.get("lilin").is_some_and(util::truthy),
            vpn: obj.get("vpn").and_then(Value::as_object).cloned().unwrap_or_default(),
            alias,
        });
    }
    Ok(nodes)
}

/// The machines in hosts.json (see hosts.example.json). Empty when there is none yet.
pub fn load_nodes() -> Res<Vec<Node>> {
    load_from(&config_path())
}

pub fn load_from(path: &Path) -> Res<Vec<Node>> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse_nodes(&text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

pub fn find<'a>(nodes: &'a [Node], alias: &str) -> Option<&'a Node> {
    nodes.iter().find(|n| n.alias == alias)
}

fn ipv4_prefix(address: &str) -> Option<String> {
    let parts: Vec<&str> = address.split('.').collect();
    (parts.len() == 4 && parts.iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))).then(|| parts[..3].join("."))
}

/// '192.168.1.0/24' when every machine's address shares one /24, else just 'LAN'.
pub fn lan_label(nodes: &[Node]) -> String {
    let mut prefixes: Vec<String> = nodes.iter().filter_map(|n| ipv4_prefix(&n.address)).collect();
    prefixes.sort();
    prefixes.dedup();
    if prefixes.len() == 1 { format!("{}.0/24", prefixes[0]) } else { "LAN".into() }
}

pub fn read_env(path: &Path) -> Vec<(String, String)> {
    let Ok(bytes) = std::fs::read(path) else { return Vec::new() };
    String::from_utf8_lossy(&bytes)
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .filter_map(|line| {
            let (key, value) = line.split_once('=')?;
            Some((key.trim().to_string(), value.trim().trim_matches(|c| c == '"' || c == '\'').to_string()))
        })
        .collect()
}

fn env_get<'a>(env: &'a [(String, String)], key: &str) -> Option<&'a str> {
    env.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v.as_str()).filter(|v| !v.is_empty())
}

/// Lilin's address and House token: lilin-house's config on the Linux machines, Lilin's own
/// .env when Archfleet runs on the server. None when neither is there.
pub fn lilin_config() -> Option<(String, String)> {
    let env = read_env(&util::home().join(".config").join("lilin-house.env"));
    if let (Some(url), Some(token)) = (env_get(&env, "LILIN_URL"), env_get(&env, "HOUSE_TOKEN")) {
        return Some((url.trim_end_matches('/').to_string(), token.to_string()));
    }
    let server = std::env::var("LILIN_ENV").unwrap_or_else(|_| r"C:\lilin\.env".into());
    let env = read_env(Path::new(&server));
    let token = env_get(&env, "HOUSE_TOKEN")?;
    Some((format!("http://127.0.0.1:{}", env_get(&env, "PORT").unwrap_or("8080")), token.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_lan() {
        let nodes = parse_nodes(
            r#"[{"alias":"a","address":"10.0.0.2","mac":"00:11:22:33:44:55"},
                {"alias":"b","hostname":"bee","label":"Bee","os":"windows","address":"10.0.0.3","lilin":true,
                 "vpn":{"restart":"x"}}]"#,
        )
        .unwrap();
        assert_eq!(nodes[0].hostname, "a");
        assert_eq!(nodes[0].label, "a");
        assert!(nodes[0].linux());
        assert!(!nodes[1].linux() && nodes[1].lilin);
        assert_eq!(nodes[1].vpn_str("restart"), Some("x"));
        assert_eq!(lan_label(&nodes), "10.0.0.0/24");
    }
}
