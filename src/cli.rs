//! Command-line tools: the clipboard command, --wake, setup-ssh and the live check.
use crate::backend;
use crate::clipboards::{MAX_BYTES, from_bytes};
use crate::config::{self, Node};
use crate::util::{self, Res, secs};
use serde_json::Value;
use std::io::{Read, Write};

fn nodes_or_exit() -> Result<Vec<Node>, i32> {
    match config::load_nodes() {
        Ok(nodes) if !nodes.is_empty() => Ok(nodes),
        Ok(_) => {
            eprintln!(
                "No machines configured yet. Copy hosts.example.json to {} and edit it (one entry per SSH alias), or run install.sh again.",
                config::config_path().display()
            );
            Err(1)
        }
        Err(error) => {
            eprintln!("{error}");
            Err(1)
        }
    }
}

const CLIPBOARD_USAGE: &str = "usage: archfleet-clipboard get <host> [--json] | set <host> [--type text/plain|image/png] | copy <source> <destination>

Read or copy home-network clipboards over SSH. Plain text and PNG; 8 MiB limit.
  get    print text exactly; --json includes image data and format metadata
  set    read the new content from stdin (never put clipboard data on the command line)
  copy   copy the source clipboard to the destination, keeping text or PNG";

/// `archfleet-clipboard get|set|copy`.
pub fn clipboard(argv: &[String]) -> i32 {
    let nodes = match nodes_or_exit() {
        Ok(n) => n,
        Err(code) => return code,
    };
    match clipboard_command(&nodes, argv) {
        Ok(()) => 0,
        Err(error) if error.starts_with("usage:") => {
            eprintln!("{error}");
            2
        }
        Err(error) => {
            eprintln!("Clipboard: {error}");
            1
        }
    }
}

fn clipboard_command(nodes: &[Node], argv: &[String]) -> Res<()> {
    let aliases: Vec<&str> = nodes.iter().map(|n| n.alias.as_str()).collect();
    let node = |alias: Option<&String>| -> Res<&Node> {
        let alias = alias.ok_or(CLIPBOARD_USAGE)?;
        config::find(nodes, alias).ok_or_else(|| format!("usage: unknown host {alias:?} (choose from {})", aliases.join(", ")))
    };
    match argv.first().map(String::as_str) {
        Some("get") => {
            let value = backend::clipboard(node(argv.get(1))?)?;
            if argv.iter().any(|a| a == "--json") {
                println!("{value}");
            } else if value["kind"] == "text" {
                let mut out = std::io::stdout();
                let _ = out.write_all(value["text"].as_str().unwrap_or("").as_bytes());
                let _ = out.flush();
            } else if value["kind"] != "empty" {
                return Err("This clipboard contains non-text data. Use get --json or copy to another host.".into());
            }
            Ok(())
        }
        Some("set") => {
            let target = node(argv.get(1))?;
            let mime = match argv.iter().position(|a| a == "--type") {
                Some(i) => argv.get(i + 1).map(String::as_str).unwrap_or(""),
                None => "text/plain",
            };
            if mime != "text/plain" && mime != "image/png" {
                return Err(CLIPBOARD_USAGE.into());
            }
            let mut data = Vec::new();
            std::io::stdin().take(MAX_BYTES as u64 + 1).read_to_end(&mut data).map_err(|e| e.to_string())?;
            backend::set_clipboard(target, &from_bytes(&data, mime)?)?;
            eprintln!("Clipboard set on {}.", target.alias);
            Ok(())
        }
        Some("copy") => {
            let (source, destination) = (node(argv.get(1))?, node(argv.get(2))?);
            let value = backend::clipboard(source)?;
            if value["kind"] != "text" && value["kind"] != "image" {
                return Err("The source clipboard is empty or has an unsupported format.".into());
            }
            backend::set_clipboard(destination, &value)?;
            eprintln!("Copied clipboard from {} to {}.", source.alias, destination.alias);
            Ok(())
        }
        _ => Err(CLIPBOARD_USAGE.into()),
    }
}

/// `archfleet --wake <alias>`: send the magic packet and exit.
pub fn wake(alias: &str) -> i32 {
    let nodes = match nodes_or_exit() {
        Ok(n) => n,
        Err(code) => return code,
    };
    let Some(node) = nodes.iter().find(|n| !alias.is_empty() && (n.alias == alias || n.hostname == alias || n.label.to_lowercase() == alias)) else {
        eprintln!("Usage: archfleet --wake <{}>", nodes.iter().map(|n| n.alias.as_str()).collect::<Vec<_>>().join("|"));
        return 2;
    };
    if node.mac.is_none() {
        eprintln!("{} has no wired MAC (\"mac\") in hosts.json, so it can't be woken over the network.", node.hostname);
        return 1;
    }
    match backend::wake(node) {
        Ok(message) => {
            println!("{message}");
            0
        }
        Err(error) => {
            eprintln!("{error}");
            1
        }
    }
}

// ---------- setup-ssh ----------

fn ssh_dir() -> std::path::PathBuf {
    util::home().join(".ssh")
}

#[cfg(unix)]
fn chmod(path: &std::path::Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
}
#[cfg(not(unix))]
fn chmod(_: &std::path::Path, _: u32) {}

/// SSH aliases for the machines in hosts.json, a dedicated key, and a check that they work.
/// The aliases go in ~/.ssh/config.d/archfleet.conf, included from the top of ~/.ssh/config, and use
/// their own key ~/.ssh/id_ed25519_archfleet (no passphrase, so Archfleet can log in unattended;
/// only public keys ever leave the machine).
pub fn setup_ssh(argv: &[String]) -> i32 {
    let nodes = match nodes_or_exit() {
        Ok(n) => n,
        Err(code) => return code,
    };
    let others: Vec<&Node> = nodes.iter().filter(|n| !n.is_local()).collect();
    if others.len() == nodes.len() {
        println!("note: no entry in hosts.json has this machine's hostname ({}); treating all as remote.", util::local_hostname());
    }
    if argv.iter().any(|a| a == "--check") {
        return ssh_check(&others);
    }
    let result = (|| -> Res<()> {
        let public_key = make_key()?;
        let include_file = write_ssh_config(&nodes)?;
        println!("Wrote SSH aliases for {} to {}", nodes.iter().map(|n| n.alias.as_str()).collect::<Vec<_>>().join(", "), include_file.display());
        authorise_hints(&others, &public_key, argv.iter().any(|a| a == "--copy"));
        println!(
            "\nThe Linux machines need sshd running (sudo systemctl enable --now sshd).\nWhen every machine has run this and been authorised: archfleet setup-ssh --check"
        );
        Ok(())
    })();
    match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("{error}");
            1
        }
    }
}

fn make_key() -> Res<String> {
    let dir = ssh_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    chmod(&dir, 0o700);
    let key = dir.join("id_ed25519_archfleet");
    if !key.exists() {
        let user = std::env::var("USER").or_else(|_| std::env::var("USERNAME")).unwrap_or_default();
        let comment = format!("{user}@{}:archfleet", util::local_hostname());
        let argv = vec![
            backend::tool("ssh-keygen"),
            "-q".into(),
            "-t".into(),
            "ed25519".into(),
            "-N".into(),
            String::new(),
            "-f".into(),
            key.to_string_lossy().to_string(),
            "-C".into(),
            comment,
        ];
        let out = util::run(&argv, None, secs(30.0), None)?;
        if !out.ok() {
            return Err(format!("ssh-keygen failed: {}", out.both()));
        }
    }
    chmod(&key, 0o600);
    std::fs::read_to_string(key.with_extension("pub")).map(|k| k.trim().to_string()).map_err(|e| e.to_string())
}

fn write_ssh_config(nodes: &[Node]) -> Res<std::path::PathBuf> {
    let dir = ssh_dir();
    let include_file = dir.join("config.d").join("archfleet.conf");
    std::fs::create_dir_all(include_file.parent().unwrap()).map_err(|e| e.to_string())?;
    chmod(include_file.parent().unwrap(), 0o700);
    let mut blocks = vec![format!("# Written by archfleet setup-ssh from {}. Run it again after editing hosts.json.\n", config::config_path().display())];
    for node in nodes {
        let names = if node.alias == node.hostname { node.alias.clone() } else { format!("{} {}", node.alias, node.hostname) };
        let mut lines = vec![format!("Host {names}"), format!("    HostName {}", if node.address.is_empty() { &node.hostname } else { &node.address })];
        if !node.user.is_empty() {
            lines.push(format!("    User {}", node.user));
        }
        for line in [
            "IdentityFile ~/.ssh/id_ed25519_archfleet",
            "IdentitiesOnly yes",
            "StrictHostKeyChecking accept-new",
            "ForwardAgent no",
            "ConnectTimeout 5",
            "ServerAliveInterval 30",
            "ServerAliveCountMax 3",
        ] {
            lines.push(format!("    {line}"));
        }
        blocks.push(lines.join("\n") + "\n");
    }
    std::fs::write(&include_file, blocks.join("\n")).map_err(|e| e.to_string())?;
    let config = dir.join("config");
    let include = "Include ~/.ssh/config.d/archfleet.conf";
    let old = std::fs::read_to_string(&config).unwrap_or_default();
    if !old.lines().any(|l| l == include) {
        if !old.is_empty() {
            let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
            std::fs::write(dir.join(format!("config.backup-{stamp}")), &old).map_err(|e| e.to_string())?;
        }
        // Include must come before any Host block, or it only applies inside that block.
        std::fs::write(&config, format!("{include}\n\n{old}")).map_err(|e| e.to_string())?;
    }
    chmod(&include_file, 0o600);
    chmod(&config, 0o600);
    Ok(include_file)
}

fn authorise_hints(nodes: &[&Node], public_key: &str, copy: bool) {
    let public = ssh_dir().join("id_ed25519_archfleet.pub");
    println!("\nThis machine's Archfleet public key ({}):\n  {public_key}\n", public.display());
    for node in nodes {
        if node.linux() {
            if copy && util::which("ssh-copy-id").is_some() {
                let who = if node.user.is_empty() { "your" } else { &node.user };
                println!("--> {}: ssh-copy-id (enter {who} password on {} once)", node.alias, node.hostname);
                let _ = std::process::Command::new("ssh-copy-id").arg("-i").arg(&public).arg(&node.alias).status();
            } else {
                println!("  {:<16} ssh-copy-id -i {} {}", node.alias, public.display(), node.alias);
            }
        } else {
            println!(
                "  {:<16} on {}, in an admin PowerShell, from the Archfleet folder:\n                   .\\windows\\setup-ssh-server.ps1 -PublicKeyFile <a copy of id_ed25519_archfleet.pub>",
                node.alias, node.hostname
            );
        }
    }
}

fn ssh_check(nodes: &[&Node]) -> i32 {
    let mut failed = false;
    for node in nodes {
        let result = (|| -> Res<String> {
            let mut argv = vec![backend::tool("ssh"), "-o".into(), "BatchMode=yes".into(), node.alias.clone(), "whoami".into()];
            let who = util::run(&argv, None, secs(15.0), None)?;
            if !who.ok() {
                return Err(if who.err().is_empty() { "ssh failed".into() } else { who.err() });
            }
            let login = who.text();
            let name = login.rsplit('\\').next().unwrap_or("").to_lowercase();
            if !node.user.is_empty() && name != node.user.to_lowercase() {
                return Err(format!("logged in as {login}, expected {}", node.user));
            }
            let stamp = format!("archfleet-ssh-test-{}-{}.tmp", std::process::id(), util::now_secs() as u64);
            let temp = std::env::temp_dir();
            let (up, down) = (temp.join(format!("{stamp}.up")), temp.join(format!("{stamp}.down")));
            let payload = format!("Archfleet connection test {stamp}");
            std::fs::write(&up, &payload).map_err(|e| e.to_string())?;
            let slash = |p: &std::path::Path| p.to_string_lossy().replace('\\', "/");
            let batch = format!("put \"{}\" \"{stamp}\"\nget \"{stamp}\" \"{}\"\nrm \"{stamp}\"\n", slash(&up), slash(&down));
            argv = vec![backend::tool("sftp"), "-o".into(), "BatchMode=yes".into(), "-b".into(), "-".into(), node.alias.clone()];
            let sftp = util::run(&argv, Some(batch.as_bytes()), secs(25.0), None);
            let back = std::fs::read_to_string(&down).unwrap_or_default();
            let _ = std::fs::remove_file(&up);
            let _ = std::fs::remove_file(&down);
            let sftp = sftp?;
            if !sftp.ok() {
                return Err(sftp.err());
            }
            if back != payload {
                return Err("SFTP round trip came back different".into());
            }
            Ok("ok  ssh + sftp round trip".into())
        })();
        let line = result.unwrap_or_else(|error| {
            failed = true;
            format!("FAILED: {error}")
        });
        println!("  {:<16} {line}", node.alias);
    }
    failed as i32
}

/// Read-only check against every machine: status, Hyprland discovery and a maintenance read.
pub fn live_check() -> i32 {
    let nodes = match nodes_or_exit() {
        Ok(n) => n,
        Err(code) => return code,
    };
    let statuses: Vec<(String, Value)> = std::thread::scope(|scope| {
        let handles: Vec<_> = nodes.iter().map(|n| scope.spawn(move || (n.alias.clone(), backend::status(n)))).collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut failed = false;
    for (alias, status) in &statuses {
        if status["online"] == true {
            println!("{alias:<16} online   status in {} ms", status["elapsed_ms"]);
        } else {
            failed = true;
            println!("{alias:<16} OFFLINE  {}", status["error"].as_str().unwrap_or(""));
        }
    }
    for node in nodes.iter().filter(|n| n.linux()) {
        if !statuses.iter().any(|(a, s)| a == &node.alias && s["online"] == true) {
            continue;
        }
        match backend::call(node, "hypr", Value::Null) {
            Ok(data) => println!(
                "{:<16} Hyprland {} · {} monitor(s) · {} windows",
                node.alias,
                data["version"]["version"].as_str().unwrap_or("?"),
                util::ja(&data, "monitors").len(),
                util::ja(&data, "clients").len()
            ),
            Err(error) => {
                failed = true;
                println!("{:<16} Hyprland: {error}", node.alias);
            }
        }
        if let Err(error) = backend::call(node, "storage", Value::Null).and_then(|r| if r["code"] == 0 { Ok(()) } else { Err(util::jtext(&r["text"])) }) {
            failed = true;
            println!("{:<16} storage: {error}", node.alias);
        }
    }
    if failed {
        println!("Some checks failed.");
        1
    } else {
        println!("Live status, Hyprland discovery and maintenance read checks passed.");
        0
    }
}
