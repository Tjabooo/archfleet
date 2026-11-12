//! Talking to the machines: the helper over SSH (or in-process for this machine), PowerShell on
//! Windows, SCP, Wake-on-LAN, the link test, Lilin's load report and the Arch news feed.
use crate::config::Node;
use crate::util::{self, Res, args, jb, js, secs};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

pub const DEFAULT_HELPER: &str = "~/.local/share/archfleet/archfleet";
const NEWS_FEED: &str = "https://archlinux.org/feeds/news/";
pub const SPEED_BYTES: usize = 64 * 1024 * 1024;
/// SSH logins don't have ~/.local/bin on PATH. hosts.json can override it per machine.
const SCREENTIME_COMMAND: &str = "PATH=\"$HOME/.local/bin:$PATH\" screentime today --json";
const WINDOWS_STATUS: &str = include_str!("../windows/status.ps1");
const WINDOWS_PROCESSES: &str = include_str!("../windows/processes.ps1");

/// Windows' own OpenSSH (Git Bash's ssh, often first on PATH, doesn't read the aliases).
pub fn tool(name: &str) -> String {
    if cfg!(windows) {
        let path = format!(r"C:\Windows\System32\OpenSSH\{name}.exe");
        if std::path::Path::new(&path).exists() {
            return path;
        }
    }
    name.to_string()
}

/// `ssh -T -o BatchMode=yes [extra] <alias>`. OpenSSH 10 warns on every login to an older sshd
/// ("not using a post-quantum key exchange"); WarnWeakCrypto=no keeps that out of results.
pub fn ssh(node: &Node, extra: &[&str]) -> Vec<String> {
    let mut argv = vec![tool("ssh")];
    if !cfg!(windows) {
        argv.extend(args(&["-o", "WarnWeakCrypto=no"]));
    }
    argv.extend(args(&["-T", "-o", "BatchMode=yes"]));
    argv.extend(args(extra));
    argv.push(node.alias.clone());
    argv
}

fn powershell(script: &str) -> String {
    format!("powershell -NoProfile -NonInteractive -EncodedCommand {}", util::ps_encode(script))
}

fn parse_answer(text: &str, stderr: &str, clipboard: bool) -> Res<Value> {
    let result: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) if clipboard => return Err("Clipboard helper did not return a valid response.".into()),
        Err(_) => {
            return Err(if stderr.is_empty() { util::cut(text, 250) } else { stderr.to_string() });
        }
    };
    if result["ok"] != true {
        let error = js(&result, "error").map(str::to_string).unwrap_or_else(|| stderr.to_string());
        return Err(if error.is_empty() { "The helper failed.".into() } else { error });
    }
    Ok(result["data"].clone())
}

/// One helper action on a machine.
pub fn call(node: &Node, action: &str, value: Value) -> Res<Value> {
    let request = json!({"action": action, "value": value});
    if !node.linux() {
        return call_windows(node, action, &request);
    }
    #[cfg(unix)]
    if node.is_local() {
        return crate::helper::dispatch(&request);
    }
    let helper = node.helper.as_deref().unwrap_or(DEFAULT_HELPER);
    let remote = if helper.ends_with(".py") { format!("python3 {helper} --stdin") } else { format!("{helper} helper --stdin") };
    let mut argv = ssh(node, &[]);
    argv.push(remote);
    let timeout = if action == "updates" { 100.0 } else { 15.0 };
    let out = util::run(&argv, Some(request.to_string().as_bytes()), secs(timeout), None)?;
    if out.timed_out {
        return Err(format!("{} did not answer within {timeout:.0} s.", node.hostname));
    }
    let (text, error) = (out.text(), out.err());
    if text.is_empty() {
        if out.code == Some(127) || error.to_lowercase().contains("no such file or directory") {
            return Err(format!("Archfleet isn't installed on {} yet (run install.sh there).", node.hostname));
        }
        return Err(if error.is_empty() { "No response from host.".into() } else { error });
    }
    parse_answer(&text, &error, action.starts_with("clipboard-"))
}

fn call_windows(node: &Node, action: &str, request: &Value) -> Res<Value> {
    match action {
        "clipboard-get" | "clipboard-set" | "clipboard-history" | "clipboard-entry" => {
            let mut argv = ssh(node, &[]);
            argv.push(format!("powershell -NoProfile -NonInteractive -File \"{}\"", node.clipboard_client()));
            let out = util::run(&argv, Some(request.to_string().as_bytes()), secs(18.0), None)?;
            let result: Value = serde_json::from_str(&out.text()).map_err(|_| "Windows clipboard helper did not return a valid response.")?;
            if result["ok"] != true {
                return Err(js(&result, "error").unwrap_or("Windows clipboard helper is unavailable.").into());
            }
            Ok(result["data"].clone())
        }
        "processes" => {
            // The script is too long for a Windows command line, so it travels on stdin.
            let mut argv = ssh(node, &[]);
            argv.push(powershell("& ([ScriptBlock]::Create([Console]::In.ReadToEnd()))"));
            let out = util::run(&argv, Some(WINDOWS_PROCESSES.as_bytes()), secs(25.0), None)?;
            let text = out.text();
            if !out.ok() || !text.starts_with('{') {
                return Err("Could not read Windows processes.".into());
            }
            serde_json::from_str(&text).map_err(|e| e.to_string())
        }
        "vpn-restart" | "vpn-test" => {
            let key = &action[4..];
            let script = node.vpn_str(key).ok_or_else(|| format!("No \"{key}\" command for this machine's VPN in hosts.json."))?;
            // every stream (Write-Host included) as plain text, not CLIXML
            let script = format!("$ProgressPreference = 'SilentlyContinue'\n& {{\n{script}\n}} *>&1 | Out-String -Width 160");
            let mut argv = ssh(node, &[]);
            argv.push(powershell(&script));
            let out = util::run(&argv, None, secs(180.0), None)?;
            let mut text = out.text();
            let error = out.err();
            if !error.is_empty() {
                text = format!("{text}\n{error}");
            }
            Ok(json!({"code": out.code.unwrap_or(-1), "text": text.trim()}))
        }
        "status" => {
            let script = format!("$vpnFile = {}\n{WINDOWS_STATUS}", util::ps_quote(node.vpn_str("status_file").unwrap_or("")));
            let mut argv = ssh(node, &[]);
            argv.push(powershell(&script));
            let out = util::run(&argv, None, secs(15.0), None)?;
            if out.timed_out {
                return Err(format!("{} did not answer within 15 s.", node.hostname));
            }
            if !out.ok() {
                let error = out.err();
                return Err(if !error.is_empty() {
                    error
                } else if !out.text().is_empty() {
                    out.text()
                } else {
                    "Could not read Windows status.".into()
                });
            }
            let mut data: Value = serde_json::from_str(&out.text()).map_err(|e| e.to_string())?;
            data["vpn"] = windows_vpn(&data["vpn"]);
            Ok(data)
        }
        _ => Err("Select an Arch machine for these controls.".into()),
    }
}

pub fn clipboard(node: &Node) -> Res<Value> {
    call(node, "clipboard-get", Value::Null)
}

pub fn set_clipboard(node: &Node, payload: &Value) -> Res<Value> {
    let (normalized, _) = crate::clipboards::payload_bytes(payload)?;
    call(node, "clipboard-set", normalized)
}

/// The status with `online` and `elapsed_ms`, or {online: false, error}.
pub fn status(node: &Node) -> Value {
    let started = Instant::now();
    let value = match node.vpn_str("state_file") {
        Some(file) if node.linux() => json!({"vpn_state": file}),
        _ => Value::Null,
    };
    match call(node, "status", value) {
        Ok(mut data) => {
            data["online"] = json!(true);
            data["elapsed_ms"] = json!(started.elapsed().as_millis() as u64);
            data
        }
        Err(error) => json!({"online": false, "error": error}),
    }
}

fn stamp(value: &Value) -> Option<f64> {
    util::parse_time(value.as_str()?).map(|t| t.timestamp_millis() as f64 / 1000.0)
}

/// A Windows VPN watchdog's status file -> the shape the Linux helper reports.
pub fn windows_vpn(raw: &Value) -> Value {
    if !raw.is_object() {
        return Value::Null;
    }
    let exit = raw.get("exit").filter(|e| e.is_object()).cloned().unwrap_or(json!({}));
    let mut problems: Vec<String> = util::ja(raw, "problems").iter().filter(|p| util::truthy(p)).map(util::jtext).collect();
    if let Some(at) = stamp(&raw["at"])
        && util::now_secs() - at > 300.0
    {
        problems.insert(0, "the watchdog has not updated its status for over 5 minutes".into());
    }
    let exit_ok = jb(&exit, "ok");
    if exit_ok && exit.get("provider") == Some(&Value::Bool(false)) {
        problems.push(format!("exit {} is not the VPN provider's", util::jtext(&exit["ip"])));
    }
    json!({"kind": "tunnel", "state": if raw["tunnel"] == "up" { "connected" } else { "down" },
           "relay": raw.get("server"), "city": if exit_ok { exit["city"].clone() } else { Value::Null },
           "exit_ip": if exit_ok { exit["ip"].clone() } else { Value::Null }, "lockdown": null,
           "last_drop": stamp(&raw["lastDropAt"]), "last_leak": stamp(&raw["lastLeakAt"]),
           "ok": jb(raw, "ok") && problems.is_empty(), "problems": problems})
}

pub fn transfer(node: &Node, local: &str, remote: &str, download: bool) -> Res<String> {
    if local.trim().is_empty() || remote.trim().is_empty() {
        return Err("Enter both the local and remote path.".into());
    }
    if local.chars().chain(remote.chars()).any(|c| (c as u32) < 32) {
        return Err("Paths cannot contain control characters.".into());
    }
    let path = util::expand(local.trim());
    let path = std::path::absolute(&path).unwrap_or(path);
    if !download && !path.is_file() {
        return Err("Choose an existing local file to upload.".into());
    }
    if download && path.is_dir() {
        return Err("Use a local filename for the download destination.".into());
    }
    let local_path = path.to_string_lossy().to_string();
    let remote_spec = format!("{}:{}", node.alias, remote);
    let (source, target) = if download { (remote_spec, local_path) } else { (local_path, remote_spec) };
    let argv = vec![tool("scp"), "-o".into(), "BatchMode=yes".into(), "-p".into(), "--".into(), source.clone(), target.clone()];
    let out = util::run(&argv, None, secs(3600.0), None)?;
    if !out.ok() {
        let error = out.err();
        return Err(if !error.is_empty() {
            error
        } else if !out.text().is_empty() {
            out.text()
        } else {
            "Transfer failed.".into()
        });
    }
    Ok(format!("Copied {source} → {target}"))
}

pub fn magic_packet(mac: Option<&str>) -> Res<Vec<u8>> {
    let digits: String = mac.unwrap_or("").chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if digits.len() != 12 {
        return Err("This machine has no valid MAC address in hosts.json.".into());
    }
    let bytes: Vec<u8> = (0..6).map(|i| u8::from_str_radix(&digits[i * 2..i * 2 + 2], 16).unwrap()).collect();
    let mut packet = vec![0xff; 6];
    for _ in 0..16 {
        packet.extend_from_slice(&bytes);
    }
    Ok(packet)
}

/// Send a Wake-on-LAN magic packet to the LAN broadcast addresses.
pub fn wake(node: &Node) -> Res<String> {
    let packet = magic_packet(node.mac.as_deref())?;
    let parts: Vec<&str> = node.address.split('.').collect();
    let mut targets = vec!["255.255.255.255".to_string()];
    if parts.len() == 4 {
        targets.insert(0, format!("{}.{}.{}.255", parts[0], parts[1], parts[2]));
    }
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").map_err(|e| e.to_string())?;
    socket.set_broadcast(true).map_err(|e| e.to_string())?;
    for target in &targets {
        for port in [9, 7] {
            let _ = socket.send_to(&packet, (target.as_str(), port));
        }
    }
    Ok(format!("Wake-on-LAN packet sent to {} ({}).", node.hostname, node.mac.as_deref().unwrap_or("")))
}

/// Median time to open a TCP connection to the machine's SSH port, in milliseconds.
pub fn latency(node: &Node, attempts: usize) -> Res<f64> {
    use std::net::ToSocketAddrs;
    let host = if node.address.is_empty() { &node.hostname } else { &node.address };
    let addr = (host.as_str(), 22).to_socket_addrs().ok().and_then(|mut a| a.next()).ok_or_else(|| format!("{} does not answer on port 22.", node.hostname))?;
    let mut samples = Vec::new();
    for _ in 0..attempts {
        let started = Instant::now();
        if std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(3)).is_ok() {
            samples.push(started.elapsed().as_secs_f64() * 1000.0);
        }
    }
    if samples.is_empty() {
        return Err(format!("{} does not answer on port 22.", node.hostname));
    }
    samples.sort_by(f64::total_cmp);
    Ok(samples[samples.len() / 2])
}

/// Push and pull SPEED_BYTES through SSH: the encrypted link as Archfleet uses it.
/// `progress(phase, results)` runs before each phase with what's measured so far.
pub fn speed_test(node: &Node, progress: &dyn Fn(&str, &Value)) -> Res<Value> {
    use std::io::Read;
    let size = SPEED_BYTES;
    let linux = node.linux();
    let (sink, payload, source, source_login) = if linux {
        ("cat > /dev/null".to_string(), vec![0u8; size], format!("head -c {size} /dev/zero"), "exit 0")
    } else {
        // PowerShell never sees EOF on stdin under Windows OpenSSH, so a native program drains the
        // upload: findstr reads 1 KiB lines that never match and prints nothing.
        let mut line = vec![b'0'; 1023];
        line.push(b'\n');
        let payload = line.repeat(size / 1024);
        let source = format!(
            "powershell -NoProfile -NonInteractive -Command \"$o=[Console]::OpenStandardOutput(); $b=New-Object byte[] 1MB; for($i=0;$i -lt {};$i++){{$o.Write($b,0,$b.Length)}}; $o.Flush()\"",
            size / (1024 * 1024)
        );
        ("findstr x".to_string(), payload, source, "powershell -NoProfile -NonInteractive -Command exit")
    };
    let mut results = json!({"bytes": size});
    progress("latency", &results);
    results["latency_ms"] = json!(latency(node, 5)?);

    // Time an SSH login running the same kind of command, to take it out of the transfer times.
    let login_time = |command: &str| -> Res<f64> {
        let mut argv = ssh(node, &[]);
        argv.push(command.into());
        let started = Instant::now();
        let out = util::run(&argv, None, secs(20.0), None)?;
        if !out.ok() {
            let error = out.err();
            return Err(if error.is_empty() { "SSH login failed.".into() } else { error });
        }
        Ok(started.elapsed().as_secs_f64())
    };
    let rate = |seconds: f64, login: f64| size as f64 / (seconds - login).max(0.05);

    let handshake = login_time("exit 0")?;
    results["handshake_ms"] = json!(handshake * 1000.0);
    progress("upload", &results);
    let mut argv = ssh(node, &[]);
    argv.push(sink);
    let started = Instant::now();
    let out = util::run(&argv, Some(&payload), secs(180.0), None)?;
    let accepted = out.code == Some(0) || (!linux && out.code == Some(1)); // findstr exits 1 when nothing matched
    if !accepted {
        let error = out.err();
        return Err(if error.is_empty() { "Upload test failed.".into() } else { error });
    }
    results["upload"] = json!(rate(started.elapsed().as_secs_f64(), handshake));

    let source_handshake = if source_login == "exit 0" { handshake } else { login_time(source_login)? };
    progress("download", &results);
    let mut argv = ssh(node, &[]);
    argv.push(source);
    let mut child = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| util::spawn_error(&argv[0], e))?;
    let mut stdout = child.stdout.take().unwrap();
    let started = Instant::now();
    let mut received = 0usize;
    let mut buf = vec![0u8; 1 << 20];
    while started.elapsed() < Duration::from_secs(60) {
        match stdout.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => received += n,
        }
    }
    let elapsed = started.elapsed().as_secs_f64();
    let _ = child.kill();
    let _ = child.wait();
    if received < size {
        return Err(format!("Download test received {received} of {size} bytes."));
    }
    results["download"] = json!(rate(elapsed, source_handshake));
    Ok(results)
}

fn http_get(url: &str, headers: &[(&str, &str)], timeout: f64) -> Res<(u16, String)> {
    let mut request = ureq::get(url).header("User-Agent", "archfleet");
    for (k, v) in headers {
        request = request.header(*k, *v);
    }
    let mut response = request.config().timeout_global(Some(secs(timeout))).http_status_as_error(false).build().call().map_err(|e| e.to_string())?;
    let status = response.status().as_u16();
    let body = response.body_mut().read_to_string().map_err(|e| e.to_string())?;
    Ok((status, body))
}

/// What streams and encodes cost the server right now: Lilin's House → Sharing report.
pub fn lilin_load() -> Res<Value> {
    let (url, token) =
        crate::config::lilin_config().ok_or("Lilin isn’t set up on this machine (no ~/.config/lilin-house.env: run lilin-house setup).")?;
    let (status, body) =
        http_get(&format!("{url}/api/house/load"), &[("X-Lilin-Token", &token)], 6.0).map_err(|_| format!("Lilin isn’t answering at {url}."))?;
    match status {
        200..=299 => serde_json::from_str(&body).map_err(|e| e.to_string()),
        403 => Err("Lilin refused the House token (only accepted straight from the home network).".into()),
        code => Err(format!("Lilin answered {code}.")),
    }
}

/// Today's Screen Time on a Linux machine: `screentime today --json`.
pub fn screentime(node: &Node) -> Res<Value> {
    if !node.linux() {
        return Err("Screen Time runs on the Arch machines.".into());
    }
    let command = node.screentime_command.as_deref().unwrap_or(SCREENTIME_COMMAND);
    let argv = if node.is_local() {
        args(&["/bin/sh", "-c", command])
    } else {
        let mut argv = ssh(node, &["-o", "ConnectTimeout=5"]);
        argv.push(command.into());
        argv
    };
    let out = util::run(&argv, None, secs(20.0), None)?;
    let error = out.err();
    if out.code == Some(127) || error.contains("not found") {
        return Err(format!("screentime isn’t installed on {} (github.com/Tjabooo/screentime).", node.hostname));
    }
    let data: Value = match serde_json::from_slice(&out.stdout) {
        Ok(v) => v,
        Err(_) => {
            let text = if !error.is_empty() {
                error
            } else if !out.text().is_empty() {
                out.text()
            } else {
                "screentime gave no answer.".into()
            };
            return Err(util::cut(text.lines().last().unwrap_or(""), 200));
        }
    };
    if !data.is_object() || data.get("total").is_none() {
        return Err("screentime answered something unexpected.".into());
    }
    Ok(data)
}

pub struct RunResult {
    pub code: Option<i32>,
    pub output: String,
    pub seconds: f64,
}

/// One shell command on a machine: the login shell on Linux, cmd.exe on Windows.
pub fn run_command(node: &Node, command: &str, timeout: f64) -> RunResult {
    let started = Instant::now();
    let argv = if node.is_local() {
        if cfg!(windows) {
            args(&["cmd", "/C", command])
        } else {
            vec![std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into()), "-c".into(), command.into()]
        }
    } else {
        let mut argv = ssh(node, &["-o", "ConnectTimeout=5"]);
        argv.push(command.into());
        argv
    };
    match util::run(&argv, None, secs(timeout), None) {
        Ok(out) if out.timed_out => RunResult { code: None, output: format!("Stopped after {timeout:.0} s."), seconds: started.elapsed().as_secs_f64() },
        Ok(out) => {
            let text = [out.text(), out.err()].into_iter().filter(|t| !t.is_empty()).collect::<Vec<_>>().join("\n").replace("\r\n", "\n");
            RunResult { code: out.code, output: text, seconds: started.elapsed().as_secs_f64() }
        }
        Err(error) => RunResult { code: Some(-1), output: error, seconds: started.elapsed().as_secs_f64() },
    }
}

pub struct NewsItem {
    pub title: String,
    pub link: String,
    pub date: chrono::DateTime<chrono::FixedOffset>,
}

fn unescape(text: &str) -> String {
    let text = text.trim();
    let text = text.strip_prefix("<![CDATA[").and_then(|t| t.strip_suffix("]]>")).unwrap_or(text);
    text.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&#39;", "'").replace("&apos;", "'").replace("&amp;", "&")
}

fn tag<'a>(item: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let start = item.find(&open)? + open.len();
    let end = item[start..].find(&format!("</{name}>"))? + start;
    Some(&item[start..end])
}

pub fn parse_news(xml: &str, limit: usize) -> Vec<NewsItem> {
    let mut items: Vec<NewsItem> = xml
        .split("<item>")
        .skip(1)
        .filter_map(|chunk| {
            let item = chunk.split("</item>").next()?;
            let date = chrono::DateTime::parse_from_rfc2822(tag(item, "pubDate")?.trim()).ok()?;
            Some(NewsItem { title: unescape(tag(item, "title").unwrap_or("")), link: unescape(tag(item, "link").unwrap_or("")), date })
        })
        .collect();
    items.sort_by_key(|item| std::cmp::Reverse(item.date));
    items.truncate(limit);
    items
}

pub fn arch_news() -> Res<Vec<NewsItem>> {
    let (status, body) = http_get(NEWS_FEED, &[], 15.0)?;
    if !(200..300).contains(&status) {
        return Err(format!("archlinux.org answered {status}."));
    }
    Ok(parse_news(&body, 8))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magic() {
        let p = magic_packet(Some("00:11:22:33:44:55")).unwrap();
        assert_eq!(p.len(), 102);
        assert_eq!(&p[6..12], &[0, 0x11, 0x22, 0x33, 0x44, 0x55]);
        assert!(magic_packet(Some("zz")).is_err());
    }

    #[test]
    fn news() {
        let xml = r#"<rss><channel><item><title>Old &amp; gold</title><link>https://a/1</link><pubDate>Mon, 01 Jan 2024 10:00:00 +0000</pubDate></item>
            <item><title><![CDATA[New]]></title><link>https://a/2</link><pubDate>Tue, 02 Jan 2024 10:00:00 +0000</pubDate></item></channel></rss>"#;
        let items = parse_news(xml, 8);
        assert_eq!(items[0].title, "New");
        assert_eq!(items[1].title, "Old & gold");
    }

    #[test]
    fn windows_vpn_shape() {
        let v = windows_vpn(&json!({"tunnel": "up", "server": "x", "exit": {"ok": true, "ip": "1.2.3.4", "city": "Taipei", "provider": false},
                                     "ok": true, "at": chrono::Local::now().to_rfc3339()}));
        assert_eq!(v["state"], "connected");
        assert_eq!(v["ok"], false);
        assert!(v["problems"][0].as_str().unwrap().contains("not the VPN provider"));
        assert!(windows_vpn(&Value::Null).is_null());
    }

    #[test]
    fn answers() {
        assert_eq!(parse_answer(r#"{"ok":true,"data":5}"#, "", false).unwrap(), 5);
        assert_eq!(parse_answer(r#"{"ok":false,"error":"no"}"#, "", false).unwrap_err(), "no");
        assert!(parse_answer("garbage", "", true).unwrap_err().contains("Clipboard"));
    }
}
