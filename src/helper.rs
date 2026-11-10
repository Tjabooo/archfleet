//! The per-machine adapter: `archfleet helper --stdin` reads one JSON request, prints one JSON answer.
//! The dashboard runs it over SSH on the other Linux machines and in-process on this one.
use crate::clipboards;
use crate::procs;
use crate::util::{self, Env, Res, args, secs};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};

pub fn main_stdin() -> i32 {
    use std::io::Read;
    let mut input = String::new();
    let request = std::io::stdin()
        .read_to_string(&mut input)
        .map_err(|e| e.to_string())
        .and_then(|_| serde_json::from_str::<Value>(&input).map_err(|e| format!("bad request: {e}")));
    let answer = request.and_then(|request| dispatch(&request));
    match answer {
        Ok(data) => {
            println!("{}", json!({"ok": true, "data": data}));
            0
        }
        Err(error) => {
            println!("{}", json!({"ok": false, "error": error}));
            1
        }
    }
}

pub fn command(argv: &[&str], env: Option<&Env>, timeout: f64) -> Res<Value> {
    let out = util::run(&args(argv), None, secs(timeout), env)?;
    if out.timed_out {
        return Err(format!("{} did not finish within {timeout:.0} s.", argv[0]));
    }
    Ok(json!({"code": out.code.unwrap_or(-1), "text": out.both()}))
}

pub fn read_int(path: &Path) -> Option<i64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn read(path: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

pub fn cpu_sample() -> (u64, u64) {
    let stat = read("/proc/stat");
    let values: Vec<u64> = stat.lines().next().unwrap_or("").split_whitespace().skip(1).take(8).filter_map(|v| v.parse().ok()).collect();
    if values.len() < 5 {
        return (0, 0);
    }
    (values.iter().sum(), values[3] + values[4])
}

/// Bytes received and sent on physical interfaces (no loopback, bridges or VPN tunnels).
fn net_sample() -> (u64, u64) {
    let (mut rx, mut tx) = (0, 0);
    for line in read("/proc/net/dev").lines().skip(2) {
        let Some((name, fields)) = line.split_once(':') else { continue };
        if !Path::new("/sys/class/net").join(name.trim()).join("device").exists() {
            continue;
        }
        let values: Vec<u64> = fields.split_whitespace().filter_map(|v| v.parse().ok()).collect();
        if values.len() > 8 {
            rx += values[0];
            tx += values[8];
        }
    }
    (rx, tx)
}

pub fn meminfo() -> (u64, u64) {
    let mut total = 0;
    let mut available = None;
    let mut free = 0;
    for line in read("/proc/meminfo").lines() {
        let mut parts = line.split_whitespace();
        let key = parts.next().unwrap_or("");
        let value: u64 = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0) * 1024;
        match key {
            "MemTotal:" => total = value,
            "MemAvailable:" => available = Some(value),
            "MemFree:" => free = value,
            _ => {}
        }
    }
    (total.saturating_sub(available.unwrap_or(free)), total)
}

/// CPU and GPU temperature in °C plus GPU busy %, from hwmon and amdgpu sysfs.
fn sensors(out: &mut Map<String, Value>) {
    let mut cpu_temp = Value::Null;
    let mut gpus: Vec<(i64, Option<i64>, Option<i64>)> = Vec::new();
    let mut entries: Vec<PathBuf> = std::fs::read_dir("/sys/class/hwmon").map(|d| d.flatten().map(|e| e.path()).collect()).unwrap_or_default();
    entries.sort();
    for hwmon in entries {
        let Ok(name) = std::fs::read_to_string(hwmon.join("name")) else { continue };
        match name.trim() {
            "k10temp" | "coretemp" | "zenpower" if cpu_temp.is_null() => {
                if let Some(v) = read_int(&hwmon.join("temp1_input")) {
                    cpu_temp = json!((v as f64 / 100.0).round() / 10.0);
                }
            }
            "amdgpu" => {
                let device = hwmon.join("device");
                gpus.push((
                    read_int(&device.join("mem_info_vram_total")).unwrap_or(0),
                    read_int(&hwmon.join("temp1_input")),
                    read_int(&device.join("gpu_busy_percent")),
                ));
            }
            _ => {}
        }
    }
    out.insert("cpu_temp".into(), cpu_temp);
    // The GPU with the most VRAM is the discrete card on a desktop, the only one on a laptop.
    let gpu = gpus.into_iter().max_by_key(|g| g.0);
    out.insert("gpu_temp".into(), json!(gpu.and_then(|g| g.1).map(|t| (t as f64 / 100.0).round() / 10.0)));
    out.insert("gpu_busy".into(), json!(gpu.and_then(|g| g.2)));
}

/// Timestamp of the newest completed `pacman -Syu`, read from the end of pacman.log.
pub fn last_upgrade() -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open("/var/log/pacman.log").ok()?;
    let end = file.seek(SeekFrom::End(0)).ok()?;
    let mut chunk: u64 = 1 << 20;
    loop {
        let start = end.saturating_sub(chunk);
        file.seek(SeekFrom::Start(start)).ok()?;
        let mut buf = Vec::with_capacity((end - start) as usize);
        file.by_ref().take(end - start).read_to_end(&mut buf).ok()?;
        let text = String::from_utf8_lossy(&buf);
        for line in text.lines().rev() {
            if line.starts_with('[') && line.contains("[PACMAN] starting full system upgrade") {
                return line.find(']').map(|i| line[1..i].to_string());
            }
        }
        if start == 0 || chunk >= 64 << 20 {
            return None;
        }
        chunk *= 4;
    }
}

/// NPU gateway state from npukit (github.com/Tjabooo/npukit): one small file read.
fn npu_status() -> Value {
    let path = util::home().join(".local/state/npukit/status.json");
    let Some(data) = std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) else {
        return Value::Null;
    };
    let Some(obj) = data.as_object() else { return Value::Null };
    let keys = ["state", "activity", "components", "chat_model", "pmode", "power", "last_tps", "updated", "runs_on"];
    Value::Object(keys.iter().map(|k| (k.to_string(), obj.get(*k).cloned().unwrap_or(Value::Null))).collect())
}

/// The Mullvad app's state (None without one). state_file: optional JSON with "last_drop".
pub fn vpn_status(state_file: Option<&str>) -> Value {
    if util::which("mullvad").is_none() {
        return Value::Null;
    }
    let raw = util::run(&args(&["mullvad", "status", "--json"]), None, secs(5.0), None)
        .ok()
        .and_then(|o| o.text().lines().last().and_then(|l| serde_json::from_str::<Value>(l).ok()));
    let Some(raw) = raw else {
        return json!({"kind": "mullvad", "state": "unknown", "problems": ["the Mullvad daemon is not answering"], "ok": false});
    };
    let details = raw.get("details").filter(|d| d.is_object()).cloned().unwrap_or(json!({}));
    let loc = details.get("location").cloned().unwrap_or(json!({}));
    let state = raw.get("state").and_then(Value::as_str).unwrap_or("unknown").to_string();
    let lock = util::run(&args(&["mullvad", "lockdown-mode", "get"]), None, secs(5.0), None).map(|o| o.text()).unwrap_or_default();
    let mut problems: Vec<String> = Vec::new();
    if state == "error" {
        let cause = details.get("cause").map(|c| c.as_str().map(str::to_string).unwrap_or_else(|| c.to_string()));
        problems.push(format!("blocked: {}", cause.unwrap_or_else(|| "null".into())));
    }
    let exit_ip = loc.get("ipv4").filter(|v| util::truthy(v)).or_else(|| loc.get("ipv6")).cloned().unwrap_or(Value::Null);
    if state == "connected" && loc.get("mullvad_exit_ip") == Some(&Value::Bool(false)) {
        problems.push(format!("exit {} is not a Mullvad address", util::jtext(&exit_ip)));
    }
    let last_drop = state_file
        .and_then(|f| std::fs::read_to_string(util::expand(f)).ok())
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.get("last_drop").filter(|d| d.is_number()).cloned())
        .unwrap_or(Value::Null);
    json!({"kind": "mullvad", "state": state, "relay": loc.get("hostname"), "city": loc.get("city"),
           "exit_ip": exit_ip, "lockdown": lockdown_on(&lock), "last_drop": last_drop,
           "ok": problems.is_empty(), "problems": problems})
}

/// `mullvad lockdown-mode get` says "...: on" when it's on.
fn lockdown_on(text: &str) -> bool {
    let lower = text.to_lowercase();
    lower.match_indices(':').any(|(i, _)| {
        let rest = lower[i + 1..].trim_start();
        rest.starts_with("on") && !rest[2..].starts_with(|c: char| c.is_alphanumeric() || c == '_')
    })
}

fn valid_location_word(word: &str) -> bool {
    (2..=24).contains(&word.len()) && word.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn vpn_action(action: &str, value: &Value) -> Res<Value> {
    if util::which("mullvad").is_none() {
        return Err("Mullvad is not installed on this machine.".into());
    }
    match action {
        "connect" | "disconnect" | "reconnect" => command(&["mullvad", action], None, 20.0),
        "relay" => {
            let text = util::jtext(value).to_lowercase();
            let words: Vec<&str> = text.split_whitespace().collect();
            if !(1..=3).contains(&words.len()) || !words.iter().all(|w| valid_location_word(w)) {
                return Err("Relay: a country code, \"country city\", or a hostname like se-sto-wg-001.".into());
            }
            let mut argv = vec!["mullvad", "relay", "set", "location"];
            argv.extend(&words);
            let result = command(&argv, None, 20.0)?;
            let state = vpn_status(None).get("state").and_then(Value::as_str).unwrap_or("").to_string();
            if result["code"] == 0 && state != "connected" && state != "connecting" {
                let _ = command(&["mullvad", "connect"], None, 20.0);
            }
            Ok(result)
        }
        _ => Err("Unsupported VPN action.".into()),
    }
}

fn disk_usage(path: &str) -> (u64, u64) {
    let c = std::ffi::CString::new(path).unwrap();
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return (0, 0);
    }
    let total = s.f_blocks as u64 * s.f_frsize as u64;
    let used = (s.f_blocks as u64 - s.f_bfree as u64) * s.f_frsize as u64;
    (used, total)
}

fn batteries() -> Vec<Value> {
    let mut list = Vec::new();
    let Ok(dir) = std::fs::read_dir("/sys/class/power_supply") else { return list };
    let mut folders: Vec<PathBuf> = dir.flatten().map(|e| e.path()).collect();
    folders.sort();
    for folder in folders {
        if std::fs::read_to_string(folder.join("present")).is_ok_and(|p| p.trim() == "0") {
            continue;
        }
        let kind = std::fs::read_to_string(folder.join("type")).unwrap_or_default();
        if kind.trim() == "Battery"
            && let Some(percent) = read_int(&folder.join("capacity"))
        {
            let state = std::fs::read_to_string(folder.join("status")).unwrap_or_default();
            list.push(json!({"percent": percent, "state": state.trim()}));
        }
    }
    list
}

/// CPU and network counters from the previous status read (any caller), so a read doesn't have to
/// sleep to measure rates. Kept in $XDG_RUNTIME_DIR; used when 0.2–60 s old.
struct Sample {
    at: f64,
    cpu: (u64, u64),
    net: (u64, u64),
}

fn sample_path(name: &str) -> PathBuf {
    util::runtime_dir().join(format!("archfleet-{name}.sample"))
}

fn load_sample() -> Option<Sample> {
    let text = std::fs::read_to_string(sample_path("status")).ok()?;
    let v: Vec<f64> = text.split_whitespace().filter_map(|x| x.parse().ok()).collect();
    (v.len() == 5).then(|| Sample { at: v[0], cpu: (v[1] as u64, v[2] as u64), net: (v[3] as u64, v[4] as u64) })
}

fn store_sample(s: &Sample) {
    let text = format!("{} {} {} {} {}", s.at, s.cpu.0, s.cpu.1, s.net.0, s.net.1);
    let _ = util::write_private(&sample_path("status"), text.as_bytes());
}

pub fn status(options: &Value) -> Res<Value> {
    let take = || Sample { at: util::now_secs(), cpu: cpu_sample(), net: net_sample() };
    let mut after = take();
    let before = match load_sample().filter(|s| (0.2..=60.0).contains(&(after.at - s.at))) {
        Some(before) => before,
        None => {
            let before = after;
            std::thread::sleep(secs(0.25));
            after = take();
            before
        }
    };
    store_sample(&after);
    let elapsed = (after.at - before.at).max(0.001);
    let total = after.cpu.0.saturating_sub(before.cpu.0);
    let idle = after.cpu.1.saturating_sub(before.cpu.1);
    let cpu = if total > 0 { ((1.0 - idle as f64 / total as f64) * 1000.0).round() / 10.0 } else { 0.0 };
    let (memory_used, memory_total) = meminfo();
    let (disk_used, disk_total) = disk_usage("/");
    let mut load = [0f64; 3];
    unsafe { libc::getloadavg(load.as_mut_ptr(), 3) };
    let uptime: f64 = read("/proc/uptime").split_whitespace().next().and_then(|v| v.parse().ok()).unwrap_or(0.0);
    let kernel = read("/proc/sys/kernel/osrelease").trim().to_string();
    let mut out = json!({
        "hostname": util::local_hostname(), "kernel": kernel, "cpu": cpu,
        "memory_used": memory_used, "memory_total": memory_total, "disk_used": disk_used, "disk_total": disk_total,
        "uptime": uptime, "load": load, "battery": batteries(),
        "net_rx": after.net.0.saturating_sub(before.net.0) as f64 / elapsed,
        "net_tx": after.net.1.saturating_sub(before.net.1) as f64 / elapsed,
        "last_upgrade": last_upgrade(), "disks": [{"name": "/", "used": disk_used, "total": disk_total}],
        "vpn": vpn_status(options.get("vpn_state").and_then(Value::as_str)),
        "npu": npu_status(),
    });
    sensors(out.as_object_mut().unwrap());
    Ok(out)
}

/// The environment of this user's Hyprland session, for commands run from SSH.
pub fn hypr_environment() -> Res<Env> {
    let runtime = format!("/run/user/{}", util::uid());
    let base: Env = vec![("XDG_RUNTIME_DIR".into(), runtime.clone())];
    let out = util::run(&args(&["hyprctl", "-j", "instances"]), None, secs(5.0), Some(&base))?;
    let instances: Vec<Value> = serde_json::from_slice(&out.stdout).map_err(|_| "No Hyprland session is running for this user.".to_string())?;
    if instances.is_empty() {
        return Err("No Hyprland session is running for this user.".into());
    }
    let signature = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").unwrap_or_default();
    let instance = match instances.iter().find(|i| i["instance"] == signature.as_str()) {
        Some(instance) => instance,
        None if instances.len() == 1 => &instances[0],
        None => return Err("Several Hyprland sessions are running. Set HYPRLAND_INSTANCE_SIGNATURE to choose one.".into()),
    };
    let mut env = base;
    env.push(("HYPRLAND_INSTANCE_SIGNATURE".into(), util::jtext(&instance["instance"])));
    env.push(("WAYLAND_DISPLAY".into(), util::jtext(&instance["wl_socket"])));
    if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none() {
        env.push(("DBUS_SESSION_BUS_ADDRESS".into(), format!("unix:path={runtime}/bus")));
    }
    Ok(env)
}

fn hypr_query(name: &str, env: &Env) -> Res<Value> {
    let out = util::run(&args(&["hyprctl", "-j", name]), None, secs(5.0), Some(env))?;
    if !out.ok() {
        return Err(out.both());
    }
    serde_json::from_slice(&out.stdout).map_err(|e| format!("hyprctl {name}: {e}"))
}

fn hypr_action(action: &str, value: &Value) -> Res<Value> {
    let env = hypr_environment()?;
    let lua = match action {
        "hypr" => {
            let mut out = Map::new();
            for name in ["version", "monitors", "workspaces", "clients", "activeworkspace"] {
                out.insert(name.into(), hypr_query(name, &env)?);
            }
            return Ok(Value::Object(out));
        }
        "reload" => return command(&["hyprctl", "reload"], Some(&env), 15.0),
        // Hyprland 0.57's Lua dispatcher API.
        "workspace" => {
            let number = util::jtext(value);
            let valid = (1..=2).contains(&number.len()) && number.chars().all(|c| c.is_ascii_digit()) && !number.starts_with('0');
            if !valid {
                return Err("Workspace must be a number from 1 to 99.".into());
            }
            format!("hl.dispatch(hl.dsp.focus({{workspace = {}}}))", json!(number))
        }
        "focus" => {
            let address = util::jtext(value);
            let valid = address.len() > 2 && address.starts_with("0x") && address[2..].chars().all(|c| c.is_ascii_hexdigit());
            if !valid {
                return Err("Invalid window address.".into());
            }
            format!("hl.dispatch(hl.dsp.focus({{window = {}}}))", json!(format!("address:{address}")))
        }
        _ => return Err("Unknown Hyprland action.".into()),
    };
    command(&["hyprctl", "eval", &lua], Some(&env), 15.0)
}

/// The logind session that runs this user's Hyprland, so it can be locked from SSH.
fn graphical_session() -> Res<String> {
    let out = command(&["loginctl", "list-sessions", "-j"], None, 15.0)?;
    let sessions: Vec<Value> = serde_json::from_str(out["text"].as_str().unwrap_or("[]")).unwrap_or_default();
    sessions
        .iter()
        .find(|s| s["uid"].as_u64() == Some(util::uid() as u64) && s.get("seat").is_some_and(util::truthy))
        .map(|s| util::jtext(&s["session"]))
        .ok_or_else(|| "No graphical login session found for this user.".to_string())
}

/// Start a program in its own session and don't wait for it (a thread reaps it).
pub fn spawn_detached(argv: &[&str], env: &Env) -> Res<()> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    let mut command = Command::new(argv[0]);
    command.args(&argv[1..]).envs(env.iter().map(|(k, v)| (k, v)));
    command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = command.spawn().map_err(|e| util::spawn_error(argv[0], e))?;
    std::thread::spawn(move || child.wait());
    Ok(())
}

fn session_action(action: &str, value: &Value) -> Res<Value> {
    let env = hypr_environment()?;
    match action {
        "screenshot" => {
            let out = util::run(&args(&["grim", "-t", "jpeg", "-q", "85", "-"]), None, secs(20.0), Some(&env))?;
            if !out.ok() || out.stdout.is_empty() {
                let error = out.err();
                return Err(if error.is_empty() { "grim could not capture the screen.".into() } else { error });
            }
            Ok(json!({"mime": "image/jpeg", "data": util::b64(&out.stdout), "bytes": out.stdout.len()}))
        }
        "lock" => {
            // hypridle answers logind's Lock signal with its lock_cmd (hyprlock).
            let session = graphical_session()?;
            let mut result = command(&["loginctl", "lock-session", &session], None, 15.0)?;
            if result["text"] == "" {
                result["text"] = json!("Screen locked.");
            }
            Ok(result)
        }
        "notify" => {
            let body = value.get("text").map(util::jtext).unwrap_or_default().trim().to_string();
            let sender = util::cut(&value.get("from").map(util::jtext).unwrap_or_else(|| "Archfleet".into()), 40);
            let bad = |c: char| (c as u32) < 32 && c != '\n';
            if body.is_empty() || body.chars().count() > 500 || body.chars().chain(sender.chars()).any(bad) {
                return Err("Write a message of 1–500 characters.".into());
            }
            let title = format!("From {sender}");
            let mut result = command(&["notify-send", "-a", "Archfleet", "-i", "network-workgroup", &title, &body], Some(&env), 15.0)?;
            if result["text"] == "" {
                result["text"] = json!("Notification shown.");
            }
            Ok(result)
        }
        "open-url" => {
            let url = util::jtext(value).trim().to_string();
            let rest = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://"));
            let valid = rest.is_some_and(|r| (1..=2000).contains(&r.len()) && !r.chars().any(|c| c.is_whitespace() || (c as u32) < 32 || c as u32 == 127));
            if !valid {
                return Err("Enter a full http:// or https:// address.".into());
            }
            spawn_detached(&["xdg-open", &url], &env)?;
            Ok(json!({"code": 0, "text": format!("Opened {url}")}))
        }
        _ => Err("Unknown session action.".into()),
    }
}

pub fn dispatch(request: &Value) -> Res<Value> {
    let action = request.get("action").and_then(Value::as_str).unwrap_or("status");
    let value = request.get("value").unwrap_or(&Value::Null);
    match action {
        "clipboard-get" => clipboards::read_wayland(&hypr_environment()?),
        "clipboard-set" => clipboards::write_wayland(value, &hypr_environment()?),
        "clipboard-history" => {
            // value: a limit, or {"limit": n, "text": bool} for the full text of text entries too
            let (limit, text) = match value {
                Value::Object(o) => (o.get("limit").and_then(Value::as_u64), o.get("text") == Some(&Value::Bool(true))),
                v => (v.as_u64(), false),
            };
            let limit = limit.map_or(10, |l| l.clamp(1, 1000)) as usize;
            Ok(Value::Array(clipboards::cliphist_history(limit, text)?))
        }
        "clipboard-entry" => clipboards::cliphist_entry(value),
        "status" => status(value),
        "vpn-connect" | "vpn-disconnect" | "vpn-reconnect" | "vpn-relay" => vpn_action(&action[4..], value),
        "processes" => procs::collect(),
        "process-term" => procs::terminate(value),
        "hypr" | "workspace" | "focus" | "reload" => hypr_action(action, value),
        "screenshot" | "lock" | "notify" | "open-url" => session_action(action, value),
        "packages" => {
            let out = util::run(&args(&["pacman", "-Qqe"]), None, secs(15.0), None)?;
            if !out.ok() {
                let text = out.both();
                return Err(if text.is_empty() { "pacman -Qqe failed.".into() } else { text });
            }
            let packages: Vec<&str> = std::str::from_utf8(&out.stdout).unwrap_or("").split_whitespace().collect();
            Ok(json!({"packages": packages, "last_upgrade": last_upgrade()}))
        }
        "updates" => {
            if util::which("checkupdates").is_some() {
                let result = command(&["checkupdates"], None, 90.0)?;
                if result["code"] == 2 {
                    return Ok(json!({"code": 0, "text": "No repository updates available."}));
                }
                return Ok(result);
            }
            let mut result = command(&["pacman", "-Qu"], None, 15.0)?;
            result["text"] = json!(format!("Cached package database (install pacman-contrib for a fresh check):\n{}", result["text"].as_str().unwrap_or("")));
            Ok(result)
        }
        "services" => command(&["systemctl", "--failed", "--no-pager"], None, 15.0),
        "journal" => command(&["journalctl", "-b", "-p", "err", "-n", "60", "--no-pager"], None, 15.0),
        "storage" => command(&["df", "-h", "-x", "tmpfs", "-x", "devtmpfs"], None, 15.0),
        "sessions" => command(&["loginctl", "list-sessions", "--no-pager"], None, 15.0),
        _ => Err("Unsupported action.".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lockdown_parse() {
        assert!(lockdown_on("Block traffic when the VPN is disconnected: on"));
        assert!(!lockdown_on("Block traffic when the VPN is disconnected: off"));
        assert!(!lockdown_on("x: one"));
    }

    #[test]
    fn rejects_bad_requests() {
        assert!(dispatch(&json!({"action": "nope"})).is_err());
        assert!(dispatch(&json!({"action": "vpn-relay", "value": "se; reboot"})).is_err());
        assert!(session_action("open-url", &json!("javascript:alert(1)")).is_err());
    }

    #[test]
    fn status_reads() {
        let s = status(&Value::Null).unwrap();
        assert!(s["memory_total"].as_u64().unwrap() > 0);
        assert!(s["cpu"].is_number());
    }
}
